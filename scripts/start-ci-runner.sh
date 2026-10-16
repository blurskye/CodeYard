#!/usr/bin/env bash
# Start a dedicated rootless Podman API for trusted local Actions workflows.
# Separate storage avoids collisions with application containers; it is NOT a
# security boundary. Anyone with this socket can execute code as your host user.
set -euo pipefail
umask 077

if [[ $(id -u) == 0 ]]; then
  echo 'Run this as your regular user, not root.' >&2
  exit 1
fi
: "${XDG_RUNTIME_DIR:?A user login session with XDG_RUNTIME_DIR is required}"
root="${CI_RUNNER_ROOT:-${XDG_CACHE_HOME:-$HOME/.cache}/codeyard-ci/storage}"
runtime="$XDG_RUNTIME_DIR/codeyard-ci"
socket="$runtime/podman.sock"
mkdir -p "$root" "$runtime"

if ! systemctl --user is-active --quiet codeyard-ci-engine.service; then
  systemd-run --user --unit=codeyard-ci-engine --collect \
    --property=Restart=on-failure --property=UMask=0077 \
    --setenv="PATH=$PATH" \
    "$(command -v podman)" --root "$root" --runroot "$runtime/run" \
    system service --time=0 "unix://$socket"
fi

deadline=$((SECONDS + 30))
while ((SECONDS < deadline)); do
  if curl --silent --fail --connect-timeout 2 --max-time 5 --unix-socket "$socket" http://localhost/_ping >/dev/null; then
    printf '\nRunner ready. Add these options when creating the app container:\n'
    printf '  -v %q:%q:ro\n' "$socket" "$socket"
    printf '  -e CI_EXECUTION_MODE=podman\n'
    printf '  -e CI_PODMAN_SOCKET=%q\n' "$socket"
    printf '\nStop: systemctl --user stop codeyard-ci-engine.service\n'
    exit 0
  fi
  sleep 1
done
journalctl --user -u codeyard-ci-engine.service -n 30 --no-pager >&2
exit 1
