#!/usr/bin/env bash
# A private, disposable Valkey instance on a loopback port.
set -euo pipefail
cd "$(dirname "$0")/.."
export VALKEYCLI_AUTH HYPERISH_TEST_REDIS_URL
VALKEYCLI_AUTH=$(openssl rand -hex 24)
cache=$(docker run --rm -d -p 127.0.0.1::6379 --tmpfs /data -e VALKEYCLI_AUTH \
    valkey/valkey:8.1.3-alpine sh -c 'exec valkey-server --save "" --appendonly no --requirepass "$VALKEYCLI_AUTH"')
trap 'docker stop "$cache" >/dev/null' EXIT
export HYPERISH_TEST_REDIS_ADDR
HYPERISH_TEST_REDIS_ADDR=$(docker port "$cache" 6379/tcp)
HYPERISH_TEST_REDIS_URL="redis://:$VALKEYCLI_AUTH@$HYPERISH_TEST_REDIS_ADDR/0"
for ((i=0; i<60; i++)); do
    if docker exec "$cache" valkey-cli ping 2>/dev/null | grep -q PONG; then break; fi
    sleep 0.2
done
cargo test --locked -p orbic-cache --features redis --test redis -- --ignored
cargo test --locked -p orbic-tests --features cache-redis --test auth shared_sessions_are_revoked_on_every_replica -- --ignored
