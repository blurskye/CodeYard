#!/usr/bin/env bash
# Run only the remaining cells-lane client rounds, retaining each round's own timeout.
# Invoke through scripts/builder-test.sh in the test environment.
set -euo pipefail
artifacts=$(mktemp)
trap 'rm -f "$artifacts"' EXIT
cargo build --locked -p codeyard -p codeyard-actions --bin codeyard --bin codeyard-actions --message-format=json >"$artifacts"
export GITSERV_TEST_OPENSSH_HELPER=$(python3 -I scripts/cargo-executable.py codeyard <"$artifacts")
export CODEYARD_ACTIONS_BIN=$(python3 -I scripts/cargo-executable.py codeyard-actions <"$artifacts")
status=0
for round in pgbouncer replica cluster quorum cells; do
    echo "== cells acceptance round: $round =="
    if bash "scripts/test-cell-e2e-$round.sh"; then
        echo "== cells acceptance round: $round passed =="
    else
        result=$?
        echo "== cells acceptance round: $round failed (exit $result) =="
        status=1
    fi
done
exit "$status"
