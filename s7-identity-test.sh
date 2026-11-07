#!/usr/bin/env bash
set -euo pipefail
if [[ ${1:-} != inner ]]; then
    exec bash scripts/builder-test.sh bash -euc 'bash s7-identity-test.sh inner'
fi
export RUST_MIN_STACK=33554432 RUST_TEST_THREADS=4
artifacts=$(mktemp)
trap 'rm -f "$artifacts"' EXIT
cargo build --locked -p codeyard -p codeyard-actions --bin codeyard --bin codeyard-actions --message-format=json >"$artifacts"
export GITSERV_TEST_OPENSSH_HELPER=$(python3 -I scripts/cargo-executable.py codeyard <"$artifacts")
export CODEYARD_ACTIONS_BIN=$(python3 -I scripts/cargo-executable.py codeyard-actions <"$artifacts")
status=0
cargo test --locked --no-fail-fast -p codeyard-db-core -p codeyard-db-repos -p codeyard-identity -p codeyard-api-http -p codeyard-cells-router -p codeyard-repos-core -p codeyard-ssh --lib || status=1
cargo test --locked -p codeyard-repos-core --test store foreign_publication -- --nocapture || status=1
cargo test --locked -p codeyard --test database foreign_authorization -- --nocapture || status=1
cargo test --locked -p codeyard --test cluster -- --nocapture || status=1
exit "$status"
