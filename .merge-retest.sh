#!/usr/bin/env bash
# Second merge round: the database target on both engines, the cluster target, then the
# quorum cluster tests twice more to tell a timing failure from a regular one.
set -uo pipefail
if [[ ${1:-} != inner ]]; then
    exec timeout 40m bash scripts/builder-test.sh bash .merge-retest.sh inner
fi
export RUST_MIN_STACK=33554432 RUST_TEST_THREADS=4 CACHE_MODE=local
unset CACHE_URLS
artifacts=$(mktemp)
status=0
cargo test --locked -p codeyard --test database --test cluster --no-run \
    --message-format=json >"$artifacts" || exit 1
export GITSERV_TEST_OPENSSH_HELPER
GITSERV_TEST_OPENSSH_HELPER=$(python3 -I scripts/cargo-executable.py codeyard <"$artifacts") || exit 1
binary() {
    python3 -I -c '
import json, sys
for line in open(sys.argv[1]):
    m = json.loads(line)
    if m.get("reason") == "compiler-artifact" and m.get("executable") and m["target"]["name"] == sys.argv[2] and m["profile"]["test"]:
        print(m["executable"]); break
' "$artifacts" "$1"
}
database=$(binary database) cluster=$(binary cluster)
[[ -n $database && -n $cluster ]] || { echo '=== FAILED: test binaries not found'; exit 1; }
export CARGO_MANIFEST_DIR=$PWD
echo '=== test database'
"$database" || { echo '=== FAILED: test database'; status=1; }
echo '=== test cluster'
"$cluster" || { echo '=== FAILED: test cluster'; status=1; }
for round in 2 3; do
    echo "=== test cluster quorum round $round"
    "$cluster" quorum:: || { echo "=== FAILED: quorum round $round"; status=1; }
done
echo "=== status $status"
exit "$status"
