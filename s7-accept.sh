#!/usr/bin/env bash
# Stage 7 acceptance: every affected target, compiled once with one package set so the
# runs below reuse the same build.
set -uo pipefail
if [[ ${1:-} != inner ]]; then
    exec bash scripts/builder-test.sh bash s7-accept.sh inner
fi
export RUST_MIN_STACK=33554432 RUST_TEST_THREADS=4 CACHE_MODE=local
unset CACHE_URLS
packages=(-p codeyard -p codeyard-cache -p codeyard-app -p codeyard-api-github
    -p codeyard-api-http -p codeyard-db-core -p codeyard-db-identity -p codeyard-db-repos
    -p codeyard-identity -p codeyard-cells-router -p codeyard-repos-core -p codeyard-ssh)
run() {
    echo "=== $*"
    "$@" || { echo "=== FAILED: $*"; status=1; }
}
status=0
cargo test --locked "${packages[@]}" --lib --test store --test database --test valkey \
    --test repositories --test cluster --no-run || exit 1
run python3 scripts/test-no-redis.py
run cargo test --locked --no-fail-fast "${packages[@]}" --lib
run cargo test --locked "${packages[@]}" --test store -- foreign_publication
run cargo test --locked "${packages[@]}" --test database -- foreign_authorization
valkey=$(mktemp)
echo '=== Valkey contracts'
bash scripts/with-valkey.sh cargo test --locked "${packages[@]}" --test valkey -- --nocapture 2>&1 \
    | tee "$valkey"
if [[ ${PIPESTATUS[0]} -ne 0 ]] || grep -q 'SKIP Valkey' "$valkey"; then
    echo '=== FAILED: Valkey contracts'
    status=1
fi
run cargo test --locked "${packages[@]}" --test repositories -- github_deploy::meta
run cargo test --locked "${packages[@]}" --test cluster
if timeout 1800 bash scripts/build-webui.sh debug; then
    export E2E_SKIP_BUILD=1 E2E_PROFILE=debug E2E_DATABASES=cells E2E_KEEP=1 E2E_TIMEOUT=1800
    run timeout 1800 bash scripts/e2e.sh
else
    echo '=== FAILED: web UI build'
    status=1
fi
echo "=== status $status"
exit "$status"
