//! Multipart retries and verification on the disk backend.

use orbic_core::Principal;
use orbic_storage::{DiskStorage, Integrity};
use sha2::{Digest, Sha256};
use tokio::io::AsyncReadExt;

#[tokio::test]
async fn resumes_after_restart_and_only_publishes_verified_bytes() {
    let root = tempfile::tempdir().unwrap();
    let disk = DiskStorage::new(root.path()).await.unwrap();
    let owner = Principal::new("a");
    let other = Principal::new("b");
    let expected = Integrity {
        bytes: 7,
        sha256: Sha256::digest(b"abcdefg").into(),
    };
    let id = disk.begin(&owner, expected).await.unwrap();
    assert!(disk.open(&owner, id).await.is_err());
    assert!(disk.put_part(&other, id, 1, &b"abc"[..]).await.is_err());
    disk.put_part(&owner, id, 1, &b"bad"[..]).await.unwrap();
    assert_eq!(
        disk.complete(&owner, id, 2).await.unwrap_err().code(),
        "missing_part"
    );
    disk.put_part(&owner, id, 2, &b"defg"[..]).await.unwrap();
    assert_eq!(
        disk.complete(&owner, id, 2).await.unwrap_err().code(),
        "file_integrity_mismatch"
    );
    assert!(disk.open(&owner, id).await.is_err());
    drop(disk);
    let disk = DiskStorage::new(root.path()).await.unwrap();
    disk.put_part(&owner, id, 1, &b"abc"[..]).await.unwrap();
    assert!(
        disk.put_part(&owner, id, 1, &b"too many bytes"[..])
            .await
            .is_err()
    );
    let (a, b) = tokio::join!(disk.complete(&owner, id, 2), disk.complete(&owner, id, 2));
    assert_eq!(a.unwrap().integrity, expected);
    assert_eq!(b.unwrap().id, id);
    assert!(disk.complete(&other, id, 2).await.is_err());
    assert!(disk.open(&other, id).await.is_err());
    let mut bytes = Vec::new();
    disk.open(&owner, id)
        .await
        .unwrap()
        .read_to_end(&mut bytes)
        .await
        .unwrap();
    assert_eq!(bytes, b"abcdefg");
}
