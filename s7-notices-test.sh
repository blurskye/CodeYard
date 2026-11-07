#!/usr/bin/env bash
set -euo pipefail
if [[ ${1:-} != inner ]]; then
    exec bash scripts/builder-test.sh bash -euc 'bash s7-notices-test.sh inner'
fi
export RUST_MIN_STACK=33554432 RUST_TEST_THREADS=4
artifacts=$(mktemp)
trap 'rm -f "$artifacts"' EXIT
cargo build --locked -p codeyard -p codeyard-actions --bin codeyard --bin codeyard-actions --message-format=json >"$artifacts"
export GITSERV_TEST_OPENSSH_HELPER=$(python3 -I scripts/cargo-executable.py codeyard <"$artifacts")
export CODEYARD_ACTIONS_BIN=$(python3 -I scripts/cargo-executable.py codeyard-actions <"$artifacts")
status=0
cargo test --locked -p codeyard-db-core -p codeyard-db-repos -p codeyard-notifications -p codeyard-cells-router -p codeyard-app --lib || status=1
cargo test --locked -p codeyard --test database cell_stubs -- --nocapture || status=1
cargo test --locked -p codeyard --test cluster -- --nocapture || status=1
cargo test --locked -p codeyard --test collaboration notifications -- --nocapture || status=1
cargo test --locked -p codeyard --test notification_polling -- --nocapture || status=1
if timeout 1800 bash scripts/build-webui.sh debug; then
    E2E_SKIP_BUILD=1 E2E_PROFILE=debug E2E_DATABASES=cells E2E_KEEP=1 E2E_TIMEOUT=1800 timeout 1800 bash scripts/e2e.sh || status=1
else
    status=1
fi
exit "$status"
