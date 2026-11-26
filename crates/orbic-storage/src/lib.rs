//! App-owned file storage. Pass the principal established by an Orbic guard to
//! every operation. Uploads stay private until their declared length and
//! SHA-256 digest have both been checked.

mod disk;
#[cfg(feature = "s3")]
mod object;

pub use disk::DiskStorage;
use hyve::{HyveError, HyveResult, Ulid, model};
#[cfg(feature = "s3")]
pub use object::{CompletedPart, ObjectStorage};
use orbic_core::Principal;
use sha2::{Digest, Sha256};

/// The expected length and SHA-256 of the complete file.
#[model]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Integrity {
    /// Number of bytes after joining all parts.
    pub bytes: u64,
    /// SHA-256 in binary form.
    pub sha256: [u8; 32],
}

/// A file that has passed the complete upload check.
#[model]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StoredFile {
    /// Server-generated identifier shared with its upload.
    pub id: Ulid,
    /// Verified file properties.
    pub integrity: Integrity,
}

#[model]
struct Upload {
    owner: String,
    expected: Integrity,
    multipart: Option<String>,
}

impl Upload {
    fn new(principal: &Principal, expected: Integrity, multipart: Option<String>) -> Self {
        Self {
            owner: principal.as_str().to_owned(),
            expected,
            multipart,
        }
    }

    fn authorize(self, principal: &Principal) -> HyveResult<Self> {
        if self.owner == principal.as_str() {
            Ok(self)
        } else {
            Err(HyveError::not_found("upload"))
        }
    }
}

// S3 uses numbered parts starting at 1. Disk uses the same numbering.
fn part_number(number: u32) -> HyveResult<()> {
    if (1..=10_000).contains(&number) {
        Ok(())
    } else {
        Err(HyveError::domain(
            "invalid_part",
            "part number must be 1 through 10000",
        ))
    }
}

fn mismatch() -> HyveError {
    HyveError::domain(
        "file_integrity_mismatch",
        "file size or SHA-256 differs from the declaration",
    )
}

#[derive(Default)]
struct Verification {
    bytes: u64,
    hash: Sha256,
}

impl Verification {
    fn chunk(&mut self, chunk: &[u8], limit: u64) -> HyveResult<()> {
        self.bytes = self
            .bytes
            .checked_add(chunk.len() as u64)
            .ok_or_else(mismatch)?;
        if self.bytes > limit {
            return Err(mismatch());
        }
        self.hash.update(chunk);
        Ok(())
    }

    fn finish(self, expected: Integrity) -> HyveResult<()> {
        let digest: [u8; 32] = self.hash.finalize().into();
        if self.bytes == expected.bytes && digest == expected.sha256 {
            Ok(())
        } else {
            Err(mismatch())
        }
    }
}
