#!/usr/bin/env bash
# Merge acceptance: library tests, the quorum write test, the database target on both
# engines, the Valkey contracts, the cluster target and the GraphQL type check. Test
# executables are built once and run directly from their package directories.
set -uo pipefail
if [[ ${1:-} != inner ]]; then
    exec timeout 40m bash scripts/builder-test.sh bash .merge-tests.sh inner
fi
export RUST_MIN_STACK=33554432 RUST_TEST_THREADS=4 CACHE_MODE=local
unset CACHE_URLS
root=$PWD
artifacts=$(mktemp)
list=$(mktemp)
status=0
echo '=== graphql types'
python3 -I scripts/github-graphql-types.py --check || { echo '=== FAILED: graphql types'; status=1; }
cargo test --locked -p codeyard -p codeyard-cache -p codeyard-app -p codeyard-api-github \
    -p codeyard-api-http -p codeyard-db-core -p codeyard-db-identity -p codeyard-db-repos \
    -p codeyard-identity -p codeyard-cells-router -p codeyard-repos-core -p codeyard-repos-rpc \
    -p codeyard-ssh --lib --test writes --test database --test valkey --test cluster \
    --no-run --message-format=json >"$artifacts" || exit 1
export GITSERV_TEST_OPENSSH_HELPER
GITSERV_TEST_OPENSSH_HELPER=$(python3 -I scripts/cargo-executable.py codeyard <"$artifacts") || exit 1
python3 -I - "$artifacts" >"$list" <<'EOF'
import json, os, sys
seen = set()
for line in open(sys.argv[1]):
    message = json.loads(line)
    if message.get("reason") != "compiler-artifact" or not message.get("executable"):
        continue
    if not message["profile"]["test"]:
        continue
    target = message["target"]
    row = (target["kind"][0], target["name"], os.path.dirname(message["manifest_path"]),
           message["executable"])
    if row not in seen:
        seen.add(row)
        print(*row, sep="\t")
EOF
tests() {
    local kind=$1 name=$2 found=0 k n dir exe
    shift 2
    while IFS=$'\t' read -r k n dir exe; do
        [[ $k == "$kind" && ($name == '*' || $n == "$name") ]] || continue
        found=1
        echo "=== $k $n"
        (cd "$dir" && CARGO_MANIFEST_DIR=$dir "$@" "$exe" "${args[@]}") || {
            echo "=== FAILED: $k $n"
            status=1
        }
    done <"$list"
    [[ $found == 1 ]] || { echo "=== FAILED: no $kind $name"; status=1; }
}
args=()
tests lib '*'
tests test writes
tests test database
valkey=$(mktemp)
args=(--nocapture)
tests test valkey bash "$root/scripts/with-valkey.sh" 2>&1 | tee "$valkey"
if ! grep -q 'test result: ok. 2 passed' "$valkey" || grep -q 'SKIP Valkey\|FAILED' "$valkey"; then
    echo '=== FAILED: Valkey contracts'
    status=1
fi
args=()
tests test cluster
echo "=== status $status"
exit "$status"
