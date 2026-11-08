export RUST_MIN_STACK=33554432 RUST_TEST_THREADS=4 RUST_BACKTRACE=1
artifacts=$(mktemp)
trap 'rm -f "$artifacts"' EXIT
cargo build --locked -p codeyard -p codeyard-runner --bin codeyard --bin codeyard-actions --message-format=json >"$artifacts"
export GITSERV_TEST_OPENSSH_HELPER=$(python3 -I scripts/cargo-executable.py codeyard <"$artifacts") CODEYARD_ACTIONS_BIN=$(python3 -I scripts/cargo-executable.py codeyard-actions <"$artifacts")
status=0
cargo test --locked -p codeyard-git --lib maintenance || status=1
cargo test --locked -p codeyard-db-directory -p codeyard-types --lib || status=1
cargo test --locked -p codeyard-database --lib -- cells:: connection || status=1
cargo test --locked -p codeyard-app --lib packages || status=1
cargo test --locked -p codeyard --test database || status=1
cargo test --locked -p codeyard --test cluster -- cells_ directory search backups quorum_default --nocapture || status=1
exit "$status"
