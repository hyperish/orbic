#!/usr/bin/env bash
# An isolated S3-compatible endpoint for the multipart contract test.
set -euo pipefail
cd "$(dirname "$0")/.."
export ROOT_ACCESS_KEY_ID ROOT_SECRET_ACCESS_KEY HYPERISH_TEST_S3_ENDPOINT
ROOT_ACCESS_KEY_ID=$(openssl rand -hex 12)
ROOT_SECRET_ACCESS_KEY=$(openssl rand -hex 32)
gateway=$(docker run --rm -d -p 127.0.0.1::7070 --tmpfs /data \
    -e ROOT_ACCESS_KEY_ID -e ROOT_SECRET_ACCESS_KEY versity/versitygw:v1.11.0 posix /data)
trap 'docker stop "$gateway" >/dev/null' EXIT
HYPERISH_TEST_S3_ENDPOINT="http://$(docker port "$gateway" 7070/tcp)"
for ((i=0; i<60; i++)); do
    if curl -s -o /dev/null --connect-timeout 1 "$HYPERISH_TEST_S3_ENDPOINT/"; then break; fi
    sleep 0.2
done
# The default S3 region creates a bucket without a LocationConstraint body.
RUST_S3_SKIP_LOCATION_CONSTRAINT=true cargo test --locked -p orbic-storage --features s3 --test object -- --ignored
