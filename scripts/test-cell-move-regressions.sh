#!/usr/bin/env bash
# Isolated retries for unrelated cluster failures from the account-move test round.
# Run inside scripts/builder-test.sh on the test builder.
set -euo pipefail
export RUST_MIN_STACK=33554432 RUST_TEST_THREADS=1 RUST_BACKTRACE=1
artifacts=$(mktemp)
trap 'rm -f "$artifacts"' EXIT
cargo build --locked -p codeyard -p codeyard-runner --bin codeyard --bin codeyard-actions --message-format=json >"$artifacts"
export GITSERV_TEST_OPENSSH_HELPER=$(python3 -I scripts/cargo-executable.py codeyard <"$artifacts")
export CODEYARD_ACTIONS_BIN=$(python3 -I scripts/cargo-executable.py codeyard-actions <"$artifacts")
status=0
cargo test --locked -p codeyard --test cells -- authorization::foreign_account_rpc_requires_live_cell_keys_and_expires_revoked_home_answers --exact --nocapture || status=1
cargo test --locked -p codeyard --test placement -- replicas::a_complete_copy_removes_stale_unusable_directories_only --exact --nocapture || status=1
exit "$status"
