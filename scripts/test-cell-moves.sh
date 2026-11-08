#!/usr/bin/env bash
# Focused stage 6 tests; run inside scripts/builder-test.sh on the test builder.
set -euo pipefail
export RUST_MIN_STACK=33554432 RUST_TEST_THREADS=4
# Parse the baseline before code generation, with PostgreSQL's exact error position.
# This transaction is always rolled back, including when psql exits on an error.
psql "$GITSERV_TEST_DATABASE_URL" -X -v ON_ERROR_STOP=1 -q \
    -c 'BEGIN; CREATE SCHEMA cell_move_schema_check; SET LOCAL search_path=cell_move_schema_check;' \
    -f schema/postgres/0001.schema.sql -c ROLLBACK
artifacts=$(mktemp)
trap 'rm -f "$artifacts"' EXIT
cargo build --locked -p codeyard -p codeyard-runner --bin codeyard --bin codeyard-actions --message-format=json >"$artifacts"
export GITSERV_TEST_OPENSSH_HELPER=$(python3 -I scripts/cargo-executable.py codeyard <"$artifacts")
export CODEYARD_ACTIONS_BIN=$(python3 -I scripts/cargo-executable.py codeyard-actions <"$artifacts")
libraries=(-p codeyard-types -p codeyard-database -p codeyard-db-placement -p codeyard-db-directory -p codeyard-router -p codeyard-git)
# Compile everything first so the time limits below cover only the tests, and one hung
# test cannot hold the shared test slot.
cargo test --locked --no-run -p codeyard --test database --test cells
cargo test --locked --no-run "${libraries[@]}" --lib
status=0
timeout 15m cargo test --locked -p codeyard --test database -- moves:: move_http:: move_cli:: move_repository:: --nocapture --test-threads=1 || status=1
timeout 10m cargo test --locked "${libraries[@]}" --lib || status=1
timeout 20m cargo test --locked -p codeyard --test database -- --skip moves:: --skip move_http:: --skip move_cli:: --skip move_repository:: || status=1
timeout 20m cargo test --locked -p codeyard --test cells || status=1
exit "$status"
