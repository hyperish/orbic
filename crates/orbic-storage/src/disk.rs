use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use hyve::{HyveError, HyveResult, Ulid};
use orbic_core::Principal;
use tempfile::{NamedTempFile, TempPath};
use tokio::fs::{self, File};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};

use crate::{Integrity, StoredFile, Upload, Verification, mismatch, part_number};

/// A private directory containing resumable uploads and verified files.
#[derive(Clone, Debug)]
pub struct DiskStorage {
    root: PathBuf,
}

fn io(error: std::io::Error) -> HyveError {
    if error.kind() == ErrorKind::NotFound {
        HyveError::not_found("upload")
    } else {
        HyveError::infra("storage_unavailable", format!("file storage: {error}"))
    }
}

fn temporary(dir: &Path) -> HyveResult<(File, TempPath)> {
    let (file, path) = NamedTempFile::new_in(dir).map_err(io)?.into_parts();
    Ok((File::from_std(file), path))
}

impl DiskStorage {
    /// Opens a server-owned directory, creating it if necessary.
    pub async fn new(root: impl AsRef<Path>) -> HyveResult<Self> {
        fs::create_dir_all(&root).await.map_err(io)?;
        Ok(Self {
            root: fs::canonicalize(&root).await.map_err(io)?,
        })
    }

    /// Begins an upload for the authenticated principal. Keep the ID to resume.
    pub async fn begin(&self, principal: &Principal, expected: Integrity) -> HyveResult<Ulid> {
        let id = Ulid::new();
        let dir = self.root.join(id.to_string());
        fs::create_dir(&dir).await.map_err(io)?;
        let meta = Upload::new(principal, expected, None);
        let initialized = async {
            fs::create_dir(dir.join("parts")).await?;
            fs::write(dir.join("upload.json"), hyve::serde::to_json(&meta)).await
        }
        .await;
        if let Err(error) = initialized {
            let _ = fs::remove_dir_all(dir).await;
            return Err(io(error));
        }
        Ok(id)
    }

    async fn upload(&self, principal: &Principal, id: Ulid) -> HyveResult<(PathBuf, Upload)> {
        let dir = self.root.join(id.to_string());
        let data = fs::read_to_string(dir.join("upload.json"))
            .await
            .map_err(io)?;
        let meta: Upload = hyve::serde::from_json(&data)
            .map_err(|_| HyveError::internal("invalid upload metadata"))?;
        Ok((dir, meta.authorize(principal)?))
    }

    /// Replaces one numbered part atomically. A cancelled write leaves its
    /// previous complete version available for a retry.
    pub async fn put_part(
        &self,
        principal: &Principal,
        id: Ulid,
        number: u32,
        mut input: impl AsyncRead + Unpin,
    ) -> HyveResult<()> {
        part_number(number)?;
        let (dir, meta) = self.upload(principal, id).await?;
        if fs::try_exists(dir.join("file")).await.map_err(io)? {
            return Err(HyveError::domain(
                "upload_complete",
                "file already published",
            ));
        }
        let parts = dir.join("parts");
        let (mut file, temp) = temporary(&parts)?;
        let mut limited = (&mut input).take(meta.expected.bytes.saturating_add(1));
        let bytes = tokio::io::copy(&mut limited, &mut file).await.map_err(io)?;
        if bytes > meta.expected.bytes {
            return Err(mismatch());
        }
        file.sync_all().await.map_err(io)?;
        drop(file);
        temp.persist(parts.join(number.to_string()))
            .map_err(|e| io(e.error))
    }

    /// Joins parts 1 through `count`, verifies the complete file, and makes
    /// it readable in one rename. A completed call may be repeated.
    pub async fn complete(
        &self,
        principal: &Principal,
        id: Ulid,
        count: u32,
    ) -> HyveResult<StoredFile> {
        part_number(count)?;
        let (dir, meta) = self.upload(principal, id).await?;
        let published = dir.join("file");
        let stored = StoredFile {
            id,
            integrity: meta.expected,
        };
        if fs::try_exists(&published).await.map_err(io)? {
            return Ok(stored);
        }
        let parts = dir.join("parts");
        let built = async {
            let (mut file, temp) = temporary(&dir)?;
            let mut verification = Verification::default();
            let mut buf = vec![0; 64 * 1024];
            for n in 1..=count {
                let mut part = File::open(parts.join(n.to_string())).await.map_err(|e| {
                    if e.kind() == ErrorKind::NotFound {
                        HyveError::domain("missing_part", format!("missing upload part {n}"))
                    } else {
                        io(e)
                    }
                })?;
                loop {
                    let n = part.read(&mut buf).await.map_err(io)?;
                    if n == 0 {
                        break;
                    }
                    verification.chunk(&buf[..n], meta.expected.bytes)?;
                    file.write_all(&buf[..n]).await.map_err(io)?;
                }
            }
            verification.finish(meta.expected)?;
            file.sync_all().await.map_err(io)?;
            drop(file);
            temp.persist_noclobber(&published).map_err(|e| io(e.error))
        }
        .await;
        // A concurrent completion can commit and remove its parts first.
        if let Err(error) = built
            && !fs::try_exists(&published).await.map_err(io)?
        {
            return Err(error);
        }
        let _ = fs::remove_dir_all(parts).await;
        Ok(stored)
    }

    /// Opens verified bytes for the file owner. A staging ID alone grants no access.
    pub async fn open(&self, principal: &Principal, id: Ulid) -> HyveResult<File> {
        let (dir, _) = self.upload(principal, id).await?;
        File::open(dir.join("file")).await.map_err(io)
    }
}
