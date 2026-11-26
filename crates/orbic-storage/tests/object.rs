//! End-to-end signing, resumption and verification against a private bucket.

#![cfg(feature = "s3")]

use std::process::Command;

use orbic_core::Principal;
use orbic_storage::{CompletedPart, Integrity, ObjectStorage};
use s3::{Bucket, BucketConfiguration, Region, creds::Credentials};
use sha2::{Digest, Sha256};

/// Reads the body from a file to exercise a normal unsigned HTTP client with
/// only a presigned URL. The test never prints a URL or signing credential.
fn upload(url: &str, data: &[u8]) -> String {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("part");
    std::fs::write(&path, data).unwrap();
    let response = Command::new("curl")
        .args(["--silent", "--fail", "--dump-header", "-", "--upload-file"])
        .arg(path)
        .arg(url)
        .output()
        .unwrap();
    assert!(
        response.status.success(),
        "signed PUT failed ({})",
        response.status
    );
    String::from_utf8(response.stdout)
        .unwrap()
        .lines()
        .filter_map(|line| line.split_once(':'))
        .find(|(name, _)| name.eq_ignore_ascii_case("etag"))
        .unwrap()
        .1
        .trim()
        .to_owned()
}

#[tokio::test]
#[ignore = "run with the local gateway via scripts/test-storage-s3.sh"]
async fn signed_parts_resume_and_verified_downloads_require_the_owner() {
    let endpoint = std::env::var("HYPERISH_TEST_S3_ENDPOINT").unwrap();
    let access = std::env::var("ROOT_ACCESS_KEY_ID").unwrap();
    let secret = std::env::var("ROOT_SECRET_ACCESS_KEY").unwrap();
    let region = Region::Custom {
        region: "us-east-1".into(),
        endpoint: endpoint.clone(),
    };
    let credentials = Credentials::new(Some(&access), Some(&secret), None, None, None).unwrap();
    let bucket = format!("orbic-{}", hyve::Ulid::new().to_string().to_lowercase());
    Bucket::create_with_path_style(&bucket, region, credentials, BucketConfiguration::default())
        .await
        .unwrap();
    let connect =
        || ObjectStorage::new(&endpoint, "us-east-1", &bucket, &access, &secret, None).unwrap();
    let owner = Principal::new("owner");
    let other = Principal::new("unrelated");
    let first = vec![123; 5 * 1024 * 1024];
    let tail = b"last";
    let digest = Sha256::new()
        .chain_update(&first)
        .chain_update(tail)
        .finalize();
    let expected = Integrity {
        bytes: (first.len() + tail.len()) as u64,
        sha256: digest.into(),
    };
    let store = connect();
    let id = store.begin(&owner, expected).await.unwrap();
    assert!(store.part_url(&other, id, 1).await.is_err());
    assert!(store.download_url(&owner, id).await.is_err());
    let first_etag = upload(&store.part_url(&owner, id, 1).await.unwrap(), &first);
    drop(store);
    let store = connect();
    upload(&store.part_url(&owner, id, 2).await.unwrap(), b"discard");
    let second_etag = upload(&store.part_url(&owner, id, 2).await.unwrap(), tail);
    let parts = [
        CompletedPart {
            number: 1,
            etag: first_etag,
        },
        CompletedPart {
            number: 2,
            etag: second_etag,
        },
    ];
    assert!(store.complete(&other, id, &parts).await.is_err());
    assert_eq!(
        store.complete(&owner, id, &parts).await.unwrap().integrity,
        expected
    );
    assert_eq!(store.complete(&owner, id, &parts).await.unwrap().id, id);
    assert!(store.download_url(&other, id).await.is_err());
    let get = store.download_url(&owner, id).await.unwrap();
    let result = Command::new("curl")
        .args(["--silent", "--fail", &get])
        .output()
        .unwrap();
    assert!(result.status.success());
    assert_eq!(Sha256::digest(result.stdout)[..], expected.sha256);
    let wrong = store.begin(&owner, expected).await.unwrap();
    let etag = upload(&store.part_url(&owner, wrong, 1).await.unwrap(), b"wrong");
    assert_eq!(
        store
            .complete(&owner, wrong, &[CompletedPart { number: 1, etag }])
            .await
            .unwrap_err()
            .code(),
        "file_integrity_mismatch"
    );
    assert!(store.download_url(&owner, wrong).await.is_err());
}
