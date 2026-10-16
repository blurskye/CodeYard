#!/usr/bin/env sh
# Conservative defaults for this trusted local runner. A workflow's own
# container.options can override these; they are not a multi-tenant sandbox.
set -eu
container_options='--cpus=2 --memory=2g --pids-limit=512'
if [ -n "${GITSERV_CONTAINER_OPTIONS:-}" ]; then
  container_options="$container_options $GITSERV_CONTAINER_OPTIONS"
fi
exec /usr/local/bin/act \
  --concurrent-jobs 1 \
  --pull=false \
  "$@" \
  --container-options "$container_options"
