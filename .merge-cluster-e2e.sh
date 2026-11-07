#!/usr/bin/env bash
# One e2e round (default cluster) with its directory kept, then the server-side causes of API
# failures from every node log. The kept directory is removed afterward.
set -uo pipefail
if [[ ${1:-} != inner ]]; then
    exec timeout 30m bash scripts/builder-test.sh bash .merge-cluster-e2e.sh inner "$@"
fi
out=$(mktemp)
E2E_KEEP=1 bash scripts/test-cell-e2e.sh "${2:-cluster}" 2>&1 | tee "$out"
status=${PIPESTATUS[0]}
kept=$(sed -n 's/^kept //p' "$out" | tail -n 1)
if [[ -n $kept && -d $kept ]]; then
    while IFS= read -r log; do
        echo "==== $log"
        grep -n -E 'dependency unavailable|mutation failed|unavailable|refused|panicked| ERROR ' "$log" | cut -c1-1500 | head -n 80
    done < <(find "$kept" -name server.log | sort)
    while IFS= read -r log; do
        echo "==== $log"
        head -c 3000 "$log"
        echo
    done < <(grep -l -E 'HTTP 503|Service unavailable' -r "$kept" --include='command-*.log' | sort | head -n 6)
    rm -rf "$kept"
fi
echo "=== status $status"
exit "$status"
