#!/usr/bin/env bash
set -euo pipefail
artifacts=$(mktemp)
trap 'rm -f "$artifacts"' EXIT
cargo build --locked -p codeyard -p codeyard-runner --bin codeyard --bin codeyard-actions --message-format=json >"$artifacts"
export GITSERV_TEST_OPENSSH_HELPER=$(python3 -I scripts/cargo-executable.py codeyard <"$artifacts")
export CODEYARD_ACTIONS_BIN=$(python3 -I scripts/cargo-executable.py codeyard-actions <"$artifacts")
export RUST_MIN_STACK=16777216
status=0
cargo test --locked -p codeyard --test cluster cells_search -- --test-threads=2 || status=1
cargo test --locked -p codeyard --test database directory:: -- --test-threads=2 || status=1
cargo test --locked -p codeyard --test collaboration search -- --test-threads=2 || status=1
cargo test --locked -p codeyard-database -p codeyard-db-directory -p codeyard-app --lib -- --test-threads=2 || status=1
exit "$status"
