#!/usr/bin/env bash
# One source check round: workspace check, Clippy and the browser WASM build.
set -uo pipefail
cargo check --locked --workspace --all-targets --message-format=short || exit 1
status=0
cargo clippy --locked --workspace --all-targets -- -D warnings || status=1
env CARGO_PROFILE_DEV_DEBUG=line-tables-only CARGO_PROFILE_TEST_DEBUG=line-tables-only \
    cargo check --locked -p codeyard-webui --target wasm32-unknown-unknown --no-default-features \
    --features web || status=1
echo "=== status $status"
exit "$status"
