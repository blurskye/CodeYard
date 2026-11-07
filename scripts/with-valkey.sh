#!/usr/bin/env bash
# Run a command with disposable loopback Valkey servers for the cache contract tests:
# one standalone server and a three-shard Cluster. The cache logs in as a user that may
# only run GET, MGET, SET and DEL on `cy:*` keys, plus the client's connection and
# Cluster topology commands, so any other command fails the contract.
set -euo pipefail
[[ $# -gt 0 ]] || { echo 'Supply a command.' >&2; exit 2; }
command -v valkey-server >/dev/null || { echo 'valkey-server is not installed.' >&2; exit 2; }
dir=$(mktemp -d)
pids=()
cleanup() {
    for pid in "${pids[@]}"; do kill "$pid" 2>/dev/null || true; done
    for pid in "${pids[@]}"; do wait "$pid" 2>/dev/null || true; done
    rm -rf "$dir"
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM HUP
password=$(python3 -c 'import secrets; print(secrets.token_hex(24))')
# Four ports whose Cluster bus ports (port + 10000) are also free.
read -r -a ports < <(python3 -I - <<'EOF'
import random, socket
def free(port):
    with socket.socket() as s:
        try:
            s.bind(("127.0.0.1", port))
            return True
        except OSError:
            return False
chosen = []
while len(chosen) < 4:
    port = random.randrange(20000, 55000)
    if port not in chosen and free(port) and free(port + 10000):
        chosen.append(port)
print(*chosen)
EOF
)
start() {
    local port=$1; shift
    mkdir -p "$dir/$port"
    cat >"$dir/$port/valkey.conf" <<EOF
bind 127.0.0.1
port $port
dir $dir/$port
save ""
appendonly no
maxmemory 64mb
maxmemory-policy allkeys-lru
user cache on >$password ~cy:* resetchannels -@all +get +mget +set +del +ping +asking +cluster|slots +cluster|shards
EOF
    printf '%s\n' "$@" >>"$dir/$port/valkey.conf"
    valkey-server "$dir/$port/valkey.conf" >"$dir/$port/log" 2>&1 &
    pids+=("$!")
}
ready() {
    for _ in $(seq 100); do
        valkey-cli -h 127.0.0.1 -p "$1" ping 2>/dev/null | grep -q PONG && return 0
        sleep 0.1
    done
    echo "Valkey on port $1 did not start." >&2
    cat "$dir/$1/log" >&2
    return 1
}
start "${ports[0]}"
for port in "${ports[@]:1}"; do
    start "$port" 'cluster-enabled yes' "cluster-config-file $dir/$port/nodes.conf" \
        'cluster-require-full-coverage no'
done
for port in "${ports[@]}"; do ready "$port"; done
timeout 30 valkey-cli --cluster create "127.0.0.1:${ports[1]}" "127.0.0.1:${ports[2]}" \
    "127.0.0.1:${ports[3]}" --cluster-replicas 0 --cluster-yes >"$dir/create.log" 2>&1 \
    || { cat "$dir/create.log" >&2; exit 1; }
for port in "${ports[@]:1}"; do
    for attempt in $(seq 100); do
        valkey-cli -h 127.0.0.1 -p "$port" cluster info | grep -q 'cluster_state:ok' && break
        [[ $attempt -lt 100 ]] || { echo "Valkey Cluster on port $port is not ready." >&2; exit 1; }
        sleep 0.1
    done
done
valkey-server --version
export CODEYARD_TEST_VALKEY_URL="redis://cache:$password@127.0.0.1:${ports[0]}/"
export CODEYARD_TEST_VALKEY_CLUSTER_URLS="redis://cache:$password@127.0.0.1:${ports[1]}/,redis://cache:$password@127.0.0.1:${ports[2]}/"
"$@"
