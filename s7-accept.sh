#!/usr/bin/env bash
# Stage 7 acceptance: every affected target, compiled once. The test executables run
# directly, because a second `cargo test` with other target filters resolves different
# features and rebuilds the workspace.
set -uo pipefail
if [[ ${1:-} != inner ]]; then
    exec bash scripts/builder-test.sh bash s7-accept.sh inner
fi
export RUST_MIN_STACK=33554432 RUST_TEST_THREADS=4 CACHE_MODE=local
unset CACHE_URLS
root=$PWD
artifacts=$(mktemp)
list=$(mktemp)
status=0
cargo test --locked -p codeyard -p codeyard-cache -p codeyard-app -p codeyard-api-github \
    -p codeyard-api-http -p codeyard-db-core -p codeyard-db-identity -p codeyard-db-repos \
    -p codeyard-identity -p codeyard-cells-router -p codeyard-repos-core -p codeyard-ssh \
    --lib --test store --test database --test valkey --test repositories --test cluster \
    --no-run --message-format=json >"$artifacts" || exit 1
# kind, target name, package directory and executable of every test binary.
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
# Run every listed binary of a kind (and name, unless `*`) from its package directory,
# after an optional wrapper command and before the libtest arguments in `args`.
tests() {
    local kind=$1 name=$2 found=0 k n dir exe
    shift 2
    while IFS=$'\t' read -r k n dir exe; do
        [[ $k == "$kind" && ($name == '*' || $n == "$name") ]] || continue
        found=1
        echo "=== $k $n $*"
        (cd "$dir" && CARGO_MANIFEST_DIR=$dir "$@" "$exe" "${args[@]}") || {
            echo "=== FAILED: $k $n"
            status=1
        }
    done <"$list"
    [[ $found == 1 ]] || { echo "=== FAILED: no $kind $name"; status=1; }
}
echo '=== test-no-redis'
python3 scripts/test-no-redis.py || { echo '=== FAILED: test-no-redis'; status=1; }
args=()
tests lib '*'
args=(foreign_collaborator_pushes_and_clones_through_public_ssh_then_loses_access)
for _ in 1 2 3; do tests lib codeyard_ssh; done
args=(foreign_publication)
tests test store
args=(foreign_authorization)
tests test database
valkey=$(mktemp)
args=(--nocapture)
tests test valkey bash "$root/scripts/with-valkey.sh" 2>&1 | tee "$valkey"
if ! grep -q 'test result: ok. 2 passed' "$valkey" || grep -q 'SKIP Valkey\|FAILED' "$valkey"; then
    echo '=== FAILED: Valkey contracts'
    status=1
fi
args=(github_deploy::meta)
tests test repositories
args=()
tests test cluster
if timeout 1800 bash scripts/build-webui.sh debug; then
    export E2E_SKIP_BUILD=1 E2E_PROFILE=debug E2E_DATABASES=cells E2E_KEEP=1 E2E_TIMEOUT=1800
    timeout 1800 bash scripts/e2e.sh || { echo '=== FAILED: e2e cells'; status=1; }
else
    echo '=== FAILED: web UI build'
    status=1
fi
echo "=== status $status"
exit "$status"
