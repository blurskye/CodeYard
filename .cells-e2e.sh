# One full e2e run on the debug profile; a 140-minute cap reports the step it stalls in.
export E2E_PROFILE=debug E2E_KEEP=1
out=$(mktemp)
timeout --signal=TERM --kill-after=60 8400 bash scripts/e2e.sh 2>&1 | tee "$out"
rc=${PIPESTATUS[0]}
work=$(sed -n 's/^e2e: temporary directory //p' "$out" | head -1)
if [[ $rc == 124 && -n $work ]]; then
    echo "e2e: stopped after 140 minutes; newest step logs:"
    ls -t "$work"/logs/*.log 2>/dev/null | head -3 | while read -r log; do echo "== $log"; tail -20 "$log"; done
fi
[[ $work == /tmp/codeyard-e2e.* ]] && rm -rf "$work"
rm -f "$out"
exit "$rc"
