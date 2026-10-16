#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."

if command -v podman >/dev/null 2>&1; then
  compose=(podman compose)
elif command -v docker >/dev/null 2>&1; then
  compose=(docker compose)
else
  echo 'podman compose or docker compose is required' >&2
  exit 1
fi

env_file=$(mktemp)
trap 'rm -f "$env_file"' EXIT
master_key=$(printf 'a%.0s' {1..64})
resolve() {
  env -u GITSERV_NODE_ROLES -u SSH_TRUSTED_PROXY_IPS \
    PUBLIC_URL=http://localhost \
    SECRETS_MASTER_KEY="$master_key" \
    S3_ACCESS_KEY=fixture-access \
    S3_SECRET_KEY=fixture-secret \
    "$@" "${compose[@]}" --env-file "$env_file" -f compose.yaml config 2>/dev/null
}

web=$(resolve GITSERV_NODE_ROLES=web)
printf '%s\n' "$web" | grep -Fxq '      GITSERV_NODE_ROLES: web'

all=$(resolve)
printf '%s\n' "$all" | grep -Fxq '      GITSERV_NODE_ROLES: web,storage,ssh,actions-worker'
printf '%s\n' "$all" | grep -Eq "      SSH_TRUSTED_PROXY_IPS: (\"\"|'')$"

printf '%s\n' 'SSH_TRUSTED_PROXY_IPS=192.0.2.10,2001:db8::10' > "$env_file"
proxy=$(resolve)
printf '%s\n' "$proxy" | grep -Fxq '      SSH_TRUSTED_PROXY_IPS: 192.0.2.10,2001:db8::10'

printf '%s\n' 'SSH_TRUSTED_PROXY_IPS=' > "$env_file"
empty_proxy=$(resolve)
printf '%s\n' "$empty_proxy" | grep -Eq "      SSH_TRUSTED_PROXY_IPS: (\"\"|'')$"
echo 'Compose config: 5 checks passed'
