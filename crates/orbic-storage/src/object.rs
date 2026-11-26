use std::collections::HashMap;

use futures_util::StreamExt;
use hyve::{HyveError, HyveResult, Ulid};
use orbic_core::Principal;
use s3::creds::Credentials;
use s3::error::S3Error;
use s3::serde_types::Part;
use s3::{Bucket, Region};

use crate::{Integrity, StoredFile, Upload, Verification, part_number};

/// A private, path-style S3-compatible bucket chosen by the application.
/// The bucket must already exist. Keep its objects private and issue download
/// URLs through the verified-file API.
#[derive(Clone)]
pub struct ObjectStorage {
    bucket: Box<Bucket>,
}

/// ETag returned to the uploader by a successful part PUT.
pub struct CompletedPart {
    /// Part number used in its upload URL.
    pub number: u32,
    /// Opaque ETag from the endpoint, including surrounding quotes if present.
    pub etag: String,
}

fn remote(error: S3Error) -> HyveError {
    // Error bodies and request URLs from a storage provider may contain credentials.
    match error {
        S3Error::HttpFailWithBody(404, _) => HyveError::not_found("upload"),
        _ => HyveError::infra("storage_unavailable", "object storage request failed"),
    }
}

fn object(id: Ulid) -> String {
    format!("orbic/objects/{id}")
}
fn descriptor(id: Ulid) -> String {
    format!("orbic/uploads/{id}")
}
fn receipt(id: Ulid) -> String {
    format!("orbic/verified/{id}")
}

impl ObjectStorage {
    /// Constructs a client for the supplied endpoint and signing credentials.
    pub fn new(
        endpoint: &str,
        region: &str,
        bucket: &str,
        access_key: &str,
        secret: &str,
        session_token: Option<&str>,
    ) -> HyveResult<Self> {
        let credentials =
            Credentials::new(Some(access_key), Some(secret), None, session_token, None)
                .map_err(|_| HyveError::internal("invalid object storage credentials"))?;
        let region = Region::Custom {
            region: region.to_owned(),
            endpoint: endpoint.to_owned(),
        };
        Ok(Self {
            bucket: Bucket::new(bucket, region, credentials)
                .map_err(remote)?
                .with_path_style(),
        })
    }

    /// Creates a multipart upload and records its immutable expectations in the bucket.
    pub async fn begin(&self, principal: &Principal, expected: Integrity) -> HyveResult<Ulid> {
        let id = Ulid::new();
        let key = object(id);
        let initiated = self
            .bucket
            .initiate_multipart_upload(&key, "application/octet-stream")
            .await
            .map_err(remote)?;
        let meta = Upload::new(principal, expected, Some(initiated.upload_id.clone()));
        if let Err(err) = self
            .bucket
            .put_object(descriptor(id), hyve::serde::to_json(&meta).as_bytes())
            .await
        {
            let _ = self.bucket.abort_upload(&key, &initiated.upload_id).await;
            return Err(remote(err));
        }
        Ok(id)
    }

    async fn upload(&self, principal: &Principal, id: Ulid) -> HyveResult<Upload> {
        let data = self
            .bucket
            .get_object(descriptor(id))
            .await
            .map_err(remote)?;
        let meta: Upload = hyve::serde::json::from_json_bytes(data.as_slice(), Default::default())
            .map_err(|_| HyveError::internal("invalid upload metadata"))?;
        meta.authorize(principal)
    }

    async fn exists(&self, key: &str) -> HyveResult<bool> {
        match self.bucket.head_object(key).await {
            Ok((_, 200)) => Ok(true),
            Ok((_, 404)) | Err(S3Error::HttpFailWithBody(404, _)) => Ok(false),
            Ok(_) => Err(HyveError::infra(
                "storage_unavailable",
                "could not check stored object",
            )),
            Err(error) => Err(remote(error)),
        }
    }

    /// Signs one part for 15 minutes. Reissuing the URL resumes the same upload.
    pub async fn part_url(
        &self,
        principal: &Principal,
        id: Ulid,
        number: u32,
    ) -> HyveResult<String> {
        part_number(number)?;
        let meta = self.upload(principal, id).await?;
        if self.exists(&object(id)).await? {
            return Err(HyveError::domain(
                "upload_complete",
                "multipart upload already closed",
            ));
        }
        let upload = meta
            .multipart
            .ok_or_else(|| HyveError::internal("missing multipart identifier"))?;
        let query = HashMap::from([
            ("uploadId".to_owned(), upload),
            ("partNumber".to_owned(), number.to_string()),
        ]);
        self.bucket
            .presign_put(object(id), 900, None, Some(query))
            .await
            .map_err(remote)
    }

    /// Closes the multipart upload, streams its bytes through SHA-256, then
    /// persists a receipt. A restart can resume after either remote write.
    pub async fn complete(
        &self,
        principal: &Principal,
        id: Ulid,
        parts: &[CompletedPart],
    ) -> HyveResult<StoredFile> {
        let meta = self.upload(principal, id).await?;
        let file = StoredFile {
            id,
            integrity: meta.expected,
        };
        if self.exists(&receipt(id)).await? {
            return Ok(file);
        }
        let key = object(id);
        if !self.exists(&key).await? {
            let upload = meta
                .multipart
                .as_deref()
                .ok_or_else(|| HyveError::internal("missing multipart identifier"))?;
            let parts = ordered_parts(parts)?;
            if let Err(error) = self
                .bucket
                .complete_multipart_upload(&key, upload, parts)
                .await
                && !self.exists(&key).await?
            {
                return Err(remote(error));
            }
        }
        let mut response = self.bucket.get_object_stream(&key).await.map_err(remote)?;
        if response.status_code != 200 {
            return Err(HyveError::infra(
                "storage_unavailable",
                "could not verify stored object",
            ));
        }
        let mut verification = Verification::default();
        while let Some(chunk) = response.bytes.next().await {
            verification.chunk(&chunk.map_err(remote)?, meta.expected.bytes)?;
        }
        verification.finish(meta.expected)?;
        self.bucket
            .put_object(receipt(id), hyve::serde::to_json(&file).as_bytes())
            .await
            .map_err(remote)?;
        Ok(file)
    }

    /// Signs a five-minute GET for a verified file after checking its owner.
    pub async fn download_url(&self, principal: &Principal, id: Ulid) -> HyveResult<String> {
        self.upload(principal, id).await?;
        if !self.exists(&receipt(id)).await? {
            return Err(HyveError::not_found("file"));
        }
        self.bucket
            .presign_get(object(id), 300, None)
            .await
            .map_err(remote)
    }
}

fn ordered_parts(input: &[CompletedPart]) -> HyveResult<Vec<Part>> {
    if input.is_empty() || input.len() > 10_000 {
        return Err(HyveError::domain(
            "invalid_part",
            "expected 1 through 10000 parts",
        ));
    }
    let mut parts = Vec::with_capacity(input.len());
    for (index, part) in input.iter().enumerate() {
        // rust-s3 places ETags into XML. Escape the opaque value as element text.
        if part.number != index as u32 + 1
            || part.etag.is_empty()
            || part.etag.len() > 1024
            || part.etag.chars().any(char::is_control)
        {
            return Err(HyveError::domain(
                "invalid_part",
                "expected consecutive numbered ETags",
            ));
        }
        parts.push(Part {
            part_number: part.number,
            etag: part
                .etag
                .replace('&', "&amp;")
                .replace('<', "&lt;")
                .replace('>', "&gt;"),
        });
    }
    Ok(parts)
}
