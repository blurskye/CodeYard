#!/usr/bin/env bash
# Run one cluster test filter several times: bash .merge-repeat.sh FILTER COUNT
set -uo pipefail
if [[ ${1:-} != inner ]]; then
    exec timeout 30m bash scripts/builder-test.sh bash .merge-repeat.sh inner "$@"
fi
filter=$2 count=$3
export RUST_MIN_STACK=33554432 RUST_TEST_THREADS=4 CACHE_MODE=local CARGO_MANIFEST_DIR=$PWD
unset CACHE_URLS
artifacts=$(mktemp)
cargo test --locked -p codeyard --test cluster --no-run --message-format=json >"$artifacts" || exit 1
export GITSERV_TEST_OPENSSH_HELPER
GITSERV_TEST_OPENSSH_HELPER=$(python3 -I scripts/cargo-executable.py codeyard <"$artifacts") || exit 1
cluster=$(python3 -I -c '
import json, sys
for line in open(sys.argv[1]):
    m = json.loads(line)
    if m.get("reason") == "compiler-artifact" and m.get("executable") and m["target"]["name"] == "cluster" and m["profile"]["test"]:
        print(m["executable"]); break
' "$artifacts")
failed=0
for ((round = 1; round <= count; round++)); do
    echo "=== round $round"
    "$cluster" "$filter" || failed=$((failed + 1))
done
echo "=== $failed of $count rounds failed"
((failed == 0))
