#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."

if command -v docker-compose >/dev/null 2>&1; then
  compose=(docker-compose)
elif command -v podman >/dev/null 2>&1; then
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
    DATABASE_URL=postgres://gitserv:gitserv@postgres:5432/gitserv \
    S3_PUBLIC_ENDPOINT=http://objects.localhost:11002 \
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
encoded=$(resolve POSTGRES_PASSWORD=p%40ss DATABASE_URL=postgres://gitserv:p%2540ss@postgres:5432/gitserv)
printf '%s\n' "$encoded" | grep -Fxq '      POSTGRES_PASSWORD: p%40ss'
printf '%s\n' "$encoded" | grep -Fxq '      DATABASE_URL: postgres://gitserv:p%2540ss@postgres:5432/gitserv'

listeners=$(resolve HTTP_ADDR=0.0.0.0:4000 SSH_ADDR=0.0.0.0:4444 API_HTTP_PORT=14000 SSH_PORT=14444)
printf '%s\n' "$listeners" | grep -Fxq '      HTTP_ADDR: 0.0.0.0:3000'
printf '%s\n' "$listeners" | grep -Fxq '      SSH_ADDR: 0.0.0.0:2222'
printf '%s\n' "$listeners" | grep -Eq 'published: [^0-9]*14000[^0-9]*$'
printf '%s\n' "$listeners" | grep -Eq 'published: [^0-9]*14444[^0-9]*$'

if resolve DATABASE_URL= >/dev/null; then
  echo 'Compose accepted a missing database URL' >&2; exit 1
fi
if resolve S3_PUBLIC_ENDPOINT= >/dev/null; then
  echo 'Compose accepted a missing browser object endpoint' >&2; exit 1
fi
echo 'Compose config: 13 checks passed'
