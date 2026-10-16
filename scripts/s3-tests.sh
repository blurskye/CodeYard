#!/usr/bin/env bash
# Runs the #[ignore]d S3 tests of tests/production_storage.rs against a throwaway RustFS.
#
#   GITSERV_TEST_DATABASE_URL=postgres://USER:PASSWORD@HOST:5432/DATABASE bash scripts/s3-tests.sh
#
# Starts RustFS on a random loopback port with random credentials and a fresh bucket
# (plus the ownership marker object the tests require), runs the tests, and stops it.
#   RUSTFS_BIN  the rustfs executable, version 1.0.1 (the default is a local path)
set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/.."

RUSTFS_BIN=${RUSTFS_BIN:-$(command -v rustfs || true)}
[[ -x $RUSTFS_BIN ]] || { echo "rustfs not found at $RUSTFS_BIN (set RUSTFS_BIN)" >&2; exit 2; }
: "${GITSERV_TEST_DATABASE_URL:?set it to a PostgreSQL URL}"

port=$((40000 + RANDOM % 20000))
access=fixture$(od -An -N6 -tx1 /dev/urandom | tr -d ' \n')
secret=$(od -An -N24 -tx1 /dev/urandom | tr -d ' \n')
bucket=gitserv-storage-test-$(od -An -N16 -tx1 /dev/urandom | tr -d ' \n')
work=$(mktemp -d)
rustfs_pid=
cleanup() {
    [[ -z $rustfs_pid ]] || kill "$rustfs_pid" 2>/dev/null || true
    rm -rf "$work"
}
trap cleanup EXIT

s3() { curl -sS -f --aws-sigv4 "aws:amz:us-east-1:s3" --user "$access:$secret" "$@"; }

mkdir "$work/data"
RUSTFS_ACCESS_KEY=$access RUSTFS_SECRET_KEY=$secret RUSTFS_OBS_LOG_DIRECTORY=$work/logs \
    "$RUSTFS_BIN" server --address "127.0.0.1:$port" "$work/data" >"$work/rustfs.log" 2>&1 &
rustfs_pid=$!
# RustFS answers 503 while it initializes; it is ready once it refuses an unsigned request.
for _ in $(seq 1 120); do
    [[ $(curl -s -o /dev/null -w '%{http_code}' "http://127.0.0.1:$port/") == 403 ]] && break
    kill -0 "$rustfs_pid" 2>/dev/null || { cat "$work/rustfs.log" >&2; exit 1; }
    sleep 0.5
done

printf 'production-storage-v1:%s' "$bucket" >"$work/marker"
for attempt in 1 2 3 4 5 6 7 8 9 10; do
    s3 -o /dev/null -X PUT "http://127.0.0.1:$port/$bucket" \
        && s3 -o /dev/null -X PUT --data-binary "@$work/marker" "http://127.0.0.1:$port/$bucket/_gitserv_fixture_owner" \
        && break
    [[ $attempt -lt 10 ]] || { cat "$work/rustfs.log" >&2; exit 1; }
    sleep 1
done

# localhost and 127.0.0.1 are different origins, as the first test requires.
GITSERV_STORAGE_TEST_ENDPOINT=http://127.0.0.1:$port \
GITSERV_STORAGE_TEST_PUBLIC_ENDPOINT=http://localhost:$port \
GITSERV_STORAGE_TEST_BUCKET=$bucket \
GITSERV_STORAGE_TEST_ACCESS_KEY=$access \
GITSERV_STORAGE_TEST_SECRET_KEY=$secret \
    cargo test --locked --test production_storage -- --ignored --test-threads=1
