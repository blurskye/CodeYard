#!/usr/bin/env bash
# End-to-end check of the real codeyard server, on SQLite, PostgreSQL and a cluster.
#
# Needs Rust, Git, OpenSSH (ssh and ssh-keygen), Python 3 with jsonschema, gh, OpenSSL,
# Node.js 24, act and the Dioxus CLI, plus a
# PostgreSQL server for the PostgreSQL and cluster rounds:
#
#   GITSERV_TEST_DATABASE_URL=postgres://USER:PASSWORD@HOST:5432/DATABASE bash scripts/e2e.sh
#
# Settings are environment variables, for example: E2E_SKIP_BUILD=1 bash scripts/e2e.sh
#
# It builds the browser bundle and the server binary, then, for each database,
# starts `codeyard serve` (built-in SSH, local CI with act) on free localhost ports
# and drives it like a user would. Each single-node round ends with a backup under load,
# a restore into an empty installation, a kept deletion and a rebuild; the PostgreSQL
# round also pairs two separate runners and restores the database to before newer Git.
# The cluster round turns on cluster mode and uses web/SSH, two storage nodes and a
# database-free runner, paired by ticket, declared key and token. Checks use the HTML forms with their CSRF tokens (no
# JavaScript), the JSON API, and git over SSH and HTTP (the ssh client only talks to
# that local server). Unmodified gh uses a test-owned TLS GHES tunnel to these same
# servers; pinned actions/github-script runs on the native runner, and signed webhook
# deliveries are checked against the vendored schemas. Every step prints PASS or FAIL. The exit status is non-zero
# when any step failed or was skipped. A cold build takes several minutes; with the
# build done, the SQLite round takes a few minutes and the PostgreSQL round about ten.
#
# Settings (environment):
#   E2E_DATABASES   rounds to run, default "sqlite postgres pgbouncer replica cluster quorum cells". PostgreSQL
#                   rounds need GITSERV_TEST_DATABASE_URL; if it
#                   is missing the round fails, it is not skipped.
#   E2E_PROFILE     release (default) or debug, for the server and the bundle.
#   E2E_SKIP_BUILD  1: use the artifacts of an earlier run, build nothing.
#   E2E_BUILD_ONLY  1: build, then stop.
#   E2E_GITHUB_ONLY 1: run only the real GitHub clients and their single-node fixtures.
#   E2E_ACTIONS_ONLY 1: run only real cache/artifact actions on SQLite and PostgreSQL.
#   E2E_KEEP        1: keep the temporary directory (server log, repositories).
#   E2E_VERBOSE     1: print the log of passing steps as well.
#   E2E_TIMEOUT     seconds after which the whole run is stopped (default 1500), so a
#                   hung step cannot hold the run open.
#   E2E_ACT_BINARY  path to act for local CI runs. Use act 0.2.89, the version the
#                   server image ships (see Containerfile).
#
# Local CI runs need a writable delegated cgroup v2 (docs/CI_LOCAL_RUNNER.md), for example
# `systemd-run --user --scope -p Delegate=yes bash scripts/e2e.sh`. Without one, the checks
# that need a finished run are reported as SKIP, so the run still exits non-zero.
set -uo pipefail

# Stop a hung run: run again under timeout, once.
if [[ -z ${E2E_UNDER_TIMEOUT:-} ]]; then
    export E2E_UNDER_TIMEOUT=1
    exec timeout --kill-after=30 "${E2E_TIMEOUT:-1500}" bash "$0" "$@"
fi

exec < /dev/null
ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
cd "$ROOT" || exit 2
HELPER=$ROOT/tests/e2e/e2e-web.py

E2E_DATABASES=${E2E_DATABASES:-sqlite postgres pgbouncer replica cluster quorum cells}
E2E_PROFILE=${E2E_PROFILE:-release}
E2E_SKIP_BUILD=${E2E_SKIP_BUILD:-0}
E2E_BUILD_ONLY=${E2E_BUILD_ONLY:-0}
E2E_KEEP=${E2E_KEEP:-0}
E2E_VERBOSE=${E2E_VERBOSE:-0}
ACT=${E2E_ACT_BINARY:-$(command -v act || true)}
[[ -z $ACT ]] || ACT=$(realpath -m -- "$ACT")
TARGET_DIR=$(realpath -m -- "${CARGO_TARGET_DIR:-$ROOT/target}")
export WEBUI_DIST=$(realpath -m -- "${WEBUI_DIST:-$ROOT/dist/webui}")
# Repository selectors and injected configuration must never reach fixture Git commands.
for git_variable in ${!GIT_@}; do unset "$git_variable"; done

case $E2E_PROFILE in
    release) CARGO_PROFILE=(--release) ;;
    debug) CARGO_PROFILE=() ;;
    *) echo "E2E_PROFILE must be release or debug" >&2; exit 2 ;;
esac
# Cargo puts the binaries for an explicit build target under a directory of its name.
PROFILE_DIR=$TARGET_DIR/${CARGO_BUILD_TARGET:+$CARGO_BUILD_TARGET/}$E2E_PROFILE
BIN=$PROFILE_DIR/codeyard
ACTIONS_BIN=$PROFILE_DIR/codeyard-actions
GITHUB_HELPER=$ROOT/tests/e2e/e2e-github.py
GITHUB_PYTHON=${CODEYARD_E2E_PYTHON:-python3}

# ssh and ssh-keygen may be outside PATH (NixOS keeps them in the system profile).
find_tool() {
    command -v "$1" 2>/dev/null && return 0
    [[ -x /run/current-system/sw/bin/$1 ]] && { echo "/run/current-system/sw/bin/$1"; return 0; }
    return 1
}

# ------------------------------------------------------------------ results --

WORK=$(mktemp -d /tmp/codeyard-e2e.XXXXXXXX)
LOGS=$WORK/logs
mkdir -p "$LOGS"
RESULTS=()
STEP_NO=0
ROUND=build
ROUND_BLOCKED=0
declare -A SERVER_PIDS=() HTTP_PORTS=() SSH_PORTS=() IROH_PORTS=()
CLUSTER_NODES=(web1 store1 store2 ci1)
PG_DB=''
PG_OWNED=()
PGBOUNCER_PID=''
READ_REPLICA_DATA=''
DATABASE_READ_URL=''
RUN=''
STARTED=$SECONDS

drop_pg() { # drops the round's database and the one a restore made
    ((${#PG_OWNED[@]})) || return 0
    local dropdb name
    for name in "${PG_OWNED[@]}"; do
        if ! dropdb=$(pg_tool dropdb) || ! pg_command "$GITSERV_TEST_DATABASE_URL" \
            "$dropdb" --maintenance-db --if-exists --force "$name"; then
            echo "Failed to drop disposable PostgreSQL database $name" >&2
            return 1
        fi
    done
    PG_DB=''
    PG_OWNED=()
    return 0
}

cleanup() {
    local rc=$?
    trap - EXIT
    terminate_operators
    terminate_servers
    stop_pgbouncer
    stop_read_replica
    drop_pg || rc=1
    if [[ $E2E_KEEP == 1 ]]; then
        echo "kept $WORK"
    else
        rm -rf "$WORK"
    fi
    exit "$rc"
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

# step [-f] [-i] NAME FUNCTION [ARGS...]
#   Runs FUNCTION, normally in a subshell with `set -e`, its output in a log.
#   -f  a failure skips the rest of the round
#   -i  run in this shell (needed to change variables or to `wait` for the server)
step() {
    local fatal=0 inline=0
    while [[ ${1:-} == -* ]]; do
        case $1 in -f) fatal=1 ;; -i) inline=1 ;; esac
        shift
    done
    local name=$1
    shift
    STEP_NO=$((STEP_NO + 1))
    if ((ROUND_BLOCKED)); then
        RESULTS+=("SKIP|$ROUND|$name|0")
        printf 'SKIP  [%s] %s (an earlier required step failed)\n' "$ROUND" "$name"
        return 1
    fi
    local log started=$SECONDS rc
    log=$LOGS/$ROUND-$(printf '%02d' "$STEP_NO").log
    STEP_SKIP=$log.skip
    if ((inline)); then
        "$@" >"$log" 2>&1
        rc=$?
    else
        (
            set -euo pipefail
            shopt -s inherit_errexit
            "$@"
        ) >"$log" 2>&1
        rc=$?
    fi
    local took=$((SECONDS - started))
    # needs_ci_runs stopped the step; it neither fails nor blocks the round.
    if ((rc != 0)) && [[ -s $STEP_SKIP ]]; then
        RESULTS+=("SKIP|$ROUND|$name|$took")
        printf 'SKIP  [%s] %s (%ss; %s)\n' "$ROUND" "$name" "$took" "$(<"$STEP_SKIP")"
        return 0
    fi
    if ((rc == 0)); then
        RESULTS+=("PASS|$ROUND|$name|$took")
        printf 'PASS  [%s] %s (%ss)\n' "$ROUND" "$name" "$took"
        if ((E2E_VERBOSE)); then sed 's/^/        | /' "$log"; fi
    else
        RESULTS+=("FAIL|$ROUND|$name|$took")
        printf 'FAIL  [%s] %s (%ss)\n' "$ROUND" "$name" "$took"
        tail -n 60 "$log" | sed 's/^/        | /'
        if ((fatal)); then ROUND_BLOCKED=1; fi
    fi
    return "$rc"
}

# needs_ci_runs || return: where local CI runs cannot execute, the step ends here as SKIP.
needs_ci_runs() {
    [[ -n $CI_RUNS_UNAVAILABLE ]] || return 0
    printf '%s\n' "$CI_RUNS_UNAVAILABLE" | tee "$STEP_SKIP" >&2
    return 1
}

# State handed from one step to the next (steps run in subshells).
put() { printf '%s' "$2" >"$STATE/$1"; }
get() {
    [[ -s $STATE/$1 ]] || { echo "missing '$1': an earlier step did not produce it" >&2; return 1; }
    cat "$STATE/$1"
}

must_fail() { # must_fail DESCRIPTION EXPECTED-DIAGNOSTIC COMMAND...
    local what=$1 expected=$2 rc=0
    shift 2
    "$@" >"$RUN/must-fail.out" 2>&1 || rc=$?
    if ((rc == 0 || (rc >= 124 && rc != 128))) || ! grep -Ei -- "$expected" "$RUN/must-fail.out" >/dev/null; then
        echo "Expected refusal: $what (exit $rc; expected $expected)" >&2
        cat "$RUN/must-fail.out" >&2
        return 1
    fi
    echo "refused as expected: $what"
}

# --------------------------------------------------------------- tooling ----

# as WHO COMMAND ARGS...: one call of the HTTP helper with WHO's cookie jar.
as() {
    local who=$1 command=$2
    shift 2
    "$PYTHON" -I "$HELPER" "$command" --base "$BASE" --jar "$RUN/jar-$who.json" --who "$who" "$@"
}

# git_as USER GIT-ARGS...: git with USER's SSH key and identity, bounded in time.
git_as() {
    local user=$1
    shift
    HOME=$RUN/home GIT_CONFIG_GLOBAL=/dev/null GIT_CONFIG_NOSYSTEM=1 GIT_TERMINAL_PROMPT=0 \
        GIT_SSH_VARIANT=ssh \
        GIT_SSH_COMMAND="$SSH -F /dev/null -o BatchMode=yes -o IdentitiesOnly=yes -o IdentityAgent=none -o ConnectTimeout=15 -o LogLevel=ERROR -o StrictHostKeyChecking=accept-new -o UserKnownHostsFile=$RUN/known_hosts -i $RUN/keys/$user" \
        GIT_AUTHOR_NAME=$user GIT_AUTHOR_EMAIL=$user@example.test \
        GIT_COMMITTER_NAME=$user GIT_COMMITTER_EMAIL=$user@example.test \
        timeout 120 git "$@"
}

sha_of() { printf '%s' "$1" | sha256sum | cut -d' ' -f1; }
random_hex() { "$PYTHON" -I -c 'import secrets; print(secrets.token_hex(6))'; }
free_ports() {
    "$PYTHON" -I -c 'import socket, sys
kind = socket.SOCK_DGRAM if sys.argv[2] == "udp" else socket.SOCK_STREAM
s = [socket.socket(socket.AF_INET, kind) for _ in range(int(sys.argv[1]))]
[x.bind(("127.0.0.1", 0)) for x in s]
print(*(x.getsockname()[1] for x in s))
[x.close() for x in s]' "${1:-2}" "${2:-tcp}"
}
pg_tool() {
    if [[ -n ${PG_BIN:-} && -x $PG_BIN/$1 ]]; then echo "$PG_BIN/$1"; else command -v "$1"; fi
}

# Keep passwords out of libpq tools' argv; Python reads the URL from its environment.
pg_command() {
    local url=$1
    shift
    E2E_PG_URL=$url "$PYTHON" -I - "$@" <<'PY'
import os, subprocess, sys, urllib.parse as u
parts = u.urlsplit(os.environ.pop("E2E_PG_URL"))
password = u.unquote(parts.password) if parts.password is not None else None
query = []
for key, value in u.parse_qsl(parts.query, keep_blank_values=True):
    if key == "password":
        password = value
    elif key == "ssl-mode":
        query.append(("sslmode", value))
    elif key != "statement-cache-capacity":
        query.append((key, value))
netloc = parts.netloc
if parts.password is not None:
    user, host = netloc.rsplit("@", 1)
    netloc = user.split(":", 1)[0] + "@" + host
url = parts._replace(netloc=netloc, query=u.urlencode(query)).geturl()
if password is not None:
    os.environ["PGPASSWORD"] = password
sys.exit(subprocess.call([sys.argv[1], sys.argv[2] + "=" + url, *sys.argv[3:]]))
PY
}

# ---------------------------------------------------------------- build -----

check_css() {
    bash "$ROOT/scripts/build-webui.sh" check
}

build_bundle() {
    bash "$ROOT/scripts/build-webui.sh" "$E2E_PROFILE"
    [[ -f $WEBUI_DIST/index.html ]]
}

build_server() {
    cargo build --locked "${CARGO_PROFILE[@]}" -p codeyard --bin codeyard -p codeyard-runner --bin codeyard-actions --message-format=json >"$WORK/cargo-artifacts.json" || return 1
    BIN=$("$PYTHON" -I scripts/cargo-executable.py codeyard <"$WORK/cargo-artifacts.json") || return 1
    ACTIONS_BIN=$("$PYTHON" -I scripts/cargo-executable.py codeyard-actions <"$WORK/cargo-artifacts.json") || return 1
    [[ -x $BIN && -x $ACTIONS_BIN ]]
    ls -l "$BIN" "$ACTIONS_BIN"
}

check_build_artifacts() {
    [[ -x $BIN ]] || { echo "no server binary at $BIN: run without E2E_SKIP_BUILD" >&2; return 1; }
    [[ -x $ACTIONS_BIN ]] || { echo "no native runner at $ACTIONS_BIN: run without E2E_SKIP_BUILD" >&2; return 1; }
    [[ -f $WEBUI_DIST/index.html ]] || { echo "no bundle at $WEBUI_DIST: run without E2E_SKIP_BUILD" >&2; return 1; }
    ls -l "$BIN" "$WEBUI_DIST/index.html"
}

# ------------------------------------------------------- server lifecycle ---

pg_url() { # pg_url NAME: GITSERV_TEST_DATABASE_URL pointed at the database NAME
    E2E_PG_DATABASE="$1" "$PYTHON" -I -c 'import os, urllib.parse as u
parts = u.urlsplit(os.environ["GITSERV_TEST_DATABASE_URL"])
query = [(key, value) for key, value in u.parse_qsl(parts.query, keep_blank_values=True) if key != "dbname"]
print(parts._replace(path="/" + os.environ["E2E_PG_DATABASE"], query=u.urlencode(query)).geturl())'
}

prepare_database() {
    drop_pg || return 1
    DATABASE_URL='' DATABASE_DIRECT_URL=''
    case $ROUND in
        sqlite) echo "SQLite database in the state directory (DATABASE_URL unset)" ;;
        postgres | pgbouncer | replica | cluster | quorum)
            if [[ -z ${GITSERV_TEST_DATABASE_URL:-} ]]; then
                echo "GITSERV_TEST_DATABASE_URL is not set: point it at a PostgreSQL server" >&2
                return 1
            fi
            local createdb
            createdb=$(pg_tool createdb) || { echo "createdb not found" >&2; return 1; }
            local candidate=gitserv_e2e_$(random_hex)
            pg_command "$GITSERV_TEST_DATABASE_URL" "$createdb" --maintenance-db "$candidate" || return 1
            PG_DB=$candidate
            PG_OWNED+=("$PG_DB")
            PG_CURRENT=$PG_DB
            DATABASE_URL=$(pg_url "$PG_DB") || return 1
            echo "PostgreSQL database $PG_DB"
            ;;
        *) echo "unknown database '$ROUND'" >&2; return 1 ;;
    esac
}

start_read_replica() {
    [[ -n ${GITSERV_TEST_POSTGRES_DATA:-} && -f ${GITSERV_TEST_POSTGRES_DATA%/data}/.codeyard-owned-postgres ]] || {
        echo 'Read replica acceptance needs the owned PostgreSQL from builder-test.sh' >&2; return 1;
    }
    local backup ctl port psql settings connections locks
    backup=$(pg_tool pg_basebackup) || return 1
    ctl=$(pg_tool pg_ctl) || return 1
    port=$(free_ports 1) || return 1
    READ_REPLICA_DATA=$RUN/read-replica
    psql=$(pg_tool psql) || return 1
    # builder-test.sh sets these on the primary command line, so basebackup's
    # copied configuration alone is insufficient for hot standby to start.
    settings=$(pg_command "$GITSERV_TEST_DATABASE_URL" "$psql" --dbname -X -At -v ON_ERROR_STOP=1 \
        -c "SELECT current_setting('max_connections') || ' ' || current_setting('max_locks_per_transaction')") || return 1
    read -r connections locks <<<"$settings"
    [[ $connections =~ ^[1-9][0-9]{0,5}$ && $locks =~ ^[1-9][0-9]{0,5}$ ]] || return 1
    pg_command "$GITSERV_TEST_DATABASE_URL" "$backup" --dbname -D "$READ_REPLICA_DATA" -R -X stream --checkpoint=fast || return 1
    "$ctl" -D "$READ_REPLICA_DATA" -l "$RUN/read-replica.log" -w \
        -o "-h 127.0.0.1 -p $port -k $RUN -c hot_standby=on -c max_connections=$connections -c max_locks_per_transaction=$locks -c log_statement=all" start || {
            tail -n 80 "$RUN/read-replica.log"; return 1;
        }
    DATABASE_READ_URL=$(E2E_REPLICA_PORT=$port E2E_PRIMARY_URL=$DATABASE_URL "$PYTHON" -I -c 'import os, urllib.parse as u
p=u.urlsplit(os.environ["E2E_PRIMARY_URL"])
host=p.netloc.rsplit(":",1)[0]
print(p._replace(netloc=host+":"+os.environ["E2E_REPLICA_PORT"]).geturl())')
}

stop_read_replica() {
    [[ -z ${READ_REPLICA_DATA:-} ]] || "$(pg_tool pg_ctl)" -D "$READ_REPLICA_DATA" -m fast -w stop >/dev/null 2>&1 || true
    READ_REPLICA_DATA=''
    DATABASE_READ_URL=''
}

t_read_replica() {
    t_github_token
    E2E_PRIMARY_URL=$DATABASE_URL E2E_REPLICA_URL=$DATABASE_READ_URL E2E_REPLICA_DATA=$READ_REPLICA_DATA \
        "$GITHUB_PYTHON" -I "$GITHUB_HELPER" --base "$BASE" --run "$RUN" --read-replica
}

start_pgbouncer() { # inline: a private transaction pool for this round's database
    local port binary
    binary=$(command -v pgbouncer) || { echo 'pgbouncer not found' >&2; return 1; }
    port=$(free_ports 1) || return 1
    E2E_POOL_UPSTREAM=$DATABASE_URL E2E_POOL_PORT=$port "$PYTHON" -I - "$RUN" <<'PY'
import os, pathlib, secrets, sys, urllib.parse as u
root = pathlib.Path(sys.argv[1]) / 'pgbouncer'
root.mkdir(mode=0o700)
parts = u.urlsplit(os.environ['E2E_POOL_UPSTREAM'])
query = dict(u.parse_qsl(parts.query))
host = query.get('host', parts.hostname or '127.0.0.1')
port = query.get('port', str(parts.port or 5432))
user = query.get('user', u.unquote(parts.username or os.environ.get('USER', '')))
password = query.get('password', u.unquote(parts.password or ''))
database = u.unquote(parts.path.lstrip('/'))
def quote(value):
    if any(c in value for c in '\r\n\x00'):
        raise ValueError('invalid PgBouncer connection setting')
    return "'" + value.replace('\\', '\\\\').replace("'", "\\'") + "'"
sslmode = query.get('sslmode', query.get('ssl-mode', 'prefer'))
sslmode = {'verifyca': 'verify-ca', 'verifyfull': 'verify-full'}.get(sslmode, sslmode)
if sslmode not in ('disable', 'allow', 'prefer', 'require', 'verify-ca', 'verify-full'):
    raise ValueError('unsupported PgBouncer TLS mode')
# Forced backend credentials are private; clients use separate throwaway credentials.
client_password = secrets.token_hex(32)
(root / 'users.txt').write_text(f'"pool_client" "{client_password}"\n')
backend = ' '.join(f'{key}={quote(value)}' for key, value in
                   [('host', host), ('port', port), ('dbname', database),
                    ('user', user), ('password', password)])
mode = os.environ.get('E2E_PGBOUNCER_MODE', 'transaction')
if mode not in ('transaction', 'session'):
    raise SystemExit('E2E_PGBOUNCER_MODE must be transaction or session')
pool_size = 2 if mode == 'transaction' else 96
config = f'''[databases]
fixture = {backend}
[pgbouncer]
listen_addr = 127.0.0.1
listen_port = {int(os.environ['E2E_POOL_PORT'])}
unix_socket_dir =
auth_type = scram-sha-256
auth_file = {root / 'users.txt'}
pool_mode = {mode}
default_pool_size = {pool_size}
max_client_conn = 100
max_prepared_statements = 200
ignore_startup_parameters = extra_float_digits,options
server_tls_sslmode = {sslmode}
server_connect_timeout = 5
client_login_timeout = 10
query_wait_timeout = 30
'''
for option, setting in [('sslrootcert', 'server_tls_ca_file'), ('sslcert', 'server_tls_cert_file'),
                        ('sslkey', 'server_tls_key_file')]:
    if option in query:
        value = query[option]
        quote(value)
        config += f'{setting} = {value}\n'
(root / 'pool.ini').write_text(config)
(root / 'url').write_text(f"postgres://pool_client:{client_password}@127.0.0.1:{int(os.environ['E2E_POOL_PORT'])}/fixture?sslmode=disable")
for path in root.iterdir():
    path.chmod(0o600)
PY
    [[ $? == 0 ]] || return 1
    "$binary" "$RUN/pgbouncer/pool.ini" >"$RUN/pgbouncer/pool.log" 2>&1 &
    PGBOUNCER_PID=$!
    local attempt
    for attempt in $(seq 1 50); do
        kill -0 "$PGBOUNCER_PID" 2>/dev/null || { echo 'PgBouncer exited during startup' >&2; return 1; }
        if (exec 3<>"/dev/tcp/127.0.0.1/$port") 2>/dev/null; then
            # Leases and migrations bypass the pool, as production does.
            DATABASE_DIRECT_URL=$DATABASE_URL
            DATABASE_URL=$(<"$RUN/pgbouncer/url")
            return 0
        fi
        sleep 0.1
    done
    echo 'PgBouncer did not open its listener' >&2
    return 1
}

stop_pgbouncer() {
    [[ -n $PGBOUNCER_PID ]] || return 0
    kill -TERM "$PGBOUNCER_PID" 2>/dev/null || true
    local attempt
    for attempt in $(seq 1 50); do
        kill -0 "$PGBOUNCER_PID" 2>/dev/null || break
        sleep 0.1
    done
    kill -KILL "$PGBOUNCER_PID" 2>/dev/null || true
    wait "$PGBOUNCER_PID" 2>/dev/null || true
    PGBOUNCER_PID=''
}

server_dir() {
    if [[ ${1:-server} == server ]]; then printf '%s\n' "$RUN"; else printf '%s/nodes/%s\n' "$RUN" "$1"; fi
}

is_runner() { [[ $1 == ci1 || $1 == r1 || $1 == r2 ]]; }

# Shared environment for serve and operator commands; runners never get DATABASE_URL.
configure_node() {
    local node=$1 roles=${SERVER_ROLES:-server,runner}
    NODE_DIR=$(server_dir "$node")
    mkdir -p "$NODE_DIR/home" "$NODE_DIR/repos"
    : >"$RUN/.env"
    case $node in
        web1 | web2) roles=web,ssh ;;
        store1 | store2 | store3) roles=storage ;;
        ci1 | r1 | r2) roles=runner ;;
    esac
    NODE_ENV=(
        "PATH=${PG_BIN:+$PG_BIN:}$PATH" "HOME=$NODE_DIR/home" "LANG=C.UTF-8" "NO_COLOR=1" "RUST_LOG=warn,codeyard=info"
        "GITSERV_STATE_DIR=$NODE_DIR/state" "REPOS_PATH=$NODE_DIR/repos" "HOST_KEY_PATH=$NODE_DIR/host_key"
        "HTTP_ADDR=127.0.0.1:${HTTP_PORTS[$node]:-0}" "SSH_ADDR=127.0.0.1:${SSH_PORTS[$node]:-0}" "SSH_MODE=builtin"
        "GITSERV_NODE_ROLES=$roles"
        "PUBLIC_URL=${ACTION_OBJECTS_ORIGIN:-$BASE}" "PUBLIC_HTTP_CLONE_BASE=$BASE" "PUBLIC_SSH_CLONE_BASE=$SSH_URL" "COOKIE_SECURE=false"
        "WEBUI_DIST=$WEBUI_DIST"
        "CI_EXECUTION_MODE=local" "CI_ACT_BINARY=$ACT" "CI_ACTIONS_BINARY=$ACTIONS_BIN"
        "CI_GITHUB_ACTION_FALLBACK=true"
        "CI_MAX_ACTIVE_RUNS=2" "CI_RUN_TIMEOUT_SECONDS=300" "WEBHOOKS_ALLOW_PRIVATE=true"
    )
    if [[ -n ${CI_PROCESS_CGROUP_ROOT:-} ]]; then
        NODE_ENV+=("CI_PROCESS_CGROUP_ROOT=$CI_PROCESS_CGROUP_ROOT")
    fi
    # node_exec clears inherited variables; explicitly retain a requested debug stack
    # budget, just as the integration-test processes do.
    if [[ -n ${RUST_MIN_STACK:-} ]]; then
        NODE_ENV+=("RUST_MIN_STACK=$RUST_MIN_STACK")
    fi
    # The gh sweep's accounts start CI runs and register after the server starts, so this
    # disposable server trusts every account to run CI.
    if [[ ${E2E_GH_SWEEP:-0} == 1 ]]; then NODE_ENV+=("CI_TRUSTED_ACTOR_IDS=$(seq -s, 1 500)"); fi
    if [[ -n $DATABASE_URL ]] && ! is_runner "$node"; then NODE_ENV+=("DATABASE_URL=$DATABASE_URL"); fi
    if ! is_runner "$node"; then NODE_ENV+=("SECRETS_MASTER_KEY=$(get master_key)"); fi
    if [[ -n ${DATABASE_READ_URL:-} ]] && ! is_runner "$node"; then NODE_ENV+=("DATABASE_READ_URL=$DATABASE_READ_URL"); fi
    if [[ -n ${DATABASE_DIRECT_URL:-} ]] && ! is_runner "$node"; then NODE_ENV+=("DATABASE_DIRECT_URL=$DATABASE_DIRECT_URL"); fi
    # A single server opens the node connection only for runners on other processes.
    if [[ $node == server && -n ${SERVER_IROH:-} ]]; then
        NODE_ENV+=("NODE_NAME=server" "IROH_BIND=127.0.0.1:$SERVER_IROH" "IROH_ADVERTISE=127.0.0.1:$SERVER_IROH")
    fi
    if [[ $node != server ]]; then
        NODE_ENV+=("NODE_NAME=$node" "IROH_BIND=127.0.0.1:${IROH_PORTS[$node]}" "IROH_ADVERTISE=127.0.0.1:${IROH_PORTS[$node]}")
        case $node in
            web1) NODE_ENV+=("CLUSTER_NODES=$(get store2_public):storage:store2") ;;
            store2) NODE_ENV+=("NODE_KEY=$(get store2_key)") ;;
            ci1) NODE_ENV+=("CLUSTER_JOIN_TOKEN=$(get worker_token)") ;;
            # One run at a time each, so queued runs spread over both runners.
            r1 | r2) NODE_ENV+=("CLUSTER_JOIN_TOKEN=$(get runner_token)" "CI_MAX_ACTIVE_RUNS=1") ;;
        esac
    fi
}

# Clear the inherited environment, then apply secrets with shell builtins, never external env
# argv. `env -0` lists what a child would inherit, including an exported variable that a
# caller's local of the same name hides, so each name is unset through every scope.
node_exec() {
    local _node_exec_entry
    while IFS= read -r -d '' _node_exec_entry; do
        # Bash sets `_` again after every command; the child gets its own.
        [[ ${_node_exec_entry%%=*} != _ ]] || continue
        while [[ -v ${_node_exec_entry%%=*} ]]; do
            unset -v "${_node_exec_entry%%=*}" 2>/dev/null || break
        done
    done < <(env -0)
    export "${NODE_ENV[@]}"
    exec "$@"
}

operator() (
    local seconds=$1 pid status=0
    shift
    mkdir -p "$WORK/operators"
    (node_exec timeout --kill-after=10 "$seconds" "$BIN" "$@") &
    pid=$!
    printf '%s\n' "$pid" >"$WORK/operators/$BASHPID"
    wait "$pid" || status=$?
    rm -f "$WORK/operators/$BASHPID"
    return "$status"
)

terminate_operators() {
    [[ -d $WORK/operators ]] || return 0
    # Git gives its children separate process groups. Walk descendants before terminating
    # the timeout parent so those groups cannot become untracked orphans.
    "$PYTHON" -I - "$WORK/operators" <<'PY'
import os, pathlib, signal, sys
parents = {}
for entry in pathlib.Path('/proc').iterdir():
    if entry.name.isdigit():
        try:
            fields = (entry / 'stat').read_text().rsplit(')', 1)[1].split()
            parents[int(entry.name)] = int(fields[1])
        except (OSError, ValueError, IndexError):
            pass
def stop(pid):
    for child, parent in list(parents.items()):
        if parent == pid:
            stop(child)
    try:
        os.kill(pid, signal.SIGKILL)
    except ProcessLookupError:
        pass
for record in pathlib.Path(sys.argv[1]).glob('*'):
    stop(int(record.read_text()))
PY
}

# server_cli ARGS...: an operator command with the single server's environment.
server_cli() {
    configure_node server
    (cd "$NODE_DIR" && operator 900 "$@" < /dev/null)
}

cluster_cli() {
    local node=$1
    shift
    configure_node "$node"
    if [[ -n ${CLUSTER_JOIN_TOKEN:-} ]]; then
        NODE_ENV+=("CLUSTER_JOIN_TOKEN=$CLUSTER_JOIN_TOKEN")
    fi
    (cd "$NODE_DIR" && operator 60 cluster "$@" < /dev/null)
}

start_server() { # inline: starts a node and waits for its actual services
    local node=${1:-server}
    [[ -z ${SERVER_PIDS[$node]:-} ]] || { echo "$node is already running" >&2; return 1; }
    if [[ $node == server ]]; then
        if [[ -z $HTTP_PORT ]]; then read -r HTTP_PORT SSH_PORT < <(free_ports); fi
        HTTP_PORTS[$node]=$HTTP_PORT
        SSH_PORTS[$node]=$SSH_PORT
    fi
    if [[ $node == server || $node == web1 ]]; then
        HTTP_PORT=${HTTP_PORTS[$node]}
        SSH_PORT=${SSH_PORTS[$node]}
        BASE=http://127.0.0.1:$HTTP_PORT
        SSH_URL=ssh://git@127.0.0.1:$SSH_PORT
    fi
    configure_node "$node" || return 1
    local dir=$NODE_DIR pid waited=0 ready=0 first_line reported_after publish_deadline
    if [[ $node == store1 || $node == store2 || $node == store3 ]]; then
        reported_after=$(pg_query "SELECT (EXTRACT(EPOCH FROM clock_timestamp()) * 1000)::bigint") || return 1
        [[ $reported_after =~ ^[0-9]+$ ]] || return 1
    fi
    touch "$dir/server.log"
    first_line=$(( $(wc -l <"$dir/server.log") + 1 ))
    echo "starting $node: HTTP ${HTTP_PORTS[$node]}, SSH ${SSH_PORTS[$node]}, iroh ${IROH_PORTS[$node]:-off}"
    if [[ $node == web2 ]]; then
        [[ -f ${CODEYARD_TEST_FAKETIME_LIBRARY:-} ]] || { echo 'The pinned libfaketime library is required' >&2; return 1; }
        # Preload the library directly: the faketime launcher forks, which would
        # hide the server PID from this harness's signal and cleanup ownership.
        NODE_ENV+=("LD_PRELOAD=$CODEYARD_TEST_FAKETIME_LIBRARY" "FAKETIME=+5m" "FAKETIME_DONT_FAKE_MONOTONIC=1")
    fi
    (cd "$dir" && node_exec "$BIN" serve < /dev/null) >>"$dir/server.log" 2>&1 &
    pid=$!
    SERVER_PIDS[$node]=$pid
    while ((waited < 120)); do
        if ! kill -0 "$pid" 2>/dev/null; then
            wait "$pid" || true
            unset 'SERVER_PIDS[$node]'
            echo "$node exited during start-up:" >&2
            tail -n 40 "$dir/server.log" >&2
            return 1
        fi
        case $node in
            server | web1 | web2)
                if curl -q --noproxy '*' -fsS --max-time 2 -o "$dir/health.json" "http://127.0.0.1:${HTTP_PORTS[$node]}/health" 2>/dev/null; then
                    grep -q '"status":"ok"' "$dir/health.json" && ready=1
                fi ;;
            store1 | store2 | store3)
                if curl -q --noproxy '*' -fsS --max-time 2 -o "$dir/health.json" "http://127.0.0.1:${HTTP_PORTS[$node]}/ready" 2>/dev/null; then
                    grep -q '"status":"ready"' "$dir/health.json" && ready=1
                fi ;;
            ci1 | r1 | r2) tail -n +"$first_line" "$dir/server.log" | grep 'database-free runner ready' >/dev/null && ready=1 ;;
        esac
        ((ready)) && break
        waited=$((waited + 1))
        sleep 0.5
    done
    ((ready)) || { echo "$node did not start after 60 s" >&2; tail -n 40 "$dir/server.log" >&2; return 1; }
    if [[ $node == store1 || $node == store2 || $node == store3 ]]; then
        # /ready can answer before publication finishes reconciliation. A pre-crash
        # heartbeat must not admit an operation against a process still starting.
        publish_deadline=$((SECONDS + 60))
        until [[ $(pg_query "SELECT last_seen>=to_timestamp($reported_after::double precision/1000) FROM nodes WHERE name='$node'") == t ]]; do
            if ! kill -0 "$pid" 2>/dev/null || ((SECONDS >= publish_deadline)); then
                echo "$node did not publish a fresh heartbeat after startup" >&2
                tail -n 40 "$dir/server.log" >&2
                return 1
            fi
            sleep 0.5
        done
    fi
    if [[ $node == server || $node == web1 ]]; then
        waited=0
        until (exec 3<>"/dev/tcp/127.0.0.1/$SSH_PORT") 2>/dev/null; do
            waited=$((waited + 1))
            if ((waited > 60)); then echo "the SSH port never opened" >&2; return 1; fi
            sleep 0.5
        done
        echo "/health: $(cat "$dir/health.json")"
    fi
}

stop_server() { # inline: SIGTERM, wait, and require exit status 0
    local node=${1:-server} dir pid waited=0 rc=0 announced
    dir=$(server_dir "$node")
    pid=${SERVER_PIDS[$node]:-}
    [[ -n $pid ]] || { echo "$node is not running" >&2; return 1; }
    announced=$(grep -c 'shutting down' "$dir/server.log" || true)
    kill -TERM "$pid" || return 1
    while kill -0 "$pid" 2>/dev/null; do
        sleep 0.2
        waited=$((waited + 1))
        if ((waited > 475)); then
            echo "$node was still running 95 s after SIGTERM" >&2
            kill -KILL "$pid" 2>/dev/null
            wait "$pid" 2>/dev/null
            unset 'SERVER_PIDS[$node]'
            return 1
        fi
    done
    wait "$pid" || rc=$?
    unset 'SERVER_PIDS[$node]'
    echo "$node exited with status $rc about $((waited / 5)) s after SIGTERM"
    ((rc == 0)) || return 1
    (($(grep -c 'shutting down' "$dir/server.log" || true) > announced)) || {
        echo "the log has no new 'shutting down' line" >&2
        return 1
    }
    local port
    for port in "${HTTP_PORTS[$node]}" "${SSH_PORTS[$node]}"; do
        if (exec 3<>"/dev/tcp/127.0.0.1/$port") 2>/dev/null; then
            echo "$node still has port $port open after exit" >&2
            return 1
        fi
    done
}

terminate_servers() { # also used after a failed round, before dropping its database
    local pid alive
    for pid in "${SERVER_PIDS[@]}"; do kill -TERM "$pid" 2>/dev/null || true; done
    for _ in $(seq 1 100); do
        alive=0
        for pid in "${SERVER_PIDS[@]}"; do kill -0 "$pid" 2>/dev/null && alive=1; done
        ((alive)) || break
        sleep 0.1
    done
    for pid in "${SERVER_PIDS[@]}"; do
        kill -KILL "$pid" 2>/dev/null || true
        wait "$pid" 2>/dev/null || true
    done
    SERVER_PIDS=()
}

# ------------------------------------------------------------------ steps ---

t_keys() { # throwaway key pairs for the users and one nobody registered
    local user
    for user in alice bob rogue; do
        "$SSH_KEYGEN" -q -t ed25519 -N '' -C "e2e-$user" -f "$RUN/keys/$user"
    done
}

t_setup() {
    # Before setup every page leads to the native setup form.
    as anon req GET / --status 303 --location /setup
    as anon req GET /login --status 303 --location /setup
    as anon req GET /setup --accept model --json-eq page.model.Auth.mode=Setup
    # The real form with the CSRF token it carries; no JavaScript involved.
    as alice form /setup --action /setup --has node_mode \
        --set username=alice --set email=alice@example.test \
        --set "password=$PW_ALICE" --set "confirm_password=$PW_ALICE" --set node_mode=public \
        --status 303 --location /
    as alice req GET / --contains 'Account menu for alice'
    as alice req GET /settings --accept model \
        --json-eq chrome.username=alice --json-eq chrome.is_admin=true
    # Setup does not run twice.
    as anon req GET /setup --status 409 --contains 'already complete'
}

t_users() {
    # Sign up through the register form: the new account is signed in at once.
    as bob form /register --action /register --has username \
        --set username=bob --set email=bob@example.test \
        --set "password=$PW_BOB" --set "confirm_password=$PW_BOB" --location /
    as bob req GET / --contains 'Account menu for bob'
    as bob req GET /settings --accept model --json-eq chrome.username=bob --json-eq chrome.is_admin=false
    # Refusals redisplay the form with what was typed.
    as dup form /register --action /register --has username \
        --set username=bob --set "password=$PW_BOB" --set "confirm_password=$PW_BOB" \
        --status 409 --field-eq username=bob
    as dup form /register --action /register --has username \
        --set username=carol --set "password=$PW_BOB" --set confirm_password=another-password \
        --status 400 --contains 'Passwords do not match' --field-eq username=carol
    # Sign out through the form, then in again through the login form.
    as bob form /logout --action /logout --location /login
    as bob req GET / --lacks 'Account menu for bob'
    as bob req GET /login --accept model --json-eq chrome.username=null
    as bob form /login --action /login --has username \
        --set username=bob --set password=not-the-password \
        --status 401 --contains 'Invalid username or password' --field-eq username=bob
    as bob form /login --action /login --has username \
        --set username=bob --set "password=$PW_BOB" --location /
    as bob req GET / --contains 'Account menu for bob'
    # Hydrated navigation reads the same signed-in presentation model.
    as bob req GET /settings --accept model --json-eq chrome.username=bob
}

t_csrf() {
    local zeros
    zeros=$(printf '0%.0s' $(seq 1 64))
    as alice form-get '/settings?tab=profile' --action /settings/profile --has display_name \
        --save "$RUN/profile-form.json"
    # A forged token, and a good token sent from another origin, are refused.
    as alice form-post --from "$RUN/profile-form.json" --set "csrf_token=$zeros" \
        --status 403 --contains 'form expired'
    as alice form-post --from "$RUN/profile-form.json" --origin http://evil.example \
        --status 403 --contains 'form expired'
    # The same form from the right origin saves.
    as alice form-post --from "$RUN/profile-form.json" --origin "$BASE" \
        --set 'display_name=Alice E2E' --set 'bio=Written by the end-to-end check' \
        --location '/settings?tab=profile&saved=profile'
    as anon req GET /alice --contains 'Alice E2E' --contains 'Written by the end-to-end check'
}

t_org() {
    local id
    as alice form /settings/organizations --action /settings/organizations --has action=create \
        --set slug=e2e-org --set 'display_name=E2E Org' \
        --set 'description=Organization made by the end-to-end check' \
        --location /settings/organizations
    id=$(as alice req GET /settings/organizations --contains 'E2E Org' \
        --print 're:href="/settings/organizations/([0-9]+)"')
    put org_id "$id"
    as alice req GET "/settings/organizations/$id" \
        --contains 'E2E Org' --contains 'Organization made by the end-to-end check'
    # A user outside the organization cannot see it.
    as bob req GET "/settings/organizations/$id" --status 404
}

t_repos() {
    # Personal public repository through the /new form.
    as alice form /new --action /new --has name \
        --set name=proj --set 'description=End to end project' --set visibility=public \
        --location /alice/proj
    as alice req GET /alice/proj --contains 'This repository is empty' \
        --contains 'End to end project' --contains "$BASE/alice/proj.git"
    # Private and CI repositories through the JSON API.
    as alice req POST /api/repos --accept json --status 201 \
        --json '{"name":"vault","description":"Private repository","is_private":true}' \
        --json-eq full_name=alice/vault --json-eq is_private=true
    as alice req POST /api/repos --accept json --status 201 \
        --json '{"name":"ci-demo","description":"Repository for the CI check"}' \
        --json-eq full_name=alice/ci-demo --json-eq is_private=false
    as alice req POST /api/repos --accept json --status 409 --json '{"name":"proj"}'
    # Visibility: strangers cannot even tell the private repository exists.
    as anon req GET /alice/vault --status 404
    as bob req GET /alice/vault --status 404
    as anon req GET /api/repos/alice/vault --accept json --status 404
    as alice req GET /alice/vault --contains 'This repository is empty'
    as bob req GET /alice/proj --contains 'End to end project'
}

t_org_repo() {
    local org_id
    org_id=$(get org_id)
    # The owner of a new repository is picked in the same /new form.
    as alice req GET /new --contains 'e2e-org (organization)'
    as alice form /new --action /new --has name \
        --set name=widgets --set "owner_org_id=$org_id" \
        --set 'description=Owned by the organization' --set visibility=public \
        --location /e2e-org/widgets
    as alice req GET /e2e-org/widgets --contains 'Owned by the organization'
    as bob req GET /e2e-org/widgets --contains 'Owned by the organization'
    as alice req GET /api/repos/e2e-org/widgets --accept json \
        --json-eq full_name=e2e-org/widgets --json-eq owner=e2e-org
}

t_ssh_keys() {
    local user public fingerprint
    for user in alice bob; do
        public=$(cat "$RUN/keys/$user.pub")
        fingerprint=$("$SSH_KEYGEN" -lf "$RUN/keys/$user.pub" | awk '{print $2}')
        as "$user" form '/settings?tab=keys' --action /settings/keys --has public_key \
            --set "name=e2e $user key" --set "public_key=$public" \
            --location '/settings?tab=keys&saved=key'
        as "$user" req GET '/settings?tab=keys&saved=key' \
            --contains "e2e $user key" --contains "$fingerprint" --contains 'SSH key added'
    done
    # A private key pasted by mistake is refused and never echoed back.
    as alice form '/settings?tab=keys' --action /settings/keys --has public_key \
        --set name=oops \
        --set $'public_key=-----BEGIN OPENSSH PRIVATE KEY-----\nAAAA\n-----END OPENSSH PRIVATE KEY-----' \
        --status 400 --contains 'never a private key' --lacks 'BEGIN OPENSSH PRIVATE KEY'
}

t_tokens() {
    local read write
    read=$(as alice form '/settings?tab=tokens' --action /settings/tokens --has name \
        --set 'name=e2e read' --set scope=read --set expiry=never --status 200 \
        --contains 'Copy this token now' --print 're:(?s)Copy this token now.*?(cy_pat_(?:[0-9]{1,5}_)?[0-9a-f]{64})')
    write=$(as alice form '/settings?tab=tokens' --action /settings/tokens --has name \
        --set 'name=e2e write' --set scope=write --set expiry=never --status 200 \
        --contains 'Copy this token now' --print 're:(?s)Copy this token now.*?(cy_pat_(?:[0-9]{1,5}_)?[0-9a-f]{64})')
    put read_token "$read"
    put write_token "$write"
    as alice req GET '/settings?tab=tokens' --contains 'e2e read' --contains 'e2e write' \
        --lacks "$read" --lacks "$write"
}

git_with_token() {
    local scope=$1 auth
    shift
    auth=$(printf 'any-username:%s' "$(get "${scope}_token")" | base64 -w0)
    git_as alice -c "http.extraHeader=Authorization: Basic $auth" -c http.postBuffer=1024 "$@"
}

t_push() {
    local work=$RUN/work/proj sha
    rm -rf "$work"
    mkdir -p "$work/src"
    cd "$work"
    git_as alice init -q -b main
    printf '# proj\n\nHello from the end-to-end check.\n' >README.md
    printf 'fn main() {}\n' >src/main.rs
    head -c 2097152 /dev/urandom >blob.bin
    put blob_sha "$(sha256sum blob.bin | cut -d' ' -f1)"
    git_as alice add -A
    git_as alice commit -q -m 'Initial commit'
    git_as alice remote add origin "$SSH_URL/alice/proj.git"
    git_as alice push -q origin main
    sha=$(git_as alice rev-parse HEAD)
    put main_sha "$sha"
    # The remote has it, over SSH and over the public HTTP URL.
    [[ $(git_as alice ls-remote origin refs/heads/main | cut -f1) == "$sha" ]]
    [[ $(git_as alice ls-remote "$BASE/alice/proj.git" refs/heads/main | cut -f1) == "$sha" ]]
    # The web UI and the API show what was pushed.
    as alice req GET /alice/proj --contains README.md --contains 'Hello from the end-to-end check.' \
        --contains 'Initial commit'
    as alice req GET /alice/proj/commits --contains 'Initial commit'
    as alice req GET /api/repos/alice/proj/commits --accept json \
        --json-eq "0.sha=$sha" --json-has '0.message=Initial commit'
    as alice req GET /api/repos/alice/proj/branches --accept json --json-eq 0.name=main
}

t_git_streaming() {
    local source=$RUN/work/stream-source transport name url auth a b large expected node peak growth
    local nodes=(server)
    local -A before=()
    [[ $ROUND == cluster ]] && nodes=(web1 store1 store2)
    # Measure what the transfers add: a debug server already holds about 240 MiB after the
    # earlier steps. Writing 5 to clear_refs resets the peak (VmHWM) to the current size.
    for node in "${nodes[@]}"; do
        echo 5 >"/proc/${SERVER_PIDS[$node]}/clear_refs"
        before[$node]=$(awk '/^VmHWM:/ {print $2}' "/proc/${SERVER_PIDS[$node]}/status")
    done
    mkdir -p "$source"
    git_as alice -C "$source" init -q -b main
    printf 'streaming\n' >"$source/README"
    git_as alice -C "$source" add README
    git_as alice -C "$source" commit -qm Initial
    for i in $(seq 1 125); do git_as alice -C "$source" commit --allow-empty -qm "History $i" < /dev/null; done
    "$PYTHON" -I - "$RUN/random.bin" <<'PY'
import random, sys
rng = random.Random(42)
with open(sys.argv[1], 'wb') as file:
    for _ in range(150):
        file.write(rng.randbytes(1024 * 1024))
PY
    for transport in ssh http; do
        name=stream-$transport
        as alice req POST /api/repos --accept json --status 201 --json "{\"name\":\"$name\",\"is_private\":true}"
        url=$SSH_URL/alice/$name.git
        [[ $transport == http ]] && url=$BASE/alice/$name.git
        # Supplying the token is harmless for SSH and keeps both paths identical.
        git_with_token write -C "$source" push -q "$url" main < /dev/null
        a=$RUN/stream-$transport-a b=$RUN/stream-$transport-b large=$RUN/stream-$transport-large
        git_with_token read clone -q "$url" "$a" < /dev/null
        git_with_token read clone -q "$url" "$b" < /dev/null
        for i in $(seq 1 3); do git_as alice -C "$b" commit --allow-empty -qm "First B $i" < /dev/null; done
        git_with_token write -C "$b" push -q origin main < /dev/null
        git_with_token read -C "$a" fetch -q origin < /dev/null
        [[ $(git_as alice -C "$a" rev-parse origin/main) == "$(git_as alice -C "$b" rev-parse HEAD)" ]]
        for i in $(seq 1 40); do git_as alice -C "$a" commit --allow-empty -qm "Ahead A $i" < /dev/null; done
        for i in $(seq 1 3); do git_as alice -C "$b" commit --allow-empty -qm "Second B $i" < /dev/null; done
        git_with_token write -C "$b" push -q origin main < /dev/null
        git_with_token read -C "$a" fetch -q origin < /dev/null
        [[ $(git_as alice -C "$a" rev-parse origin/main) == "$(git_as alice -C "$b" rev-parse HEAD)" ]]
        cp "$RUN/random.bin" "$b/random.bin"
        git_as alice -C "$b" add random.bin < /dev/null
        git_as alice -C "$b" commit -qm 'Large pack' < /dev/null
        git_with_token write -C "$b" push -q origin main < /dev/null
        git_with_token read clone -q "$url" "$large" < /dev/null
        expected=$(git_as alice -C "$b" rev-parse 'HEAD^{tree}')
        [[ $(git_as alice -C "$large" rev-parse 'HEAD^{tree}') == "$expected" ]]
        find "$large/.git/objects/pack" -name '*.pack' -size +128M | grep -q .
    done
    # Buffering one 150 MiB pack would add more than 150 MiB; streaming adds a few.
    for node in "${nodes[@]}"; do
        peak=$(awk '/^VmHWM:/ {print $2}' "/proc/${SERVER_PIDS[$node]}/status")
        [[ -n $peak && -n ${before[$node]} ]] || { echo "$node peak memory missing" >&2; return 1; }
        growth=$((peak - before[$node]))
        ((growth < 65536)) || { echo "$node memory grew $growth KiB during Git transfers, to $peak KiB" >&2; return 1; }
        echo "$node memory grew $growth KiB during Git transfers, to $peak KiB"
    done
}

t_clone() {
    local sha vault=$RUN/work/vault
    sha=$(get main_sha)
    rm -rf "$RUN"/clone-*
    git_as alice clone -q "$SSH_URL/alice/proj.git" "$RUN/clone-ssh"
    git_as alice clone -q "$BASE/alice/proj.git" "$RUN/clone-http"
    for kind in ssh http; do
        [[ $(git_as alice -C "$RUN/clone-$kind" rev-parse HEAD) == "$sha" ]]
        grep -q 'Hello from the end-to-end check.' "$RUN/clone-$kind/README.md"
        [[ $(sha256sum "$RUN/clone-$kind/blob.bin" | cut -d' ' -f1) == "$(get blob_sha)" ]]
    done
    [[ $(git_as alice -C "$RUN/clone-ssh" ls-tree -r HEAD | sha256sum) == "$(git_as alice -C "$RUN/clone-http" ls-tree -r HEAD | sha256sum)" ]]
    # The private repository: its owner pushes and clones over SSH.
    rm -rf "$vault"
    mkdir -p "$vault"
    cd "$vault"
    git_as alice init -q -b main
    printf 'top secret\n' >secret.txt
    git_as alice add -A
    git_as alice commit -q -m 'Secret'
    git_as alice remote add origin "$SSH_URL/alice/vault.git"
    git_as alice push -q origin main
    git_as alice clone -q "$SSH_URL/alice/vault.git" "$RUN/clone-vault"
    grep -q 'top secret' "$RUN/clone-vault/secret.txt"
    # Anonymous Git clients receive a Basic challenge; page/raw access stays hidden.
    must_fail 'anonymous HTTP clone of the private repository' 'could not read Username|Authentication failed|401' \
        git_as alice clone -q "$BASE/alice/vault.git" "$RUN/clone-vault-http"
    [[ ! -e $RUN/clone-vault-http/secret.txt ]]
    as anon req GET '/alice/vault.git/info/refs?service=git-upload-pack' --status 401
    as anon req GET /alice/vault/raw/main/secret.txt --status 404
    as bob req GET /alice/vault/raw/main/secret.txt --status 404
    # Private clone with a read token, and a large chunked push with a write token.
    as anon req GET '/alice/proj.git/info/refs?service=git-receive-pack' --status 401
    git_with_token read clone -q "$BASE/alice/vault.git" "$RUN/clone-token"
    cd "$RUN/clone-token"
    grep -q 'top secret' secret.txt
    head -c 3145728 /dev/urandom >http-blob.bin
    git_as alice add -A
    git_as alice commit -q -m 'Push over HTTP'
    must_fail 'push with a read-only token' '403' git_with_token read push -q origin main
    git_with_token write push -q origin main
    [[ $(git_with_token read ls-remote origin refs/heads/main | cut -f1) == "$(git_as alice rev-parse HEAD)" ]]
    local log
    local logs=("$RUN/server.log")
    if [[ $ROUND == cluster ]]; then logs=("$RUN"/nodes/*/server.log); fi
    for log in "${logs[@]}"; do
        [[ -f $log ]]
        ! grep -Fq "$(get read_token)" "$log"
        ! grep -Fq "$(get write_token)" "$log"
    done
    # Another user reads the public repository over SSH but not the private one,
    # and cannot push.
    git_as bob clone -q "$SSH_URL/alice/proj.git" "$RUN/clone-bob"
    must_fail "bob's SSH clone of the private repository" 'Repository not found' \
        git_as bob clone -q "$SSH_URL/alice/vault.git" "$RUN/clone-bob-vault"
    cd "$RUN/clone-bob"
    printf 'bob was here\n' >bob.txt
    git_as bob add -A
    git_as bob commit -q -m 'Bob writes'
    must_fail "bob's push to alice/proj" 'Permission denied: write access required' git_as bob push -q origin main
    # A key nobody registered is refused during authentication.
    must_fail 'unregistered SSH key' 'Permission denied \(publickey\)' git_as rogue ls-remote "$SSH_URL/alice/proj.git"
}

t_ci_push() {
    local work=$RUN/work/ci-demo
    # Keep the original act coverage; the real github-script repository uses the native engine.
    as alice form '/alice/ci-demo/settings?tab=actions' --action /alice/ci-demo/_forms/actions-settings \
        --has engine --set engine=act --location '/alice/ci-demo/settings?tab=actions'
    rm -rf "$work"
    mkdir -p "$work/.github/workflows"
    cd "$work"
    git_as alice init -q -b main
    printf 'CI demo\n' >README.md
    if [[ $# -gt 0 ]]; then
        cp "$1" .github/workflows/ci.yml
    else
        cat >.github/workflows/ci.yml <<'YAML'
name: E2E CI
on: [push]
jobs:
  hello:
    runs-on: self-hosted
    steps:
      - run: echo hello
      - run: echo "context ${{ github.ref }} ${{ github.repository }} ${{ github.actor }}"
YAML
    fi
    git_as alice add -A
    git_as alice commit -q -m 'Add the CI workflow'
    git_as alice remote add origin "$BASE/alice/ci-demo.git"
    git_with_token write push -q origin main
    put ci_sha "$(git_as alice rev-parse HEAD)"
}

t_editor() {
    local main0 saved newtext stale guide tip
    main0=$(get main_sha)
    # The editor loads the file and the revision it was read at.
    as alice req GET /alice/proj/edit/main/README.md \
        --field-eq "expected_revision=$main0" --field-eq $'content=# proj\n\nHello from the end-to-end check.\n' \
        --field-eq creating=no --field-eq branch=main --field-eq path=README.md
    # Two browser tabs open the same file.
    as alice form-get /alice/proj/edit/main/README.md --action /alice/proj/_forms/write-file \
        --has path --save "$RUN/tab-a.json"
    as alice form-get /alice/proj/edit/main/README.md --action /alice/proj/_forms/write-file \
        --has path --save "$RUN/tab-b.json"
    # Tab A saves through the plain HTML form.
    newtext=$'# proj\n\nEdited in the web editor: <b>é ☃</b> & done.\n'
    as alice form-post --from "$RUN/tab-a.json" --submit intent=save \
        --set "content=$newtext" --set 'message=Edit README in the web editor' \
        --location /alice/proj/blob/refs/heads/main/README.md
    as alice req GET /alice/proj/raw/main/README.md --body-sha256 "$(sha_of "$newtext")"
    saved=$(as alice req GET /api/repos/alice/proj/commits --accept json \
        --json-has '0.message=Edit README in the web editor' --json-eq "0.parents.0=$main0" \
        --print json:0.sha)
    # Tab B still holds the old revision: the save is refused, the draft kept.
    stale=$'# proj\n\nA stale draft that must not be lost.\n'
    as alice form-post --from "$RUN/tab-b.json" --submit intent=save \
        --set "content=$stale" --set 'message=Stale edit' \
        --status 409 --contains 'The branch changed since this editor was loaded' \
        --field-eq "content=$stale" --field-eq "expected_revision=$main0"
    as alice req GET /alice/proj/raw/main/README.md --body-sha256 "$(sha_of "$newtext")"
    tip=$(as alice req GET /api/repos/alice/proj/commits --accept json --print json:0.sha)
    [[ $tip == "$saved" ]]
    # A new file through the same form.
    guide=$'# Guide\n\nCreated in the web editor.\n'
    as alice form /alice/proj/new/main --action /alice/proj/_forms/write-file --has creating=yes \
        --set path=docs/guide.md --set "content=$guide" --set 'message=Add docs/guide.md' \
        --submit intent=save --location /alice/proj/blob/refs/heads/main/docs/guide.md
    as alice req GET /alice/proj/raw/main/docs/guide.md --body-sha256 "$(sha_of "$guide")"
    as alice req GET /alice/proj/blob/main/docs/guide.md --contains 'Created in the web editor.'
}

t_pull_request() {
    local origin tip head pr merged readme
    origin=$(get main_sha)
    tip=$(as alice req GET /api/repos/alice/proj/commits --accept json --print json:0.sha)
    [[ $tip != "$origin" ]]
    # A branch cut from the first commit, after main has moved on, with one new
    # file: merging it takes a real two-parent merge commit.
    as alice req POST /api/repos/alice/proj/branches --accept json \
        --json "{\"name\":\"feature\",\"sha\":\"$origin\"}" --json-eq name=feature --json-eq "sha=$origin"
    head=$(as alice req POST /api/repos/alice/proj/contents/feature.txt --accept json --status 201 \
        --json "{\"content\":\"$(printf 'feature work\n' | base64 -w0)\",\"message\":\"Add feature.txt\",\"branch\":\"feature\",\"expected_revision\":\"$origin\"}" \
        --print json:sha)
    pr=$(as alice req POST /api/repos/alice/proj/pulls --accept json --status 201 \
        --json '{"title":"Add feature.txt","body":"Opened by the end-to-end check","head_branch":"feature","base_branch":"main"}' \
        --json-eq state=open --json-eq "head_sha=$head" --json-eq "base_sha=$tip" \
        --print json:number)
    [[ $pr =~ ^[0-9]+$ ]]
    put pr_number "$pr"
    as bob req GET "/alice/proj/pulls/$pr" --contains 'Add feature.txt'
    as alice req GET "/alice/proj/pulls/$pr/diff" --contains feature.txt
    # Someone without write access cannot merge.
    as bob req POST "/api/repos/alice/proj/pulls/$pr/merge" --status 403,404 --accept json \
        --json "{\"expected_head\":\"$head\",\"expected_base\":\"$tip\"}"
    # Merge through the API; the base branch moves to a merge commit of both.
    readme=$(as alice req GET /alice/proj/raw/main/README.md --print sha256)
    merged=$(as alice req POST "/api/repos/alice/proj/pulls/$pr/merge" --accept json \
        --json "{\"expected_head\":\"$head\",\"expected_base\":\"$tip\"}" \
        --json-eq merged=true --print json:sha)
    [[ $(git_as alice ls-remote "$BASE/alice/proj.git" refs/heads/main | cut -f1) == "$merged" ]]
    [[ $merged != "$tip" && $merged != "$head" ]]
    as alice req GET /api/repos/alice/proj/commits --accept json \
        --json-eq "0.sha=$merged" --json-eq "0.parents.0=$tip" --json-eq "0.parents.1=$head" \
        --json-has "0.message=Merge pull request #$pr from feature"
    as alice req GET "/api/repos/alice/proj/pulls/$pr" --accept json --json-eq state=merged
    as alice req GET /alice/proj/raw/main/feature.txt --body-sha256 "$(sha_of $'feature work\n')"
    as alice req GET /alice/proj/raw/main/README.md --body-sha256 "$readme"
    as alice req GET "/alice/proj/pulls/$pr" --contains 'Add feature.txt'
    as alice req GET /alice/proj/commits --contains "Merge pull request #$pr"
    # Merging again is refused.
    as alice req POST "/api/repos/alice/proj/pulls/$pr/merge" --accept json --status 400,409 \
        --json "{\"expected_head\":\"$head\",\"expected_base\":\"$tip\"}"
}

t_fork_pull_request() {
    local fork tip head pr
    tip=$(tip_of main)
    fork=$(as bob req POST /api/repos/alice/proj/forks --accept json --status 201 --print json:repo.id)
    if [[ $ROUND == cluster ]]; then
        [[ $(repo_node bob proj) == store1 ]]
        [[ $(repo_node bob proj) == "$(repo_node alice proj)" ]]
        echo 'fork bob/proj is stored next to alice/proj on store1'
    fi
    head=$(as bob req POST /api/repos/bob/proj/contents/from-fork.txt --accept json --status 201 \
        --json "{\"content\":\"$(printf 'fork contribution\n' | base64 -w0)\",\"message\":\"Add fork contribution\",\"branch\":\"main\",\"expected_revision\":\"$tip\"}" --print json:sha)
    pr=$(as bob req POST /api/repos/alice/proj/pulls --accept json --status 201 \
        --json "{\"title\":\"Fork contribution\",\"head_repo_id\":$fork,\"head_branch\":\"main\",\"base_branch\":\"main\"}" \
        --json-eq "head_sha=$head" --print json:number)
    as alice req GET "/alice/proj/pulls/$pr" --contains 'bob/proj:main'
    as alice req GET "/api/repos/alice/proj/pulls/$pr/diff" --accept json --contains-raw from-fork.txt
    as alice req POST "/api/repos/alice/proj/pulls/$pr/merge" --accept json \
        --json "{\"expected_head\":\"$head\",\"expected_base\":\"$tip\"}" --json-eq merged=true
    as alice req GET /alice/proj/raw/main/from-fork.txt --body-sha256 "$(sha_of $'fork contribution\n')"
}

tip_of() { as alice req GET "/api/repos/alice/proj/commits?ref=$1" --accept json --print json:0.sha; }

# open_pull TOPIC FILE [BASE]: branch TOPIC off BASE's (default main) tip with one new
# file and open a pull request into BASE; prints "tip head number"
open_pull() {
    local topic=$1 file=$2 base=${3:-main} tip head number
    tip=$(tip_of "$base")
    as alice req POST /api/repos/alice/proj/branches --accept json \
        --json "{\"name\":\"$topic\",\"sha\":\"$tip\"}" >&2
    head=$(as alice req POST "/api/repos/alice/proj/contents/$file" --accept json --status 201 \
        --json "{\"content\":\"$(printf '%s\n' "$file" | base64 -w0)\",\"message\":\"Add $file\",\"branch\":\"$topic\",\"expected_revision\":\"$tip\"}" \
        --print json:sha)
    number=$(as alice req POST /api/repos/alice/proj/pulls --accept json --status 201 \
        --json "{\"title\":\"Add $file\",\"head_branch\":\"$topic\",\"base_branch\":\"$base\"}" \
        --print json:number)
    echo "$tip $head $number"
}

# expect_eq WHAT GOT WANT: passes when the two values are equal, otherwise names both
expect_eq() {
    [[ $2 == "$3" ]] || { echo "$1: got '$2', expected '$3'" >&2; return 1; }
}

# commit_file BRANCH FILE PARENT: one more commit on BRANCH after PARENT, adding FILE (its
# content is its name); prints the new commit
commit_file() {
    as alice req POST "/api/repos/alice/proj/contents/$2" --accept json --status 201 \
        --json "{\"content\":\"$(printf '%s\n' "$2" | base64 -w0)\",\"message\":\"Add $2\",\"branch\":\"$1\",\"expected_revision\":\"$3\"}" \
        --print json:sha
}

t_protection() {
    local base opened tip head pr merged rewritten
    base=$(tip_of main)
    as alice req POST /api/repos/alice/proj/branches --accept json \
        --json "{\"name\":\"protected\",\"sha\":\"$base\"}" --json-eq name=protected
    as alice req PUT /api/repos/alice/proj/branch-protection --accept json \
        --json '{"pattern":"protected","require_pull_request":true,"required_approving_reviews":1}' \
        --json-eq pattern=protected --json-eq required_approving_reviews=1
    as alice req GET /api/repos/alice/proj/branch-protection --accept json --json-eq 0.pattern=protected
    # Direct changes are refused: git push, the web editor and the contents API.
    cd "$RUN/clone-ssh"
    git_as alice fetch -q origin
    git_as alice checkout -q -b protected origin/protected
    printf 'direct\n' >direct.txt
    git_as alice add -A
    git_as alice commit -q -m 'Direct change'
    must_fail 'direct push to the protected branch' 'pre-receive hook declined' git_as alice push -q origin protected
    if [[ $ROUND == cluster ]]; then
        must_fail 'HTTP push to the protected branch' 'pre-receive hook declined' git_with_token write push -q "$BASE/alice/proj.git" protected
        grep -q 'pre-receive hook declined' "$RUN/must-fail.out"
    fi
    [[ $(git_as alice ls-remote origin refs/heads/protected | cut -f1) == "$base" ]]
    # The editor cannot commit to the protected branch; it offers a new branch, which opens the compare page.
    as alice form /alice/proj/edit/protected/README.md --action /alice/proj/_forms/write-file --has path \
        --set 'message=Edit the protected branch' --submit intent=save \
        --location /alice/proj/compare/protected...alice-patch-1
    as alice req POST /api/repos/alice/proj/contents/direct.txt --accept json --status 403 \
        --json "{\"content\":\"$(printf 'x\n' | base64 -w0)\",\"message\":\"Direct\",\"branch\":\"protected\",\"expected_revision\":\"$base\"}"
    [[ $(tip_of protected) == "$base" ]]
    # A pull request is the way in, and it needs an approving review from a writer.
    opened=$(open_pull topic-protected topic-protected.txt protected)
    read -r tip head pr <<<"$opened"
    [[ $tip == "$base" ]]
    as alice req POST "/api/repos/alice/proj/pulls/$pr/merge" --accept json --status 409 \
        --json "{\"expected_head\":\"$head\",\"expected_base\":\"$tip\"}" \
        --contains-raw 'approving review'
    # Public readers may review, but their approval cannot satisfy this writer rule.
    as bob req POST "/api/repos/alice/proj/pulls/$pr/reviews" --accept json --status 200,201 \
        --json "{\"state\":\"approved\",\"commit_sha\":\"$head\"}"
    as alice req POST "/api/repos/alice/proj/pulls/$pr/merge" --accept json --status 409 \
        --json "{\"expected_head\":\"$head\",\"expected_base\":\"$tip\"}" \
        --contains-raw 'approving review'
    as alice req POST /api/repos/alice/proj/collaborators --accept json --status 200,201 \
        --json '{"username":"bob","access_level":"write"}'
    as bob req POST "/api/repos/alice/proj/pulls/$pr/reviews" --accept json --status 200,201 \
        --json "{\"state\":\"approved\",\"commit_sha\":\"$head\"}"
    merged=$(as alice req POST "/api/repos/alice/proj/pulls/$pr/merge" --accept json \
        --json "{\"expected_head\":\"$head\",\"expected_base\":\"$tip\"}" \
        --json-eq merged=true --print json:sha)
    [[ $(tip_of protected) == "$merged" ]]
    as alice req GET /alice/proj/raw/protected/topic-protected.txt --body-sha256 "$(sha_of $'topic-protected.txt\n')"
    # A rule that blocks force pushes refuses the owner too. Listing alice in force_push_allowances
    # lets her through. Main is put back and the rule removed afterwards.
    git_as alice checkout -q --orphan rewritten-main
    git_as alice commit -q --allow-empty -m 'Rewrite the default branch'
    rewritten=$(git_as alice rev-parse HEAD)
    as alice req PUT /api/repos/alice/proj/branch-protection --accept json \
        --json '{"pattern":"main","require_pull_request":false}' --json-eq pattern=main
    must_fail 'force push past a rule that blocks it' 'pre-receive hook declined' git_as alice push -q --force origin "$rewritten:main"
    [[ $(tip_of main) == "$base" ]]
    as alice req PUT /api/repos/alice/proj/branch-protection --accept json \
        --json '{"pattern":"main","require_pull_request":false,"force_push_allowances":["alice"]}' \
        --json-eq pattern=main --json-eq force_push_allowances.0=alice
    git_as alice push -q --force origin "$rewritten:main"
    [[ $(tip_of main) == "$rewritten" ]]
    git_as alice push -q --force origin "$base:main"
    as alice req DELETE /api/repos/alice/proj/branch-protection/main --status 204
    [[ $(tip_of main) == "$base" ]]
    git_as alice checkout -q main
}

# A pull request follows its base branch. A client that still holds the old base is
# told to reload, and the page's own form merges with the current commits.
t_pr_base() {
    local opened tip head pr moved
    opened=$(open_pull topic-base topic-base.txt)
    read -r tip head pr <<<"$opened"
    # Another change lands on main; it does not conflict with the pull request.
    moved=$(as alice req POST /api/repos/alice/proj/contents/landed-on-main.txt --accept json --status 201 \
        --json "{\"content\":\"$(printf 'landed\n' | base64 -w0)\",\"message\":\"Land on main\",\"branch\":\"main\",\"expected_revision\":\"$tip\"}" \
        --print json:sha)
    as alice req GET "/api/repos/alice/proj/pulls/$pr" --accept json \
        --json-eq "base_sha=$moved" --json-eq "head_sha=$head"
    as alice req POST "/api/repos/alice/proj/pulls/$pr/merge" --accept json --status 409 \
        --json "{\"expected_head\":\"$head\",\"expected_base\":\"$tip\"}" --contains-raw revision_conflict
    [[ $(tip_of main) == "$moved" ]]
    as alice form "/alice/proj/pulls/$pr" --action "/alice/proj/pulls/$pr/merge" \
        --has "expected_head=$head" --has "expected_base=$moved" --set confirm=merge \
        --location "/alice/proj/pulls/$pr"
    as alice req GET "/api/repos/alice/proj/pulls/$pr" --accept json --json-eq state=merged
    as alice req GET /alice/proj/raw/main/landed-on-main.txt --body-sha256 "$(sha_of $'landed\n')"
    as alice req GET /alice/proj/raw/main/topic-base.txt --body-sha256 "$(sha_of $'topic-base.txt\n')"
}

# A pull request follows its head branch: an editor save and a git push over SSH.
t_pr_head() {
    local opened tip head pr newer pushed base merged
    opened=$(open_pull topic-head topic-head.txt)
    read -r tip head pr <<<"$opened"
    newer=$(as alice req POST /api/repos/alice/proj/contents/topic-head-2.txt --accept json --status 201 \
        --json "{\"content\":\"$(printf 'more\n' | base64 -w0)\",\"message\":\"Add topic-head-2.txt\",\"branch\":\"topic-head\",\"expected_revision\":\"$head\"}" \
        --print json:sha)
    as alice req GET "/api/repos/alice/proj/pulls/$pr" --accept json \
        --json-eq "head_sha=$newer" --json-eq "base_sha=$tip"
    cd "$RUN/clone-ssh"
    git_as alice fetch -q origin
    git_as alice checkout -q -B topic-head origin/topic-head
    printf 'pushed over ssh\n' >topic-head-3.txt
    git_as alice add -A
    git_as alice commit -q -m 'Push to the head branch'
    git_as alice push -q origin topic-head
    pushed=$(git_as alice rev-parse HEAD)
    git_as alice checkout -q main
    as alice req GET "/api/repos/alice/proj/pulls/$pr" --accept json --json-eq "head_sha=$pushed"
    as alice req GET "/alice/proj/pulls/$pr" --contains "$pushed"
    # Merging uses the commits it records now.
    base=$(as alice req GET "/api/repos/alice/proj/pulls/$pr" --accept json --print json:base_sha)
    merged=$(as alice req POST "/api/repos/alice/proj/pulls/$pr/merge" --accept json \
        --json "{\"expected_head\":\"$pushed\",\"expected_base\":\"$base\"}" \
        --json-eq merged=true --print json:sha)
    [[ $(tip_of main) == "$merged" ]]
    as alice req GET /alice/proj/raw/main/topic-head-3.txt --body-sha256 "$(sha_of $'pushed over ssh\n')"
}

# A deleted head branch blocks the merge, a closed pull request is not followed and
# reopening it catches up.
t_pr_lifecycle() {
    local opened tip head pr newer updated
    opened=$(open_pull topic-gone topic-gone.txt)
    read -r tip head pr <<<"$opened"
    as alice req DELETE /api/repos/alice/proj/branches/topic-gone --status 204
    as alice req GET "/api/repos/alice/proj/pulls/$pr" --accept json --json-eq state=open --json-eq "head_sha=$head"
    as alice req POST "/api/repos/alice/proj/pulls/$pr/merge" --accept json --status 409 \
        --json "{\"expected_head\":\"$head\",\"expected_base\":\"$tip\"}" --contains-raw head_branch_missing
    opened=$(open_pull topic-closed topic-closed.txt)
    read -r tip head pr <<<"$opened"
    updated=$(as alice req GET "/api/repos/alice/proj/pulls/$pr" --accept json --print json:updated_at)
    as alice req PUT "/api/repos/alice/proj/pulls/$pr" --accept json \
        --json "{\"expected_state\":\"open\",\"expected_updated\":\"$updated\",\"state\":\"closed\"}" --json-eq state=closed
    newer=$(as alice req POST /api/repos/alice/proj/contents/topic-closed-2.txt --accept json --status 201 \
        --json "{\"content\":\"$(printf 'more\n' | base64 -w0)\",\"message\":\"Add topic-closed-2.txt\",\"branch\":\"topic-closed\",\"expected_revision\":\"$head\"}" \
        --print json:sha)
    as alice req GET "/api/repos/alice/proj/pulls/$pr" --accept json --json-eq "head_sha=$head"
    updated=$(as alice req GET "/api/repos/alice/proj/pulls/$pr" --accept json --print json:updated_at)
    as alice req PUT "/api/repos/alice/proj/pulls/$pr" --accept json \
        --json "{\"expected_state\":\"closed\",\"expected_updated\":\"$updated\",\"state\":\"open\"}" \
        --json-eq state=open --json-eq "head_sha=$newer"
}

# An approval belongs to the head it was given for: after a new push the protected
# branch asks for a fresh one.
t_pr_approval() {
    local opened tip head pr newer
    opened=$(open_pull topic-approve topic-approve.txt protected)
    read -r tip head pr <<<"$opened"
    as bob req POST "/api/repos/alice/proj/pulls/$pr/reviews" --accept json --status 201 \
        --json "{\"state\":\"approved\",\"commit_sha\":\"$head\"}"
    newer=$(as alice req POST /api/repos/alice/proj/contents/topic-approve-2.txt --accept json --status 201 \
        --json "{\"content\":\"$(printf 'more\n' | base64 -w0)\",\"message\":\"Add topic-approve-2.txt\",\"branch\":\"topic-approve\",\"expected_revision\":\"$head\"}" \
        --print json:sha)
    as alice req POST "/api/repos/alice/proj/pulls/$pr/merge" --accept json --status 409 \
        --json "{\"expected_head\":\"$newer\",\"expected_base\":\"$tip\"}" --contains-raw 'approving review'
    [[ $(tip_of protected) == "$tip" ]]
    as bob req POST "/api/repos/alice/proj/pulls/$pr/reviews" --accept json --status 201 \
        --json "{\"state\":\"approved\",\"commit_sha\":\"$newer\"}"
    as alice req POST "/api/repos/alice/proj/pulls/$pr/merge" --accept json \
        --json "{\"expected_head\":\"$newer\",\"expected_base\":\"$tip\"}" --json-eq merged=true
}

# A draft pull request is taken out of draft with the page's form (no JavaScript) or
# through the API, by its author or by someone who can write.
t_pr_draft() {
    local tip topic head first second
    tip=$(tip_of main)
    for topic in topic-draft-page topic-draft-api; do
        as alice req POST /api/repos/alice/proj/branches --accept json \
            --json "{\"name\":\"$topic\",\"sha\":\"$tip\"}" >/dev/null
        as alice req POST "/api/repos/alice/proj/contents/$topic.txt" --accept json --status 201 \
            --json "{\"content\":\"$(printf '%s\n' "$topic" | base64 -w0)\",\"message\":\"Add $topic.txt\",\"branch\":\"$topic\",\"expected_revision\":\"$tip\"}" >/dev/null
    done
    first=$(as alice req POST /api/repos/alice/proj/pulls --accept json --status 201 \
        --json '{"title":"Draft for the page","head_branch":"topic-draft-page","base_branch":"main","is_draft":true}' \
        --json-eq is_draft=true --print json:number)
    second=$(as alice req POST /api/repos/alice/proj/pulls --accept json --status 201 \
        --json '{"title":"Draft for the API","head_branch":"topic-draft-api","base_branch":"main","is_draft":true}' \
        --json-eq is_draft=true --print json:number)
    head=$(as alice req GET "/api/repos/alice/proj/pulls/$first" --accept json --print json:head_sha)
    # A draft cannot be merged.
    as alice req POST "/api/repos/alice/proj/pulls/$first/merge" --accept json --status 409 \
        --json "{\"expected_head\":\"$head\",\"expected_base\":\"$tip\"}" --contains-raw unsupported_merge
    # The page offers the action; its form needs no JavaScript.
    as alice req GET "/alice/proj/pulls/$first" --contains 'Ready for review'
    as alice form "/alice/proj/pulls/$first" --action "/alice/proj/pulls/$first/ready" \
        --location "/alice/proj/pulls/$first"
    as alice req GET "/api/repos/alice/proj/pulls/$first" --accept json --json-eq is_draft=false
    as alice req GET "/alice/proj/pulls/$first" --lacks 'Ready for review'
    # Through the API, by someone who can write but is not the author. Not signed in
    # is refused, and a second request finds nothing left to do.
    as anon req POST "/api/repos/alice/proj/pulls/$second/ready" --accept json --status 401
    as bob req POST "/api/repos/alice/proj/pulls/$second/ready" --accept json --json-eq is_draft=false
    as bob req POST "/api/repos/alice/proj/pulls/$second/ready" --accept json --status 409 --contains-raw invalid_state
}

# The repository's merge methods. A method it does not allow is refused with its code,
# each allowed method leaves the history it promises, and the settings read back. The
# settings are restored at the end, so later steps merge as before.
t_pr_merge_methods() {
    local tip sone shead squash rtip rone rhead rebase base merged replayed
    read -r tip sone squash <<<"$(open_pull merge-squash merge-squash-one.txt)"
    shead=$(commit_file merge-squash merge-squash-two.txt "$sone")
    read -r rtip rone rebase <<<"$(open_pull merge-rebase merge-rebase-one.txt)"
    rhead=$(commit_file merge-rebase merge-rebase-two.txt "$rone")
    as alice req PATCH /api/repos/alice/proj --accept json \
        --json '{"is_private":false,"description":"End to end project","allow_merge_commit":false}'
    as alice req GET /api/repos/alice/proj --accept json \
        --json-eq allow_merge_commit=false --json-eq allow_squash_merge=true --json-eq allow_rebase_merge=true
    as alice req POST "/api/repos/alice/proj/pulls/$rebase/merge" --accept json --status 409 \
        --json "{\"expected_head\":\"$rhead\",\"expected_base\":\"$rtip\",\"merge_method\":\"merge\"}" \
        --json-eq error=merge_method_not_allowed
    # Squash: one new commit on main, by the author's name, with the head's tree.
    merged=$(as alice req POST "/api/repos/alice/proj/pulls/$squash/merge" --accept json \
        --json "{\"expected_head\":\"$shead\",\"expected_base\":\"$tip\",\"merge_method\":\"squash\"}" \
        --json-eq merged=true --print json:sha)
    as alice req GET "/api/repos/alice/proj/pulls/$squash" --accept json --json-eq state=merged
    expect_eq 'main after the squash' "$(tip_of main)" "$merged"
    cd "$RUN/clone-ssh"
    git_as alice fetch -q origin
    expect_eq 'commits added by the squash' "$(git_as alice rev-list --count "$tip..$merged")" 1
    expect_eq 'squash author' "$(git_as alice log -1 --format='%an <%ae>' "$merged")" 'alice <alice@users.noreply>'
    expect_eq 'squash tree' "$(git_as alice rev-parse "$merged^{tree}")" "$(git_as alice rev-parse "$shead^{tree}")"
    # Only rebase is allowed now: squash is refused by its code.
    as alice req PATCH /api/repos/alice/proj --accept json \
        --json '{"is_private":false,"description":"End to end project","allow_squash_merge":false}'
    as alice req GET /api/repos/alice/proj --accept json \
        --json-eq allow_merge_commit=false --json-eq allow_squash_merge=false --json-eq allow_rebase_merge=true
    base=$(as alice req GET "/api/repos/alice/proj/pulls/$rebase" --accept json --print json:base_sha)
    as alice req POST "/api/repos/alice/proj/pulls/$rebase/merge" --accept json --status 409 \
        --json "{\"expected_head\":\"$rhead\",\"expected_base\":\"$base\",\"merge_method\":\"squash\"}" \
        --json-eq error=merge_method_not_allowed
    # Rebase: both commits are replayed on main with their messages and new ids; no merge commit.
    merged=$(as alice req POST "/api/repos/alice/proj/pulls/$rebase/merge" --accept json \
        --json "{\"expected_head\":\"$rhead\",\"expected_base\":\"$base\",\"merge_method\":\"rebase\"}" \
        --json-eq merged=true --print json:sha)
    as alice req GET "/api/repos/alice/proj/pulls/$rebase" --accept json --json-eq state=merged
    expect_eq 'main after the rebase' "$(tip_of main)" "$merged"
    git_as alice fetch -q origin
    expect_eq 'merge commits added by the rebase' "$(git_as alice rev-list --merges --count "$base..$merged")" 0
    expect_eq 'commits added by the rebase' "$(git_as alice rev-list --count "$base..$merged")" 2
    expect_eq 'rebased messages' "$(git_as alice log --reverse --format=%s "$base..$merged")" \
        $'Add merge-rebase-one.txt\nAdd merge-rebase-two.txt'
    replayed=$(git_as alice rev-list "$base..$merged")
    [[ $replayed != *"$rone"* && $replayed != *"$rhead"* ]]
    as alice req PATCH /api/repos/alice/proj --accept json \
        --json '{"is_private":false,"description":"End to end project","allow_merge_commit":true,"allow_squash_merge":true,"allow_rebase_merge":true}'
    as alice req GET /api/repos/alice/proj --accept json \
        --json-eq allow_merge_commit=true --json-eq allow_squash_merge=true --json-eq allow_rebase_merge=true
}

# A stack of three pull requests, each based on the branch of the one below it. Rebase stack
# from the middle layer moves all three onto a new main, and merging the top layer lands the
# whole stack in one merge commit of main and the top layer.
t_pr_stack() {
    local tip pra prb prc moved newa newb newc merged
    read -r tip _ pra <<<"$(open_pull stack-a stack-a.txt)"
    read -r _ _ prb <<<"$(open_pull stack-b stack-b.txt stack-a)"
    read -r _ _ prc <<<"$(open_pull stack-c stack-c.txt stack-b)"
    # Opening a pull request does not add it to a stack; joining does, parent first.
    as alice req GET "/api/repos/alice/proj/pulls/$prb" --accept json --json-eq stack=null
    as alice req POST "/api/repos/alice/proj/pulls/$prb/stack" --accept json \
        --json-eq stack.size=2 --json-eq stack.position=2 --json-eq stack.base_branch=main
    as alice req POST "/api/repos/alice/proj/pulls/$prc/stack" --accept json \
        --json-eq stack.size=3 --json-eq stack.position=3 --json-eq stack.base_branch=main
    as alice req GET "/api/repos/alice/proj/pulls/$pra" --accept json \
        --json-eq stack.size=3 --json-eq stack.position=1 \
        --json-eq "stack.entries.0.number=$pra" --json-eq "stack.entries.1.number=$prb" \
        --json-eq "stack.entries.2.number=$prc"
    moved=$(as alice req POST /api/repos/alice/proj/contents/stack-landed.txt --accept json --status 201 \
        --json "{\"content\":\"$(printf 'landed\n' | base64 -w0)\",\"message\":\"Land on main\",\"branch\":\"main\",\"expected_revision\":\"$tip\"}" \
        --print json:sha)
    as alice req POST "/api/repos/alice/proj/pulls/$prb/stack/rebase" --accept json --json-eq rebased=3
    cd "$RUN/clone-ssh"
    git_as alice fetch -q origin
    newa=$(git_as alice rev-parse origin/stack-a)
    newb=$(git_as alice rev-parse origin/stack-b)
    newc=$(git_as alice rev-parse origin/stack-c)
    git_as alice merge-base --is-ancestor "$moved" "$newa"
    git_as alice merge-base --is-ancestor "$newa" "$newb"
    git_as alice merge-base --is-ancestor "$newb" "$newc"
    as alice req GET "/api/repos/alice/proj/pulls/$pra" --accept json --json-eq "head_sha=$newa" --json-eq "base_sha=$moved"
    as alice req GET "/api/repos/alice/proj/pulls/$prb" --accept json --json-eq "head_sha=$newb" --json-eq "base_sha=$newa"
    as alice req GET "/api/repos/alice/proj/pulls/$prc" --accept json --json-eq "head_sha=$newc" --json-eq "base_sha=$newb"
    merged=$(as alice req POST "/api/repos/alice/proj/pulls/$prc/merge" --accept json \
        --json "{\"expected_head\":\"$newc\",\"expected_base\":\"$newb\"}" \
        --json-eq merged=true --print json:sha)
    expect_eq 'main after the stack merge' "$(tip_of main)" "$merged"
    git_as alice fetch -q origin
    # A merge always makes a merge commit of trunk and the top layer, which holds all three layers.
    expect_eq 'parents of the stack merge commit' "$(git_as alice rev-list --parents -n 1 "$merged" | cut -d' ' -f2-)" "$moved $newc"
    expect_eq 'merge commits added by the stack merge' "$(git_as alice rev-list --merges --count "$moved..$merged")" 1
    expect_eq 'commits added by the stack merge' "$(git_as alice rev-list --count "$moved..$merged")" 4
    for merged_pr in "$pra" "$prb" "$prc"; do
        as alice req GET "/api/repos/alice/proj/pulls/$merged_pr" --accept json --json-eq state=merged
    done
}

# The review, the inline comment and the Reviewers panel use the page forms, without
# JavaScript. The API shows the same reviews and requests.
t_pr_review_pages() {
    local tip head pr cid
    read -r tip head pr <<<"$(open_pull review-page review-page.txt)"
    head=$(as alice req POST /api/repos/alice/proj/contents/range.txt --accept json --status 201 \
        --json "{\"content\":\"$(printf 'one\ntwo\nthree\nfour\n' | base64 -w0)\",\"message\":\"Add range fixture\",\"branch\":\"review-page\",\"expected_revision\":\"$head\"}" \
        --print json:sha)
    # Reviewers must be able to write; grant it here rather than rely on an earlier step.
    as alice req POST /api/repos/alice/proj/collaborators --accept json --status 200,201 \
        --json '{"username":"bob","access_level":"write"}'
    as alice form "/alice/proj/pulls/$pr" --action "/alice/proj/pulls/$pr/requested_reviewers" \
        --has username --set username=bob --location "/alice/proj/pulls/$pr#reviewers"
    as alice req GET "/api/repos/alice/proj/pulls/$pr/requested_reviewers" --accept json --json-eq 0.username=bob
    as alice req GET "/alice/proj/pulls/$pr" --contains 'Review requested' \
        --contains 'Remove the review request for bob'
    as bob form "/alice/proj/pulls/$pr/diff" --action "/alice/proj/pulls/$pr/reviews" --has state \
        --submit intent=submit --set state=approved --set 'body=Approved by the end-to-end check' \
        --location "/alice/proj/pulls/$pr#review-*"
    as bob form "/alice/proj/pulls/$pr/diff?path=review-page.txt&side=RIGHT&line=1" \
        --action "/alice/proj/pulls/$pr/comments" --has path_json \
        --set 'body=Please check this line' --location "/alice/proj/pulls/$pr/diff#thread-*"
    cid=$(as alice req GET "/api/repos/alice/proj/pulls/$pr/comments" --accept json --print json:0.id)
    as alice req GET "/alice/proj/pulls/$pr/diff" --contains 'Please check this line' \
        --contains 'Resolve conversation'
    as alice form "/alice/proj/pulls/$pr/diff" --action "/alice/proj/pulls/$pr/comments/$cid/resolve" \
        --location "/alice/proj/pulls/$pr/diff#thread-$cid"
    as alice req GET "/alice/proj/pulls/$pr/diff" --contains 'Reopen conversation' --lacks 'Resolve conversation'
    as alice req GET "/alice/proj/pulls/$pr" --contains 'approved these changes' \
        --contains 'Approved by the end-to-end check' --contains 'Ask bob to review again'
    as alice req GET "/api/repos/alice/proj/pulls/$pr/reviews" --accept json \
        --json-eq 0.state=approved --json-eq 0.author.username=bob --json-eq "0.commit_sha=$head"
    # Asking again after a review, then removing the new request.
    as alice form "/alice/proj/pulls/$pr" --action "/alice/proj/pulls/$pr/requested_reviewers" \
        --has username=bob --location "/alice/proj/pulls/$pr#reviewers"
    as alice req GET "/alice/proj/pulls/$pr" --contains 'Remove the review request for bob'
    as alice req GET "/api/repos/alice/proj/pulls/$pr/requested_reviewers" --accept json --json-eq 0.username=bob
    as alice form "/alice/proj/pulls/$pr" --action "/alice/proj/pulls/$pr/requested_reviewers/bob/remove" \
        --location "/alice/proj/pulls/$pr#reviewers"
    as alice req GET "/alice/proj/pulls/$pr" --lacks 'Remove the review request for bob' \
        --contains 'Ask bob to review again'
    as alice req GET "/api/repos/alice/proj/pulls/$pr/requested_reviewers" --accept json --contains-raw '[]'
    as bob review-range "/alice/proj/pulls/$pr/diff" --path range.txt
}

# A mention notifies the mentioned user and links on the page; a commit SHA and a #number
# link too. A name that is not an account stays plain text.
t_mentions_and_references() {
    local tip head pr
    read -r tip head pr <<<"$(open_pull mention-page mention-page.txt)"
    as alice req POST "/api/repos/alice/proj/pulls/$pr/comments" --accept json --status 201 \
        --json "{\"body\":\"Thanks @bob. See #$pr and commit $head. Ask @nobody-here too.\"}"
    as bob req GET "/api/notifications?limit=100" --accept json \
        --contains-raw '"kind":"mention"' --contains-raw "alice mentioned you in alice/proj#$pr"
    as bob req GET "/alice/proj/pulls/$pr" \
        --contains 'class="markdown-reference">@bob</a>' \
        --contains "href=\"/alice/proj/issues/$pr\"" \
        --contains "href=\"/alice/proj/commit/$head\"" \
        --contains '@nobody-here' --lacks 'href="/nobody-here"'
}

t_issues() {
    local location n
    location=$(as alice form /alice/proj/issues/new --action /alice/proj/issues/new --has title \
        --set 'title=E2E issue' --set 'body=Something to track' --submit intent=create \
        --location '/alice/proj/issues/*' --print location)
    n=${location##*/}
    [[ $n =~ ^[0-9]+$ ]]
    put issue_number "$n"
    as alice req GET "/alice/proj/issues/$n" --contains 'E2E issue' --contains 'Something to track'
    as alice form "/alice/proj/issues/$n" --action "/alice/proj/issues/$n/comments" --has body \
        --set 'body=First comment from the web form' --submit intent=comment \
        --location "/alice/proj/issues/$n#comment-*"
    as alice req GET "/alice/proj/issues/$n" --contains 'First comment from the web form'
    as bob req POST "/api/repos/alice/proj/issues/$n/comments" --accept json --status 201 \
        --json '{"body":"Second comment from bob through the API"}' \
        --json-eq body='Second comment from bob through the API'
    as alice req GET "/api/repos/alice/proj/issues/$n/comments" --accept json \
        --json-eq '0.body=First comment from the web form' \
        --json-eq '1.body=Second comment from bob through the API'
    as alice req GET /api/repos/alice/proj/issues --accept json \
        --json-eq '0.title=E2E issue' --json-eq 0.state=open --json-eq "0.number=$n"
    as bob req GET /alice/proj/issues --contains 'E2E issue'
}

# upload_as WHO NAME CONTENT-TYPE REASON VISIBILITY FILE: reserve, PUT and confirm as WHO; prints the file id
upload_as() {
    local who=$1 name=$2 type=$3 reason=$4 visibility=$5 file=$6 size sha reserved id url
    size=$(stat -c %s "$file")
    sha=$(sha256sum "$file" | cut -d' ' -f1)
    reserved=$(as "$who" req POST /api/files --csrf --accept json \
        --json "{\"filename\":\"$name\",\"content_type\":\"$type\",\"file_size\":$size,\"reason\":\"$reason\",\"visibility\":\"$visibility\",\"sha256_hex\":\"$sha\"}" \
        --json-eq method=PUT --json-eq capability=authenticated_local_put --json-eq csrf_required=true \
        --print json:file_id --print json:upload_url)
    id=$(sed -n 1p <<<"$reserved")
    url=$(sed -n 2p <<<"$reserved")
    [[ $url == "/api/files/$id/content" ]]
    as "$who" req PUT "$url" --csrf --body-file "$file" --content-type "$type" --status 204
    as "$who" req POST /api/files/confirm --csrf --accept json --json "{\"file_id\":\"$id\"}" \
        --json-eq success=true
    echo "$id"
}
upload_as_alice() { upload_as alice "$@"; }

t_avatar() {
    local png=$RUN/avatar.png fake=$RUN/not-a-png.bin note=$RUN/note.txt size sha id note_id reserved
    "$PYTHON" -I -c 'import struct, sys, zlib
def chunk(kind, data):
    body = kind + data
    return struct.pack(">I", len(data)) + body + struct.pack(">I", zlib.crc32(body) & 0xFFFFFFFF)
png = (b"\x89PNG\r\n\x1a\n" + chunk(b"IHDR", struct.pack(">IIBBBBB", 1, 1, 8, 2, 0, 0, 0))
       + chunk(b"IDAT", zlib.compress(b"\x00\xff\x00\x00")) + chunk(b"IEND", b""))
open(sys.argv[1], "wb").write(png)' "$png"
    printf '<html>this is not a png</html>' >"$fake"
    printf 'private note\n' >"$note"
    size=$(stat -c %s "$png")
    sha=$(sha256sum "$png" | cut -d' ' -f1)
    # Reserving without the session's CSRF token is refused.
    as alice req POST /api/files --accept json --status 403 \
        --json "{\"filename\":\"avatar.png\",\"content_type\":\"image/png\",\"file_size\":$size,\"reason\":\"avatar\",\"visibility\":\"public\",\"sha256_hex\":\"$sha\"}"
    # Reserve, PUT the bytes to the local upload URL, confirm.
    id=$(upload_as_alice avatar.png image/png avatar public "$png" | tail -n 1)
    as alice req POST /api/files/confirm --csrf --accept json --json "{\"file_id\":\"$id\"}" --status 409
    # Download: the owner and anyone (it is a public avatar) get the same bytes.
    as alice req GET "/api/files/$id/download" --accept any --header-eq 'content-type: image/png' \
        --body-sha256 "$sha"
    as anon req GET "/api/files/$id/download" --accept any --header-eq 'content-type: image/png' \
        --body-sha256 "$sha"
    as alice req GET "/api/files/$id/dl" --accept json \
        --json-eq "file_size=$size" --json-eq capability=authorized_application_get
    # Bytes that are not the declared PNG are rejected when uploaded.
    reserved=$(as alice req POST /api/files --csrf --accept json \
        --json "{\"filename\":\"fake.png\",\"content_type\":\"image/png\",\"file_size\":$(stat -c %s "$fake"),\"reason\":\"avatar\",\"visibility\":\"public\",\"sha256_hex\":\"$(sha256sum "$fake" | cut -d' ' -f1)\"}" \
        --print json:upload_url)
    as alice req PUT "$reserved" --csrf --body-file "$fake" --content-type image/png --status 422
    # A file kept private to its owner is not downloadable by anyone else.
    note_id=$(upload_as_alice note.txt text/plain other owner "$note" | tail -n 1)
    as alice req GET "/api/files/$note_id/download" --accept any --body-sha256 "$(sha256sum "$note" | cut -d' ' -f1)"
    as bob req GET "/api/files/$note_id/download" --accept any --status 404
    as anon req GET "/api/files/$note_id/download" --accept any --status 404
    as bob req GET "/api/files/$note_id/dl" --accept json --status 404
    put note_id "$note_id"
    put note_sha "$(sha256sum "$note" | cut -d' ' -f1)"
}

# The profile settings form sends the picture itself; it works without JavaScript.
t_avatar_profile_form() {
    local png=$RUN/avatar.png fake=$RUN/not-a-png.bin url sha
    sha=$(sha256sum "$png" | cut -d' ' -f1)
    as alice req GET '/settings?tab=profile' --contains 'Profile picture' --lacks 'Remove picture'
    as alice form '/settings?tab=profile' --action /settings/avatar --file "picture=$png" \
        --location '/settings?tab=profile&saved=avatar'
    url=$(as alice req GET /settings --accept model --print json:page.model.Settings.model.profile.avatar_url)
    [[ $url == /api/files/*/download ]]
    as anon req GET "$url" --accept any --header-eq 'content-type: image/png' --body-sha256 "$sha"
    as alice req GET '/settings?tab=profile&saved=avatar' --contains 'Profile picture updated.' \
        --contains "$url" --contains 'Remove picture'
    as bob req GET /alice --contains "$url"
    # Bytes that are not a picture are refused and the picture stays.
    as alice form '/settings?tab=profile' --action /settings/avatar --file "picture=$fake" \
        --status 422 --contains 'Choose a PNG, JPEG, GIF or WebP picture'
    as alice req GET /settings --accept model --json-eq "page.model.Settings.model.profile.avatar_url=$url"
    # The remove form clears it and deletes the file.
    as alice form '/settings?tab=profile' --action /settings/avatar/remove \
        --location '/settings?tab=profile&saved=avatar-removed'
    as alice req GET /settings --accept model --json-eq page.model.Settings.model.profile.avatar_url=null
    as anon req GET "$url" --accept any --status 404
}

# The organization page sends the picture the same way; its owner and admins may.
t_avatar_org_form() {
    local png=$RUN/avatar.png id url
    id=$(get org_id)
    as alice req GET "/settings/organizations/$id" --contains 'Organization picture' --lacks 'Remove picture'
    as alice form "/settings/organizations/$id" --action "/settings/organizations/$id/avatar" \
        --file "picture=$png" --location "/settings/organizations/$id"
    url=$(as alice req GET "/settings/organizations/$id" \
        --contains 'Remove picture' --print 're:src="(/api/files/[0-9a-f-]+/download)"')
    as anon req GET "$url" --accept any --header-eq 'content-type: image/png' \
        --body-sha256 "$(sha256sum "$png" | cut -d' ' -f1)"
    # Someone outside the organization cannot see or change it.
    as bob req GET "/settings/organizations/$id" --status 404
    as alice form "/settings/organizations/$id" --action "/settings/organizations/$id/avatar/remove" \
        --location "/settings/organizations/$id"
    as alice req GET "/settings/organizations/$id" --lacks 'Remove picture' --lacks '/api/files/'
    as anon req GET "$url" --accept any --status 404
}

# check_pages: every row is WHO PATH LANE TEXT ("-" for none) and is fetched as
# WHO, then checked against the server HTML and hydration contract.
check_pages() {
    local who path lane contains issue pr
    issue=$(get issue_number)
    pr=$(get pr_number)
    while read -r who path lane contains; do
        [[ -n $who ]] || continue
        path=${path//@ISSUE@/$issue}
        path=${path//@PR@/$pr}
        local args=()
        if [[ $who == anon ]]; then args+=(--anonymous); else args+=(--user "$who"); fi
        if [[ $lane != - ]]; then args+=(--lane "$lane"); fi
        if [[ $contains != - ]]; then args+=(--contains "$contains"); fi
        as "$who" page "$path" "${args[@]}"
    done <<'PAGES'
anon / Discovery -
anon /explore Discovery alice/proj
anon /login Identity Sign in
anon /register Identity -
anon /search?q=proj Discovery proj
anon /alice Identity alice
anon /alice/proj Repositories README.md
anon /alice/proj/blob/main/README.md Repositories README.md
anon /alice/proj/commits Repositories Initial commit
anon /alice/proj/branches Repositories main
anon /alice/proj/issues Collaboration -
anon /alice/proj/issues/@ISSUE@ Collaboration -
anon /alice/proj/pulls Collaboration -
anon /alice/proj/pulls/@PR@ Collaboration -
anon /e2e-org/widgets Repositories widgets
alice / Discovery proj
alice /new Discovery -
alice /settings Identity -
alice /settings/organizations Organizations -
alice /admin - -
alice /admin/users Identity -
alice /admin/settings Identity -
alice /admin/audit Identity -
alice /admin/storage Storage -
alice /admin/nodes Nodes No nodes have joined
alice /notifications - -
alice /alice/proj/settings Repositories -
alice /alice/proj/edit/main/README.md Repositories -
alice /alice/ci-demo/actions Actions -
bob / Discovery proj
PAGES
}

t_pages() {
    check_pages
    local target_id
    target_id=$(as alice req GET '/admin/users?q=bob' --print 're:action="/admin/accounts/([0-9]+)/ban"')
    as alice page /admin/users --user alice --lane Identity --contains 'Ban bob'
    as bob req GET /admin/users --status 403
    as alice form /admin/users \
        --action "/admin/accounts/$target_id/ban" --unset confirm_delete \
        --status 400 --contains 'Confirm the ban before saving it'
    # The data client navigation fetches: same page, as typed JSON.
    as alice req GET /alice/proj --accept model --header-eq 'content-type: application/vnd.codeyard.page+json' \
        --json-eq page.lane=Repositories --json-eq location=/alice/proj --json-eq chrome.username=alice
    # Native assets, health and the repository's clone endpoints.
    as anon req GET /assets/tailwind.css --header-has 'content-type: text/css'
    as anon req GET /assets/logo.svg --header-has 'content-type: image/svg+xml'
    as anon req GET /assets/fonts/mona-sans.woff2 --header-has 'content-type: font/woff2'
    as anon req GET /health --accept json --json-eq status=ok
    as anon req GET /ready --accept json --json-eq status=ready
    as bob req GET /admin/nodes --status 403
    # Without PUBLIC_SSH_CLONE_BASE the SSH clone URL takes its host from PUBLIC_URL, which is $BASE.
    as anon req GET /alice/proj --contains-raw "$BASE/alice/proj.git" \
        --contains-raw "$SSH_URL/alice/proj.git"
    as anon req GET /no/such/page --status 404
}

t_bundle() {
    as anon bundle --dir "$WEBUI_DIST"
}

wait_ci_run() {
    local id=$1 deadline=${2:-$((SECONDS + 150))} status
    while :; do
        status=$(as alice req GET "/api/ci/jobs/$id" --accept json --print json:status)
        case $status in success | failed | cancelled) break ;; esac
        ((SECONDS < deadline)) || { echo "run $id is still '$status' at the deadline" >&2; return 1; }
        sleep 1
    done
    if [[ $status != success ]]; then
        echo "run $id ended '$status'" >&2
        as alice req GET "/api/ci/jobs/$id" --accept json --print json:error_message >&2 || true
        as alice req GET "/alice/ci-demo/actions?job=$id&view=logs&log_limit=5000" >&2 || true
        return 1
    fi
}

t_ci_result() {
    local sha id deadline=$((SECONDS + 150)) queued_until=$((SECONDS + 45))
    sha=$(get ci_sha)
    # The push queued exactly one run for this commit.
    until id=$(as alice req GET '/api/ci/jobs?owner=alice&repo=ci-demo' --accept json \
        --json-eq "jobs.0.head_sha=$sha" --print json:jobs.0.id 2>/dev/null); do
        ((SECONDS < queued_until)) || { echo "no CI run was queued for $sha" >&2; return 1; }
        sleep 1
    done
    put ci_run "$id"
    echo "run $id: branch field '$(as alice req GET "/api/ci/jobs/$id" --accept json --print json:branch)'"
    as alice req GET "/api/ci/jobs/$id" --accept json \
        --json-eq event=push --json-eq branch=main \
        --json-eq workflow_path=.github/workflows/ci.yml
    # The Actions page lists the run under its branch, and the branch filter finds it.
    as alice req GET /alice/ci-demo/actions --no-scripts --contains "$id" --lacks refs/heads/main
    as alice req GET '/alice/ci-demo/actions?branch=main' --no-scripts --contains "$id"
    as alice req GET '/alice/ci-demo/actions?branch=other' --no-scripts --lacks "$id"
    needs_ci_runs || return
    wait_ci_run "$id" "$deadline"
    # The claiming worker stored its mode; a queued run is still `unassigned` until then.
    as alice req GET "/api/ci/jobs/$id" --accept json --json-eq mode=local
    as alice req GET "/alice/ci-demo/actions?job=$id&view=logs" --contains '| hello'
    # The workflow sees the real branch, repository and pusher, not act's defaults.
    as alice req GET "/alice/ci-demo/actions?job=$id&view=logs" \
        --contains '| context refs/heads/main alice/ci-demo alice'
    as alice req GET "/alice/ci-demo/actions?job=$id" --contains '| hello' --contains 'gitserv/workflow/.github/workflows/ci.yml'
    as alice req GET "/api/repos/alice/ci-demo/commits/$sha/status" --accept json \
        --json-eq state=success --json-eq total_count=1 --json-eq statuses.0.context=gitserv/workflow/.github/workflows/ci.yml
}

# The file API moves a branch itself, so it queues the push workflows like an SSH push.
t_ci_api_save() {
    local sha id deadline=$((SECONDS + 150)) queued_until=$((SECONDS + 45))
    sha=$(as alice req POST /api/repos/alice/ci-demo/contents/notes.txt --accept json --status 201 \
        --json "{\"content\":\"$(printf 'saved through the API\n' | base64 -w0)\",\"message\":\"Add notes.txt\",\"branch\":\"main\",\"expected_revision\":\"$(get ci_sha)\"}" \
        --print json:sha)
    until id=$(as alice req GET '/api/ci/jobs?owner=alice&repo=ci-demo' --accept json \
        --json-eq "jobs.0.head_sha=$sha" --print json:jobs.0.id 2>/dev/null); do
        ((SECONDS < queued_until)) || { echo "no CI run was queued for $sha" >&2; return 1; }
        sleep 1
    done
    as alice req GET "/api/ci/jobs/$id" --accept json --json-eq event=push --json-eq branch=main
    needs_ci_runs || return
    # Let the run finish, so the restart below stops an idle server.
    wait_ci_run "$id" "$deadline"
}

# Every gh command and flag against this round's server; opt in with E2E_GH_SWEEP=1. The
# table of cases goes to $RUN/gh-sweep.txt, which gh_sweep_step prints after the step line.
t_gh_sweep() {
    local rc=0
    local -a skip=()
    # Cases that need a finished CI run report SKIP where local runs cannot execute.
    [[ -z $CI_RUNS_UNAVAILABLE ]] || skip=(--skip-runs "$CI_RUNS_UNAVAILABLE")
    "$GITHUB_PYTHON" -I "$ROOT/tests/e2e/gh_sweep/harness.py" --base "$BASE" --workers "${E2E_GH_SWEEP_WORKERS:-4}" \
        --per-account 2 --record-output "$RUN/gh-sweep-argv.json" "${skip[@]}" >"$RUN/gh-sweep.txt" 2>&1 || rc=$?
    grep '^SWEEP ' "$RUN/gh-sweep.txt" || echo "the sweep printed no totals; see $RUN/gh-sweep.txt"
    "$GITHUB_PYTHON" -I "$ROOT/tests/e2e/gh_sweep/harness.py" --check --records "$RUN/gh-sweep-argv.json" \
        | "$GITHUB_PYTHON" -I -c 'import json, sys; r = json.load(sys.stdin); print("gh sweep flags not exercised:", len(r["missing_flags"]), "waived:", r["waived"])'
    return $rc
}

gh_sweep_step() {
    [[ ${E2E_GH_SWEEP:-0} == 1 ]] || return 0
    step 'real gh sweep: every gh command and flag, several accounts at once' t_gh_sweep
    # Every case line, each cut at 2000 characters.
    [[ ! -f $RUN/gh-sweep.txt ]] || head -n 3000 "$RUN/gh-sweep.txt" | cut -c1-2000 | sed 's/^/        | /'
}

# The GHES TLS tunnel and webhook receiver belong to this helper and stop before it returns.
t_github_token() {
    local token
    token=$(as bob form '/settings?tab=tokens' --action /settings/tokens --has name \
        --set 'name=real gh client' --set scope=write --set expiry=never --status 200 \
        --contains 'Copy this token now' --print 're:(?s)Copy this token now.*?(cy_pat_(?:[0-9]{1,5}_)?[0-9a-f]{64})')
    put gh_bob_token "$token"
}

t_github_clients() {
    local id engine rc=0
    local -a skip=()
    t_github_token
    [[ -z $CI_RUNS_UNAVAILABLE ]] || skip=(--skip-runs "$CI_RUNS_UNAVAILABLE")
    local check_postgres=0
    if [[ $ROUND == postgres || $ROUND == cluster ]]; then check_postgres=1; fi
    E2E_CHECK_PG_RESTART=$check_postgres E2E_STOP_WEB_PID=${E2E_STOP_WEB_PID:-} "$GITHUB_PYTHON" -I "$GITHUB_HELPER" --base "$BASE" --run "$RUN" "${skip[@]}" || rc=$?
    id=$(get gh_run_id)
    [[ $id =~ ^[0-9a-f-]{36}$ ]]
    if [[ $ROUND == sqlite ]]; then
        engine=$("$PYTHON" -I - "$RUN/state/gitserv.db" "$id" <<'PY'
import sqlite3, sys, uuid
with sqlite3.connect(sys.argv[1]) as connection:
    print(connection.execute("SELECT engine FROM ci_runs WHERE run_id=?", (uuid.UUID(sys.argv[2]).bytes,)).fetchone()[0])
PY
)
    else
        engine=$(pg_query "SELECT engine FROM ci_runs WHERE run_id='$id'")
    fi
    expect_eq 'github-script ran on the native engine' "$engine" codeyard-actions
    ((rc == 0)) || return "$rc"
    needs_ci_runs || return
}

actions_restart_server() {
    # The upstream toolkits reserve non-GitHub hosts for GHES (v1 caches, no v4 artifacts).
    # Their .localhost test origin changes metadata only; Results traffic uses the real listener.
    as alice form /admin/actions --action /admin/actions --has results_url \
        --set "results_url=$BASE" --set default_engine=codeyard-actions \
        --location '/admin/actions?saved=1' || return 1
    stop_server || return 1
    ACTION_OBJECTS_ORIGIN=http://codeyard.localhost:$HTTP_PORT
    start_server
}

t_action_objects() {
    needs_ci_runs || return
    local repo directory sha id expected deadline
    # Each round owns a new database: identical keys test repository isolation too.
    for repo in action-objects action-isolation; do
        as alice req POST /api/repos --accept json --status 201 \
            --json "{\"name\":\"$repo\",\"is_private\":true}"
        directory=$RUN/$repo
        mkdir -p "$directory/.github/workflows"
        git_as alice -C "$directory" init -q -b main
        git_as alice -C "$directory" remote add origin "$BASE/alice/$repo.git"
        cat >"$directory/.github/workflows/objects.yml" <<'YAML'
name: Real cache and artifact actions
on: push
env:
  EXPECT_HIT: 'false'
jobs:
  upload:
    runs-on: self-hosted
    steps:
      - run: |
          mkdir expected
          printf 'Codeyard actions bytes\n\000\377\n' > expected/payload.bin
      - id: restore
        uses: actions/cache/restore@0057852bfaa89a56745cba8c7296529d2fc39830 # v4
        with:
          path: saved-cache
          key: e2e-saved
      - env:
          HIT: ${{ steps.restore.outputs.cache-hit }}
        run: |
          if [ "$EXPECT_HIT" = true ]; then
            test "$HIT" = true
            cmp expected/payload.bin saved-cache/payload.bin
            echo CACHE_RESTORE_HIT
          else
            test "$HIT" != true
            test ! -e saved-cache/payload.bin
            mkdir saved-cache
            cp expected/payload.bin saved-cache/payload.bin
            echo CACHE_RESTORE_MISS
          fi
      - if: env.EXPECT_HIT == 'false'
        uses: actions/cache/save@0057852bfaa89a56745cba8c7296529d2fc39830 # v4
        with:
          path: saved-cache
          key: e2e-saved
      - id: unknown
        uses: actions/cache/restore@0057852bfaa89a56745cba8c7296529d2fc39830 # v4
        with:
          path: unknown-cache
          key: e2e-never-saved
      - env:
          HIT: ${{ steps.unknown.outputs.cache-hit }}
        run: |
          test "$HIT" != true
          test ! -e unknown-cache
          echo CACHE_UNKNOWN_MISS
      - id: combined
        uses: actions/cache@0057852bfaa89a56745cba8c7296529d2fc39830 # v4
        with:
          path: combined-cache
          key: e2e-combined
      - env:
          HIT: ${{ steps.combined.outputs.cache-hit }}
        run: |
          if [ "$EXPECT_HIT" = true ]; then
            test "$HIT" = true
            cmp expected/payload.bin combined-cache/payload.bin
            echo CACHE_COMBINED_HIT
          else
            test "$HIT" != true
            test ! -e combined-cache/payload.bin
            mkdir combined-cache
            cp expected/payload.bin combined-cache/payload.bin
          fi
      - id: artifact
        uses: actions/upload-artifact@ea165f8d65b6e75b540449e92b4886f43607fa02 # v4
        with:
          name: real-bytes
          path: expected/payload.bin
          if-no-files-found: error
      - env:
          ARTIFACT_ID: ${{ steps.artifact.outputs.artifact-id }}
        run: test -n "$ARTIFACT_ID"
  download:
    needs: upload
    runs-on: self-hosted
    steps:
      - uses: actions/download-artifact@d3f86a106a0bac45b974a628896c90dbdf5c8093 # v4
        with:
          name: real-bytes
          path: downloaded
      - run: |
          printf 'Codeyard actions bytes\n\000\377\n' > expected.bin
          cmp expected.bin downloaded/payload.bin
          echo ARTIFACT_BYTES_OK
YAML
        for expected in false true; do
            [[ $repo != action-isolation || $expected == false ]] || continue
            if [[ $expected == true ]]; then
                "$PYTHON" -I - "$directory/.github/workflows/objects.yml" <<'PY'
import pathlib, sys
path = pathlib.Path(sys.argv[1])
path.write_text(path.read_text().replace("EXPECT_HIT: 'false'", "EXPECT_HIT: 'true'"))
PY
            fi
            git_as alice -C "$directory" add .github
            git_as alice -C "$directory" commit -q -m "Check cache hit $expected"
            # Use the same authenticated HTTP push as the other CI fixture.
            git_with_token write -C "$directory" push -q origin main
            sha=$(git_as alice -C "$directory" rev-parse HEAD)
            deadline=$((SECONDS + 300))
            until id=$(as alice req GET "/api/ci/jobs?owner=alice&repo=$repo" --accept json \
                --json-eq "jobs.0.head_sha=$sha" --print json:jobs.0.id 2>/dev/null); do
                ((SECONDS < deadline)) || { echo "no run queued for $repo $sha" >&2; return 1; }
                sleep 1
            done
            if ! wait_ci_run "$id" "$deadline"; then
                as alice req GET "/alice/$repo/actions?job=$id&view=logs&log_limit=5000" >&2 || true
                return 1
            fi
            as alice req GET "/alice/$repo/actions?job=$id&view=logs&log_limit=5000" \
                --contains ARTIFACT_BYTES_OK --contains CACHE_UNKNOWN_MISS
            if [[ $expected == true ]]; then
                as alice req GET "/alice/$repo/actions?job=$id&view=logs&log_limit=5000" \
                    --contains CACHE_RESTORE_HIT --contains CACHE_COMBINED_HIT
            else
                as alice req GET "/alice/$repo/actions?job=$id&view=logs&log_limit=5000" --contains CACHE_RESTORE_MISS
            fi
        done
    done
}

t_after_restart() {
    local head
    # Sessions, accounts, repositories and CI history live in the database/disk.
    as alice req GET /settings --accept model --json-eq chrome.username=alice
    as alice req GET /alice/proj --contains README.md
    as alice req GET "/api/repos/alice/proj/pulls/$(get pr_number)" --accept json --json-eq state=merged
    # SSH works again and presents the same host key (known_hosts remembers it).
    git_as alice ls-remote "$SSH_URL/alice/proj.git" refs/heads/main | grep -q refs/heads/main
    git_as alice ls-remote "$BASE/alice/proj.git" HEAD | grep -q HEAD
    # A push after the restart still works end to end.
    cd "$RUN/clone-ssh"
    git_as alice checkout -q main
    git_as alice pull -q --ff-only
    printf 'after restart\n' >after-restart.txt
    git_as alice add -A
    git_as alice commit -q -m 'Push after restart'
    git_as alice push -q origin main
    head=$(git_as alice rev-parse HEAD)
    [[ $(git_as alice ls-remote "$SSH_URL/alice/proj.git" refs/heads/main | cut -f1) == "$head" ]]
    as alice req GET /alice/proj/raw/main/after-restart.txt --body-sha256 "$(sha_of $'after restart\n')"
    needs_ci_runs || return
    as alice req GET "/api/ci/jobs/$(get ci_run)" --accept json --json-eq status=success
}

t_log_clean() {
    local log
    log=$(server_dir "${1:-server}")/server.log
    if grep -qE 'panicked|thread .*panic' "$log"; then
        grep -E 'panicked|thread .*panic' "$log" | head -n 5 >&2
        return 1
    fi
    echo "$log: warnings: $(grep -c ' WARN ' "$log" || true), errors: $(grep -c ' ERROR ' "$log" || true) (negative checks log some on purpose)"
}

# ------------------------------------------------------------- runner steps --

runners_restart_server() { # inline: the server keeps no runner of its own and opens the node connection
    local udp
    local -a tcp ports
    stop_server || return 1
    read -r udp < <(free_ports 1 udp) || return 1
    SERVER_ROLES=server
    SERVER_IROH=$udp
    start_server || return 1
    read -r -a tcp < <(free_ports 4) || return 1
    read -r -a ports < <(free_ports 2 udp) || return 1
    HTTP_PORTS[r1]=${tcp[0]} SSH_PORTS[r1]=${tcp[1]} IROH_PORTS[r1]=${ports[0]}
    HTTP_PORTS[r2]=${tcp[2]} SSH_PORTS[r2]=${tcp[3]} IROH_PORTS[r2]=${ports[1]}
    mkdir -p "$(server_dir r1)" "$(server_dir r2)"
}

t_runner_token() {
    # A single-node installation invites runners only.
    must_fail 'a storage invite in single-node mode' 'codeyard cluster enable' \
        server_cli cluster invite --roles storage
    put runner_token "$(server_cli cluster invite --reusable --roles runner)"
}

t_runners_repo() {
    local work=$RUN/work/runners
    as alice req POST /api/repos --accept json --status 201 \
        --json '{"name":"runners","description":"Runs for the runner checks"}' --json-eq full_name=alice/runners
    as alice form '/alice/runners/settings?tab=actions' --action /alice/runners/_forms/actions-settings \
        --has engine --set engine=act --location '/alice/runners/settings?tab=actions'
    rm -rf "$work"
    mkdir -p "$work/.github/workflows"
    cd "$work"
    git_as alice init -q -b main
    # Each run appends its commit to one file, so a run that ran twice shows up.
    cat >.github/workflows/ci.yml <<YAML
name: Runners
on: [push]
jobs:
  slow:
    runs-on: self-hosted
    steps:
      - run: while [ -e "$RUN/runner-latch" ]; do sleep 0.2; done; sleep 12
      - run: echo "\${{ github.sha }}" >>"$RUN/runner-marks"
YAML
    git_as alice add -A
    git_as alice commit -q -m 'Add the slow workflow'
    git_as alice remote add origin "$BASE/alice/runners.git"
}

runners_push() { # runners_push MESSAGE: one commit to alice/runners; prints "run commit"
    local sha id deadline=$((SECONDS + 45))
    cd "$RUN/work/runners"
    git_as alice commit -q --allow-empty -m "$1"
    git_with_token write push -q origin main
    sha=$(git_as alice rev-parse HEAD)
    until id=$(pg_query "SELECT run_id FROM ci_runs WHERE head_sha='$sha' AND event='push'") \
        && [[ $id =~ ^[0-9a-f-]{36}$ ]]; do
        ((SECONDS < deadline)) || { echo "no run was queued for $sha" >&2; return 1; }
        sleep 1
    done
    echo "$id $sha"
}

runner_of() { # runner_of RUN: the runner node that claimed RUN
    local key node
    key=$(pg_query "SELECT split_part(worker, '/', 1) FROM ci_runs WHERE run_id='$1'")
    for node in r1 r2; do
        if [[ -n $key && $(pg_query "SELECT public_key FROM nodes WHERE name='$node'") == "$key" ]]; then
            echo "$node"
            return 0
        fi
    done
    echo "run $1 was claimed by no runner ('$key')" >&2
    return 1
}

t_runners_share() {
    local n run sha taken='' deadline=$((SECONDS + 120))
    needs_ci_runs || return
    touch "$RUN/runner-latch"
    for n in 1 2 3 4; do
        read -r run sha < <(runners_push "Shared run $n")
        [[ -n $sha ]]
        put "shared_$n" "$run $sha"
    done
    until [[ $(pg_query "SELECT count(DISTINCT worker) FROM ci_runs WHERE status='running' AND repo_id=(SELECT id FROM repositories WHERE name='runners')") == 2 ]]; do
        ((SECONDS < deadline)) || { rm -f "$RUN/runner-latch"; echo 'both runners did not claim work' >&2; return 1; }
        sleep 0.2
    done
    rm -f "$RUN/runner-latch"
    for n in 1 2 3 4; do
        # put stores no final newline, so read from a here-string.
        read -r run sha <<<"$(get "shared_$n")"
        wait_ci_run "$run" $((SECONDS + 300))
        taken+=" $(runner_of "$run")"
        # One claim ran it once.
        [[ $(grep -c "^$sha\$" "$RUN/runner-marks") == 1 ]] \
            || { echo "run $run for $sha left $(grep -c "^$sha\$" "$RUN/runner-marks") marks" >&2; return 1; }
    done
    echo "claimed by:$taken"
    [[ $taken == *r1* && $taken == *r2* ]]
}

t_runner_revoked() {
    local n run sha first_line deadline=$((SECONDS + 60))
    needs_ci_runs || return
    first_line=$(( $(wc -l <"$RUN/nodes/r1/server.log") + 1 ))
    server_cli cluster revoke r1
    for n in 1 2; do
        read -r run sha < <(runners_push "While r1 is revoked $n")
        [[ -n $sha ]]
        wait_ci_run "$run" $((SECONDS + 150))
        [[ $(runner_of "$run") == r2 ]]
    done
    until tail -n +"$first_line" "$RUN/nodes/r1/server.log" | grep 'CI worker could not claim a run' >/dev/null; do
        ((SECONDS < deadline)) || { echo 'r1 did not attempt a refused claim' >&2; return 1; }
        sleep 1
    done
    kill -0 "${SERVER_PIDS[r1]}"
    server_cli cluster approve r1
}

t_runner_killed() { # inline: kill -9 the runner that is running a job
    local run sha node other status message started deadline=$((SECONDS + 90))
    needs_ci_runs || return
    read -r run sha < <(runners_push 'Killed mid-job') || return 1
    [[ -n $run ]] || return 1
    until [[ $(pg_query "SELECT status FROM ci_runs WHERE run_id='$run'") == running ]] \
        && node=$(runner_of "$run" 2>/dev/null); do
        ((SECONDS < deadline)) || { echo "run $run never started" >&2; return 1; }
        sleep 1
    done
    sleep 3
    echo "kill -9 $node (pid ${SERVER_PIDS[$node]}) while it runs $run"
    kill -KILL "${SERVER_PIDS[$node]}" || return 1
    wait "${SERVER_PIDS[$node]}" 2>/dev/null
    unset "SERVER_PIDS[$node]"
    started=$SECONDS
    deadline=$((SECONDS + 200))
    while status=$(pg_query "SELECT status FROM ci_runs WHERE run_id='$run'") \
        && [[ $status == running || $status == queued ]]; do
        ((SECONDS < deadline)) || { echo "run $run is still $status" >&2; return 1; }
        sleep 2
    done
    message=$(as alice req GET "/api/ci/jobs/$run" --accept json --print json:error_message) || return 1
    echo "run $run ended '$status' $((SECONDS - started)) s after the kill: $message"
    [[ $status == failed && $message == *'stopped before the run finished'* ]] || return 1
    # The other runner keeps working.
    other=r1
    [[ $node == r1 ]] && other=r2
    read -r run sha < <(runners_push 'After the kill') || return 1
    wait_ci_run "$run" $((SECONDS + 150)) || return 1
    [[ $(runner_of "$run") == "$other" ]] || return 1
    echo "$other ran $run"
}

runners_stop() { # inline
    local node failed=0
    for node in r1 r2; do
        if [[ -n ${SERVER_PIDS[$node]:-} ]]; then stop_server "$node" || failed=1; fi
        t_log_clean "$node" || failed=1
    done
    ((failed == 0))
}

# ------------------------------------------------------------- backup steps --

# The database goes back to the copy in BACKUP; Git and uploads stay as they are.
restore_database_only() {
    local backup=$1 dropdb createdb pg_restore
    if [[ -n $DATABASE_URL ]]; then
        dropdb=$(pg_tool dropdb)
        createdb=$(pg_tool createdb)
        pg_restore=$(pg_tool pg_restore)
        pg_command "$GITSERV_TEST_DATABASE_URL" "$dropdb" --maintenance-db --force "$PG_CURRENT"
        pg_command "$GITSERV_TEST_DATABASE_URL" "$createdb" --maintenance-db "$PG_CURRENT"
        pg_command "$DATABASE_URL" "$pg_restore" --dbname --exit-on-error --single-transaction --clean --if-exists \
            --no-owner --no-privileges "$backup/database.pgdump"
    else
        rm -f "$RUN/state/gitserv.db" "$RUN/state/gitserv.db-wal" "$RUN/state/gitserv.db-shm"
        cp "$backup/database.sqlite" "$RUN/state/gitserv.db"
    fi
}

t_backup_under_load() {
    local loop
    # The README as backed up; earlier steps edited it, and the load only pushes another branch.
    put readme_sha "$(as alice req GET /alice/proj/raw/main/README.md --print sha256)"
    server_cli backup "$RUN/backup"
    rm -f "$RUN/stop-load"
    git_with_token write clone -q "$BASE/alice/proj.git" "$RUN/clone-load"
    (
        cd "$RUN/clone-load"
        git_as alice checkout -q -b load
        n=0
        until [[ -e $RUN/stop-load ]]; do
            n=$((n + 1))
            git_as alice commit -q --allow-empty -m "Load $n"
            git_with_token write push -q origin load
            as alice req POST /api/repos/alice/proj/issues --accept json --status 201 \
                --json "{\"title\":\"Load issue $n\"}" >/dev/null
        done
        echo "$n pushes and issues"
    ) >"$RUN/load.log" 2>&1 &
    loop=$!
    sleep 3
    # The second run into the same directory copies only what changed.
    server_cli backup "$RUN/backup"
    touch "$RUN/stop-load"
    wait "$loop"
    cat "$RUN/load.log"
    ls -l "$RUN/backup"
}

t_restore_into_empty() { # inline: the data moves aside, as on a new machine
    local createdb
    mv "$RUN/state" "$RUN/state.before-restore" || return 1
    mv "$RUN/repos" "$RUN/repos.before-restore" || return 1
    if [[ -n $DATABASE_URL ]]; then
        createdb=$(pg_tool createdb) || return 1
        pg_command "$GITSERV_TEST_DATABASE_URL" "$createdb" --maintenance-db "${PG_DB}_r" || return 1
        PG_OWNED+=("${PG_DB}_r")
        PG_CURRENT=${PG_DB}_r
        DATABASE_URL=$(pg_url "$PG_CURRENT") || return 1
    fi
    server_cli restore "$RUN/backup"
}

readme_matches() { # readme_matches FILE: FILE is alice/proj's README as backed up
    local sha
    sha=$(sha256sum <"$1" | cut -d' ' -f1)
    [[ $sha == "$(get readme_sha)" ]] || { echo "$1 has sha256 $sha, not the backed-up README" >&2; return 1; }
}

t_local_files_with_runners() {
    # This is a single-node installation with node transport open for runner pairing.
    # Existing local uploads must not become storage-node objects when it restarts.
    as alice req GET "/api/files/$(get note_id)/download" --accept any --body-sha256 "$(get note_sha)"
}

t_restored_works() {
    server_cli repo verify | tee "$RUN/verify.out"
    grep -q 'No problems found.' "$RUN/verify.out"
    git_with_token read clone -q "$BASE/alice/proj.git" "$RUN/clone-restored"
    readme_matches "$RUN/clone-restored/README.md"
    as alice req GET "/api/repos/alice/proj/issues/$(get issue_number)" --accept json --json-eq 'title=E2E issue'
    as alice req GET "/alice/proj/issues/$(get issue_number)" --contains 'First comment from the web form'
    cd "$RUN/clone-restored"
    git_with_token read fetch -q origin "refs/pull/$(get pr_number)/head:restored-pull"
    git_as alice checkout -q restored-pull
    grep -q 'feature work' feature.txt || { echo "the pull request checkout lacks feature.txt" >&2; return 1; }
    as alice req GET "/api/files/$(get note_id)/download" --accept any --body-sha256 "$(get note_sha)"
    needs_ci_runs || return
    as alice req GET "/alice/ci-demo/actions?job=$(get ci_run)&view=logs" --contains '| hello'
}

t_delete_kept() {
    local work=$RUN/work/doomed
    as alice req POST /api/repos --accept json --status 201 --json '{"name":"doomed"}'
    rm -rf "$work"
    mkdir -p "$work"
    cd "$work"
    git_as alice init -q -b main
    printf 'kept for a week\n' >kept.txt
    git_as alice add -A
    git_as alice commit -q -m 'Before the delete'
    git_with_token write push -q "$BASE/alice/doomed.git" main
    server_cli backup "$RUN/backup-before-delete"
    as alice req DELETE /api/repos/alice/doomed --status 204
    as alice req GET /api/repos/alice/doomed --accept json --status 404
    ls -d "$RUN"/repos/deleted/*.git
}

t_database_before_delete() {
    restore_database_only "$RUN/backup-before-delete"
    must_fail 'verify with the repository only in deleted/' 'a deleted copy is kept' server_cli repo verify
    server_cli repo verify --repair
    server_cli repo verify | grep 'No problems found.'
}

t_deleted_repo_back() {
    git_with_token read clone -q "$BASE/alice/doomed.git" "$RUN/clone-doomed"
    grep -q 'kept for a week' "$RUN/clone-doomed/kept.txt"
}

t_git_newer_prepare() {
    local tip head number late moved
    read -r tip head number < <(open_pull pitr-topic pitr.txt)
    [[ $number =~ ^[0-9]+$ ]]
    server_cli backup "$RUN/backup-t"
    # After time T: the pull request's branch moves and another pull request opens.
    moved=$(as alice req POST /api/repos/alice/proj/contents/pitr-more.txt --accept json --status 201 \
        --json "{\"content\":\"$(printf 'after T\n' | base64 -w0)\",\"message\":\"After T\",\"branch\":\"pitr-topic\",\"expected_revision\":\"$head\"}" \
        --print json:sha)
    local deadline=$((SECONDS + 30))
    until [[ $(as alice req GET "/api/repos/alice/proj/pulls/$number" --accept json --print json:head_sha) == "$moved" ]]; do
        ((SECONDS < deadline)) || { echo "pull request #$number did not follow its branch" >&2; return 1; }
        sleep 1
    done
    read -r tip head late < <(open_pull pitr-late late.txt)
    [[ $late =~ ^[0-9]+$ ]]
    put pitr_pull "$number"
    put pitr_head "$moved"
    put pitr_late "$late"
}

t_git_newer_repair() {
    restore_database_only "$RUN/backup-t"
    must_fail 'verify with Git newer than the database' "pull request #$(get pitr_pull) records" \
        server_cli repo verify
    server_cli repo verify --repair
    server_cli repo verify | grep 'No problems found.'
}

t_git_newer_works() {
    as alice req GET "/api/repos/alice/proj/pulls/$(get pitr_pull)" --accept json \
        --json-eq state=open --json-eq "head_sha=$(get pitr_head)"
    as alice req GET "/api/repos/alice/proj/pulls/$(get pitr_late)" --accept json --status 404
    [[ -z $(git_as alice ls-remote "$BASE/alice/proj.git" "refs/pull/$(get pitr_late)/*") ]]
    git_with_token read clone -q "$BASE/alice/proj.git" "$RUN/clone-after-t"
    git_as alice -C "$RUN/clone-after-t" log -1 --format=%H origin/pitr-topic | grep -q "$(get pitr_head)"
}

language_rows() {
    if [[ -n $DATABASE_URL ]]; then
        pg_query 'SELECT count(*) FROM repo_languages'
    else
        "$PYTHON" -I -c 'import sqlite3, sys
print(sqlite3.connect(sys.argv[1]).execute("SELECT count(*) FROM repo_languages").fetchone()[0])' "$RUN/state/gitserv.db"
    fi
}

t_drop_derived() {
    local pack
    shopt -s nullglob
    echo "language rows before: $(language_rows)"
    rm -f "$RUN"/repos/*.git/objects/pack/*.idx "$RUN"/repos/*.git/objects/info/commit-graph
    rm -rf "$RUN"/repos/*.git/objects/info/commit-graphs
    if [[ -n $DATABASE_URL ]]; then
        pg_query 'DELETE FROM repo_language_cache; DELETE FROM repo_languages'
    else
        "$PYTHON" -I -c 'import sqlite3, sys
db = sqlite3.connect(sys.argv[1])
db.execute("DELETE FROM repo_language_cache")
db.execute("DELETE FROM repo_languages")
db.commit()' "$RUN/state/gitserv.db"
    fi
    [[ $(language_rows) == 0 ]]
    server_cli repo rebuild
    for pack in "$RUN"/repos/*.git/objects/pack/*.pack; do [[ -e ${pack%.pack}.idx ]]; done
    (($(language_rows) > 0))
}

t_rebuilt_works() {
    # The README text t_editor saved, rendered on the repository page.
    as alice req GET /alice/proj --contains README.md --contains 'Edited in the web editor'
    as alice req GET /alice/proj/raw/main/README.md --body-sha256 "$(get readme_sha)"
    as alice req GET /api/repos/alice/proj/languages --accept json --contains-raw '"Rust"'
    as alice req GET '/search?q=proj&type=repos' --contains alice/proj
    as alice req GET '/search?q=Edited&type=code' --contains README.md
    git_with_token read clone -q "$BASE/alice/proj.git" "$RUN/clone-rebuilt"
    readme_matches "$RUN/clone-rebuilt/README.md"
    git_as alice -C "$RUN/clone-rebuilt" fsck --no-progress
}

# ------------------------------------------------------------ cluster steps --

pg_query() {
    local psql
    psql=$(pg_tool psql) || return 1
    PGCONNECT_TIMEOUT=5 PGOPTIONS='-c statement_timeout=10000' \
        pg_command "$DATABASE_URL" "$psql" --dbname -X -qAt --no-password -v ON_ERROR_STOP=1 -c "$1"
}

prepare_cluster() { # inline: distinct localhost ports, disks and declared identity
    local node index=0 keys
    local -a tcp udp
    read -r -a tcp < <(free_ports 8) || return 1
    read -r -a udp < <(free_ports 4 udp) || return 1
    for node in "${CLUSTER_NODES[@]}"; do
        HTTP_PORTS[$node]=${tcp[$((index * 2))]}
        SSH_PORTS[$node]=${tcp[$((index * 2 + 1))]}
        IROH_PORTS[$node]=${udp[$index]}
        mkdir -p "$(server_dir "$node")" || return 1
        index=$((index + 1))
    done
    : >"$RUN/.env"
    keys=$(cd "$RUN" && env -i "PATH=$PATH" "$BIN" cluster keygen) || return 1
    put store2_key "$(awk -F= '/^NODE_KEY=/{print $2}' <<<"$keys")"
    put store2_public "$(awk -F= '/^NODE_PUBLIC_KEY=/{print $2}' <<<"$keys")"
    put master_key "$("$PYTHON" -I -c 'import secrets; print(secrets.token_hex(32))')"
    [[ $(get store2_key) =~ ^[0-9a-f]{64}$ && $(get store2_public) =~ ^[0-9a-f]{64}$ ]]
}

t_cluster_join_store() {
    local ticket
    ticket=$(cluster_cli web1 invite --roles storage)
    CLUSTER_JOIN_TOKEN=$ticket cluster_cli store1 join --name store1
}

t_cluster_worker_token() {
    put worker_token "$(cluster_cli web1 invite --reusable --roles runner)"
}

t_cluster_membership() {
    local node roles listing port path
    listing=$(cluster_cli web1 list)
    printf '%s\n' "$listing"
    for node in "${CLUSTER_NODES[@]}"; do
        case $node in web1) roles=web,ssh ;; ci1) roles=runner ;; *) roles=storage ;; esac
        grep -Eq "^$node +active +$roles +" <<<"$listing"
        [[ $(pg_query "SELECT status FROM nodes WHERE name='$node'") == active ]]
        if [[ $node == store1 || $node == store2 || $node == store3 ]]; then
            # Storage exposes probes, not the web application or public metrics.
            for path in /health /ready; do
                [[ $(curl -q --noproxy '*' -sS --max-time 2 -o /dev/null -w '%{http_code}' "http://127.0.0.1:${HTTP_PORTS[$node]}$path") == 200 ]] || return 1
            done
            for path in /metrics /login /api/v3/user; do
                [[ $(curl -q --noproxy '*' -sS --max-time 2 -o /dev/null -w '%{http_code}' "http://127.0.0.1:${HTTP_PORTS[$node]}$path") == 404 ]] || return 1
            done
        fi
        if [[ $node != web1 ]]; then
            # Only web1 serves SSH; database-free workers also have no HTTP listener.
            local -a closed_ports=("${SSH_PORTS[$node]}")
            if is_runner "$node"; then closed_ports+=("${HTTP_PORTS[$node]}"); fi
            for port in "${closed_ports[@]}"; do
                if (exec 3<>"/dev/tcp/127.0.0.1/$port") 2>/dev/null; then
                    echo "$node unexpectedly exposes user port $port" >&2
                    return 1
                fi
            done
        fi
    done
    as alice page /admin/nodes --user alice --lane Nodes \
        --contains web1 --contains store1 --contains store2 --contains ci1
    as anon req GET /ready --accept json --json-eq status=ready
    [[ ! -e $RUN/nodes/ci1/state/gitserv.db ]]
}

repo_node() {
    pg_query "SELECT n.name FROM repositories r JOIN nodes n ON n.id=r.storage_node_id JOIN users u ON u.id=r.owner_user_id WHERE u.username='$1' AND r.name='$2'"
}

t_cluster_placement() {
    local name path
    for name in proj vault ci-demo; do
        [[ $(repo_node alice "$name") == store1 ]]
        path=$(pg_query "SELECT disk_path FROM repositories WHERE name='$name'")
        [[ -d $RUN/nodes/store1/repos/${path##*/} ]]
        [[ ! -e $RUN/nodes/web1/repos/${path##*/} ]]
    done
    # Both stores share one filesystem. Exclude store1 briefly so equal
    # free-space reports cannot decide which node owns the outage control repo.
    cluster_cli web1 revoke store1
    as alice req POST /api/repos --accept json --status 201 --json '{"name":"survivor"}'
    cluster_cli web1 approve store1
    [[ $(repo_node alice survivor) == store2 ]]
    as alice req POST /api/repos/alice/survivor/contents/README.md --accept json --status 201 \
        --json '{"content":"c3RvcmUyCg==","message":"Stored on store2","branch":"main","expected_revision":"unborn"}'
    as alice req GET /alice/survivor --contains 'Stored on store2'
}

t_cluster_http_push() {
    local sha
    cd "$RUN/clone-http"
    printf 'pushed through web1 to store1\n' >http.txt
    git_as alice add http.txt
    git_as alice commit -q -m 'HTTP push to remote storage'
    git_with_token write push -q origin main
    sha=$(git_as alice rev-parse HEAD)
    put main_sha "$sha"
    git_with_token read clone -q "$BASE/alice/proj.git" "$RUN/clone-second-http"
    git_as alice clone -q "$SSH_URL/alice/proj.git" "$RUN/clone-second-ssh"
    for kind in http ssh; do
        [[ $(git_as alice -C "$RUN/clone-second-$kind" rev-parse HEAD) == "$sha" ]]
        grep -q 'pushed through web1 to store1' "$RUN/clone-second-$kind/http.txt"
    done
}

t_cluster_ci_push() {
    as alice req PUT /api/repos/alice/ci-demo/secrets/CLUSTER_SECRET --status 204 \
        --json '{"value":"cluster-only-test-secret"}'
    cat >"$RUN/cluster-ci.yml" <<'YAML'
name: E2E CI
on: [push, pull_request]
jobs:
  hello:
    runs-on: self-hosted
    steps:
      - run: |
          if [ "$EVENT" = pull_request ]; then
            test -z "$CLUSTER_SECRET"
            echo CLUSTER_FORK_NO_SECRETS
          else
            test "$CLUSTER_SECRET" = cluster-only-test-secret
            echo hello
            echo "context ${{ github.ref }} ${{ github.repository }} ${{ github.actor }}"
          fi
        env:
          EVENT: ${{ github.event_name }}
          CLUSTER_SECRET: ${{ secrets.CLUSTER_SECRET }}
YAML
    t_ci_push "$RUN/cluster-ci.yml"
}

assert_ci_worker() {
    local id=$1
    [[ $(pg_query "SELECT split_part(worker, '/', 1) FROM ci_runs WHERE run_id='$id'") == \
        "$(pg_query "SELECT public_key FROM nodes WHERE name='ci1'")" ]]
}

t_cluster_ci_result() {
    t_ci_result
    assert_ci_worker "$(get ci_run)"
    as alice req GET "/alice/ci-demo/actions?job=$(get ci_run)&view=logs" --no-scripts --contains hello --contains success
}

t_cluster_fork_ci() {
    local fork tip head pr id count
    tip=$(get ci_sha)
    fork=$(as bob req POST /api/repos/alice/ci-demo/forks --accept json --status 201 --print json:repo.id)
    [[ $(repo_node bob ci-demo) == "$(repo_node alice ci-demo)" ]]
    head=$(as bob req POST /api/repos/bob/ci-demo/contents/fork.txt --accept json --status 201 \
        --json "{\"content\":\"Zm9yayBjaQo=\",\"message\":\"Fork CI\",\"branch\":\"main\",\"expected_revision\":\"$tip\"}" --print json:sha)
    pr=$(as bob req POST /api/repos/alice/ci-demo/pulls --accept json --status 201 \
        --json "{\"title\":\"Fork workflow approval\",\"head_repo_id\":$fork,\"head_branch\":\"main\",\"base_branch\":\"main\"}" --print json:number)
    as alice req GET "/alice/ci-demo/pulls/$pr" --contains 'Run workflows'
    as bob req POST "/api/repos/alice/ci-demo/pulls/$pr/workflows" --accept json --status 404 \
        --json "{\"expected_head\":\"$head\"}"
    # The fork run is kept waiting for approval and no worker claims it. Give the worker several polls.
    sleep 7
    count=$(pg_query "SELECT count(*) || '/' || count(*) FILTER (WHERE approval_required AND worker IS NULL) FROM ci_runs WHERE head_sha='$head' AND event='pull_request'")
    expect_eq 'one fork run waits for approval, unclaimed' "$count" 1/1
    as alice req POST "/api/repos/alice/ci-demo/pulls/$pr/workflows" --accept json \
        --json "{\"expected_head\":\"$head\"}"
    id=$(pg_query "SELECT run_id FROM ci_runs WHERE head_sha='$head' AND event='pull_request' AND is_fork")
    [[ $id =~ ^[0-9a-f-]{36}$ ]]
    needs_ci_runs || return
    wait_ci_run "$id"
    assert_ci_worker "$id"
    as alice req GET "/alice/ci-demo/actions?job=$id&view=logs" --contains 'CLUSTER_FORK_NO_SECRETS'
    as alice req GET "/alice/ci-demo/actions?job=$id&view=logs" --no-scripts --contains CLUSTER_FORK_NO_SECRETS
    as alice req GET "/api/repos/alice/ci-demo/commits/$head/status" --accept json \
        --json-eq state=success --json-eq total_count=1 --json-eq statuses.0.context=gitserv/workflow/.github/workflows/ci.yml
}

# A replica is a second copy of a repository on another storage node. store1 keeps
# the copy current; once store1 has been down for a minute the copy takes the
# repository over and takes the pushes.
replica_dir() { # replica_dir: the repository's directory name
    local path
    path=$(pg_query "SELECT disk_path FROM repositories WHERE name='replica'")
    printf '%s\n' "${path##*/}"
}

replica_current() { # replica_current NODE: t once the copy on NODE has every write
    pg_query "SELECT rr.applied_version = r.write_version FROM repository_replicas rr JOIN repositories r ON r.id=rr.repository_id JOIN nodes n ON n.id=rr.node_id WHERE r.name='replica' AND n.name='$1'"
}

t_replica_create() { # store2 is revoked while the repository is made, so store1 is its primary
    local deadline=$((SECONDS + 60)) dir output
    output=$(cluster_cli web1 replicas set 2)
    grep -q '^Each repository keeps 2 copies' <<<"$output"
    cluster_cli web1 revoke store2
    as alice req POST /api/repos --accept json --status 201 --json '{"name":"replica"}'
    cluster_cli web1 approve store2
    [[ $(repo_node alice replica) == store1 ]]
    as alice req POST /api/repos/alice/replica/contents/README.md --accept json --status 201 \
        --json '{"content":"cmVwbGljYQo=","message":"First copy","branch":"main","expected_revision":"unborn"}'
    dir=$(replica_dir)
    until [[ $(replica_current store2) == t ]]; do
        ((SECONDS < deadline)) || { echo 'store2 did not keep a current copy' >&2; return 1; }
        sleep 1
    done
    [[ $(git --git-dir "$RUN/nodes/store2/repos/mirrors/$dir" rev-parse refs/heads/main) == \
        $(git --git-dir "$RUN/nodes/store1/repos/$dir" rev-parse refs/heads/main) ]]
    git_with_token read clone -q "$BASE/alice/replica.git" "$RUN/clone-replica-outage"
}

t_replica_take_over() { # store1 stays down: store2 takes the repository over by itself
    local deadline=$((SECONDS + 240)) dir
    dir=$(replica_dir)
    until [[ $(repo_node alice replica) == store2 ]]; do
        ((SECONDS < deadline)) || { echo 'store2 did not take the repository over' >&2; return 1; }
        sleep 2
    done
    until [[ -d $RUN/nodes/store2/repos/$dir ]]; do
        ((SECONDS < deadline)) || { echo 'store2 did not move the copy into place' >&2; return 1; }
        sleep 1
    done
    grep -q 'took over the repository from a current copy' "$RUN/nodes/store2/server.log"
}

t_replica_push() { # a clone and a push through web1 reach the promoted store2
    local sha
    git_with_token read clone -q "$BASE/alice/replica.git" "$RUN/clone-replica"
    cd "$RUN/clone-replica"
    printf 'written after the promotion\n' >promoted.txt
    git_as alice add promoted.txt
    git_as alice commit -q -m 'Push after promotion'
    version=$(pg_query "SELECT write_version FROM repositories WHERE name='replica'")
    git_with_token write push -q origin main
    [[ $(pg_query "SELECT write_version FROM repositories WHERE name='replica'") -gt $version ]]
    sha=$(git_as alice rev-parse HEAD)
    [[ $(git --git-dir "$RUN/nodes/store2/repos/$(replica_dir)" rev-parse refs/heads/main) == "$sha" ]]
    as alice req GET /api/repos/alice/replica/commits --accept json --json-has '0.message=Push after promotion'
}

t_replica_catch_up() { # store1 returns as a copy of the promoted repository and follows it
    local deadline=$((SECONDS + 90)) dir
    dir=$(replica_dir)
    until [[ $(replica_current store1) == t ]]; do
        ((SECONDS < deadline)) || { echo 'store1 did not catch up as a copy' >&2; return 1; }
        sleep 1
    done
    [[ -d $RUN/nodes/store1/repos/mirrors/$dir ]]
    [[ ! -e $RUN/nodes/store1/repos/$dir ]]
    [[ $(git --git-dir "$RUN/nodes/store1/repos/mirrors/$dir" rev-parse refs/heads/main) == \
        $(git --git-dir "$RUN/nodes/store2/repos/$dir" rev-parse refs/heads/main) ]]
}

t_replica_hand_back() { # a person moves the repository back to store1 while both nodes are up
    local deadline=$((SECONDS + 60)) dir
    dir=$(replica_dir)
    cluster_cli web1 promote alice/replica --to store1
    [[ $(repo_node alice replica) == store1 ]]
    until [[ -d $RUN/nodes/store1/repos/$dir && -d $RUN/nodes/store2/repos/mirrors/$dir ]]; do
        ((SECONDS < deadline)) || { echo 'the directories did not change places' >&2; return 1; }
        sleep 1
    done
    as alice req GET /alice/replica --contains 'replica'
    as alice req GET /api/repos/alice/replica/commits --accept json --status 200
}

t_replica_outage_reads() { # store1 is down: web1 answers a page, an API read and clones of alice/replica from its copy
    as alice req GET /alice/replica --contains 'replica'
    as alice req GET /api/repos/alice/replica/commits --accept json --json-has '0.message=First copy'
    git_with_token read clone -q "$BASE/alice/replica.git" "$RUN/clone-replica-outage-http"
    git_as alice clone -q "$SSH_URL/alice/replica.git" "$RUN/clone-replica-outage-ssh"
    grep -q replica "$RUN/clone-replica-outage-http/README.md"
    grep -q replica "$RUN/clone-replica-outage-ssh/README.md"
    grep -q 'primary unreachable; read served by a copy' "$RUN/nodes/web1/server.log"
}

t_replica_outage_push() { # store1 is down: a push is a write, so it is refused and the copy does not move
    local before
    cd "$RUN/clone-replica-outage"
    before=$(git_as alice rev-parse HEAD)
    printf 'written while store1 is down\n' >outage.txt
    git_as alice add outage.txt
    git_as alice commit -q -m 'Push while store1 is down'
    must_fail 'HTTP push while store1 is stopped' '503' git_with_token write push -q origin main
    grep -q 503 "$RUN/must-fail.out"
    [[ $(git --git-dir "$RUN/nodes/store2/repos/mirrors/$(replica_dir)" rev-parse refs/heads/main) == "$before" ]]
}

t_cluster_outage_no_copy() { # store1 is down and store2 is revoked: alice/replica has no usable copy, so 503
    cluster_cli web1 revoke store2
    (
        # Approve store2 again on exit, even after a failed check. The checks are not on the
        # left of `||`, so errexit stops the subshell at the first failure.
        trap 'cluster_cli web1 approve store2' EXIT
        set -euo pipefail
        as alice req GET /alice/replica --status 503 --contains 'unavailable'
        as alice req GET /api/repos/alice/replica/commits --accept json --status 503 --contains-raw 'storage node unavailable'
        as anon req GET '/alice/replica.git/info/refs?service=git-upload-pack' --status 503 --contains-raw 'storage node unavailable'
        must_fail 'HTTP clone with no usable copy' '503' git_with_token read clone -q "$BASE/alice/replica.git" "$RUN/clone-revoked-http"
        grep -q 503 "$RUN/must-fail.out"
        must_fail 'SSH clone with no usable copy' 'storage node unavailable' git_as alice clone -q "$SSH_URL/alice/replica.git" "$RUN/clone-revoked-ssh"
    )
}

t_cluster_outage_survivor() {
    as alice req GET /alice/survivor --contains 'Stored on store2'
    as alice req GET /api/repos/alice/survivor/commits --accept json --json-has '0.message=Stored on store2'
    git_with_token read clone -q "$BASE/alice/survivor.git" "$RUN/clone-survivor-http"
    git_as alice clone -q "$SSH_URL/alice/survivor.git" "$RUN/clone-survivor-ssh"
    grep -q store2 "$RUN/clone-survivor-http/README.md"
    grep -q store2 "$RUN/clone-survivor-ssh/README.md"
    as anon req GET /ready --accept json --json-eq status=ready
}

t_cluster_revoke_worker() {
    local n sha id first_line deadline=$((SECONDS + 45))
    first_line=$(( $(wc -l <"$RUN/nodes/ci1/server.log") + 1 ))
    cluster_cli web1 revoke ci1
    git_with_token read clone -q "$BASE/alice/ci-demo.git" "$RUN/clone-revocation"
    cd "$RUN/clone-revocation"
    for n in 1 2; do
        git_as alice commit -q --allow-empty -m "Queued while ci1 is revoked $n"
        git_with_token write push -q origin main
        sha=$(git_as alice rev-parse HEAD)
        id=$(pg_query "SELECT run_id FROM ci_runs WHERE head_sha='$sha' AND event='push'")
        [[ $id =~ ^[0-9a-f-]{36}$ ]]
        put "revoked_run_$n" "$id"
    done
    sleep 10
    until tail -n +"$first_line" "$RUN/nodes/ci1/server.log" | grep 'CI worker could not claim a run' >/dev/null; do
        ((SECONDS < deadline)) || { echo 'ci1 did not attempt a refused claim' >&2; return 1; }
        sleep 1
    done
    kill -0 "${SERVER_PIDS[ci1]}"
    for n in 1 2; do
        id=$(get "revoked_run_$n")
        [[ $(pg_query "SELECT status || ':' || coalesce(worker, '') FROM ci_runs WHERE run_id='$id'") == queued: ]]
        as alice req GET "/api/ci/jobs/$id" --accept json --json-eq status=queued
    done
    tail -n +"$first_line" "$RUN/nodes/ci1/server.log"
}

t_cluster_approve_worker() {
    local n id
    cluster_cli web1 approve ci1
    needs_ci_runs || return
    for n in 1 2; do
        id=$(get "revoked_run_$n")
        wait_ci_run "$id"
        assert_ci_worker "$id"
        as alice req GET "/alice/ci-demo/actions?job=$id&view=logs" --contains '| hello'
        as alice req GET "/alice/ci-demo/actions?job=$id&view=logs" --no-scripts --contains hello --contains success
    done
}

t_cluster_github_clients() {
    local ticket old_pid=${SERVER_PIDS[web1]} rc=0 skew
    command -v faketime >/dev/null || { echo 'faketime is required for the five-minute clock skew check' >&2; return 1; }
    skew=$(( $(FAKETIME_DONT_FAKE_MONOTONIC=1 faketime -f '+5m' date +%s) - $(date +%s) ))
    ((skew >= 299 && skew <= 301)) || { echo 'The test clock did not advance five minutes' >&2; return 1; }
    local -a ports
    read -r -a ports < <(free_ports 2)
    HTTP_PORTS[web2]=${ports[0]} SSH_PORTS[web2]=${ports[1]}
    IROH_PORTS[web2]=$(free_ports 1 udp)
    ticket=$(cluster_cli web1 invite --roles web,ssh) || return 1
    CLUSTER_JOIN_TOKEN=$ticket cluster_cli web2 join --name web2 || return 1
    BASE=http://127.0.0.1:${HTTP_PORTS[web2]}
    SSH_URL=ssh://git@127.0.0.1:${SSH_PORTS[web2]}
    start_server web2 || return 1
    CLUSTER_NODES+=(web2)
    # The helper drives real gh and Git clients through web2, kills web1 during
    # traffic, and verifies every acknowledged issue and clone afterward.
    E2E_STOP_WEB_PID=$old_pid t_github_clients || rc=$?
    if ! kill -0 "$old_pid" 2>/dev/null; then
        wait "$old_pid" 2>/dev/null || true
        unset 'SERVER_PIDS[web1]'
        start_server web1 || rc=1
    else
        BASE=http://127.0.0.1:${HTTP_PORTS[web1]}
        SSH_URL=ssh://git@127.0.0.1:${SSH_PORTS[web1]}
    fi
    ((rc == 0))
}

# Existing asynchronous repositories keep their fixtures. New repositories use three
# voters, with real client publications and process death immediately after ack.
wait_quorum_ready() {
    local name=$1 ready earliest=$((SECONDS + 10)) deadline=$((SECONDS + 120))
    # Each fault starts from three current voters. HTTP readiness alone does not
    # mean a restarted storage process has caught up, or that failed-dial caches expired.
    while ((SECONDS < deadline)); do
        ready=$(pg_query "SELECT COUNT(*) FROM repositories r JOIN users u ON u.id=r.owner_user_id JOIN repository_write_control c ON c.repository_id=r.id WHERE u.username='alice' AND r.name='$name' AND c.serving=1 AND (SELECT COUNT(*) FROM repository_voters v WHERE v.repository_id=r.id)=3 AND NOT EXISTS(SELECT 1 FROM repository_voters v WHERE v.repository_id=r.id AND (v.term<>c.term OR v.applied_version<>c.committed_version)) AND NOT EXISTS(SELECT 1 FROM repository_writes w WHERE w.repository_id=r.id AND w.finished=0)") || return 1
        [[ $ready != 1 || $SECONDS -lt $earliest ]] || return 0
        sleep 1
    done
    echo 'Three voting copies did not become current before the next fault' >&2
    return 1
}

t_cluster_quorum_primary_loss() {
    local transport name source primary pid expected destination deadline ticket
    local -a ports
    read -r -a ports < <(free_ports 2)
    HTTP_PORTS[store3]=${ports[0]} SSH_PORTS[store3]=${ports[1]}
    IROH_PORTS[store3]=$(free_ports 1 udp)
    ticket=$(cluster_cli web1 invite --roles storage) || return 1
    CLUSTER_JOIN_TOKEN=$ticket cluster_cli store3 join --name store3 || return 1
    CLUSTER_NODES+=(store3)
    start_server store3 || return 1
    pg_query "UPDATE node_configuration SET write_quorum='majority',replicas='2' WHERE id=1" >/dev/null || return 1
    for transport in http ssh; do
        name=quorum-survival-$transport
        as alice req POST /api/repos --accept json --status 201 --json "{\"name\":\"$name\",\"is_private\":true}" || return 1
        source=$RUN/work/$name
        mkdir -p "$source" || return 1
        git_as alice -C "$source" init -q -b main || return 1
        printf 'acknowledged over %s\n' "$transport" >"$source/README.md" || return 1
        git_as alice -C "$source" add README.md || return 1
        git_as alice -C "$source" commit -qm 'A quorum holds this commit' || return 1
        expected=$(git_as alice -C "$source" rev-parse HEAD) || return 1
        primary=$(repo_node alice "$name") || return 1
        [[ $primary == store1 || $primary == store2 || $primary == store3 ]] || return 1
        pid=${SERVER_PIDS[$primary]:-}
        [[ $pid =~ ^[0-9]+$ ]] || { echo 'The storage primary has no owned process' >&2; return 1; }
        if [[ $transport == http ]]; then
            git_with_token write -C "$source" push -q "$BASE/alice/$name.git" main || return 1
            kill -KILL "$pid" || return 1
        else
            git_as alice -C "$source" push -q "$SSH_URL/alice/$name.git" main || return 1
            kill -KILL "$pid" || return 1
        fi
        wait "$pid" 2>/dev/null || true
        unset "SERVER_PIDS[$primary]"
        destination=$RUN/clone-$name
        deadline=$((SECONDS + 120))
        until git_with_token read clone -q "$BASE/alice/$name.git" "$destination"; do
            ((SECONDS < deadline)) || { echo 'An acknowledged commit was not cloneable within two minutes' >&2; return 1; }
            [[ ! -e $destination ]] || rm -rf -- "$destination"
            sleep 1
        done
        [[ $(git_as alice -C "$destination" rev-parse HEAD) == "$expected" ]] || return 1
        git_as alice clone -q "$SSH_URL/alice/$name.git" "$destination-ssh" || return 1
        [[ $(git_as alice -C "$destination-ssh" rev-parse HEAD) == "$expected" ]] || return 1
        start_server "$primary" || return 1
        wait_quorum_ready "$name" || return 1
    done
}

t_cluster_quorum_other_publishers() {
    local mode primary pid branch name=quorum-survival-http
    for mode in editor branch merge; do
        primary=$(repo_node alice "$name") || return 1
        [[ $primary == store1 || $primary == store2 || $primary == store3 ]] || return 1
        pid=${SERVER_PIDS[$primary]:-}
        [[ $pid =~ ^[0-9]+$ ]] || return 1
        "$GITHUB_PYTHON" -I "$GITHUB_HELPER" --base "$BASE" --run "$RUN" \
            --durability "$mode" --repository "alice/$name" --primary-pid "$pid" || return 1
        branch=main
        [[ $mode != branch ]] || branch=durable
        git_as alice clone -q --branch "$branch" "$SSH_URL/alice/$name.git" "$RUN/durability-$mode-ssh" || return 1
        [[ $(git_as alice -C "$RUN/durability-$mode-ssh" rev-parse HEAD) == \
           $(git_as alice -C "$RUN/durability-$mode/clone" rev-parse HEAD) ]] || return 1
        wait "$pid" 2>/dev/null || true
        unset "SERVER_PIDS[$primary]"
        start_server "$primary" || return 1
        wait_quorum_ready "$name" || return 1
    done
}

t_cluster_quorum_no_majority() {
    local name=quorum-survival-http source=$RUN/work/quorum-survival-http
    local primary node before expected deadline
    local -a followers=()
    primary=$(repo_node alice "$name") || return 1
    git_with_token read -C "$source" fetch -q "$BASE/alice/$name.git" main || return 1
    git_as alice -C "$source" merge --ff-only -q FETCH_HEAD || return 1
    before=$(git_as alice -C "$source" rev-parse HEAD) || return 1
    for node in store1 store2 store3; do
        [[ $node != "$primary" ]] || continue
        followers+=("$node")
        stop_server "$node" || return 1
    done
    printf 'A majority must hold this commit\n' >"$source/majority.txt"
    git_as alice -C "$source" add majority.txt || return 1
    git_as alice -C "$source" commit -qm 'Require a durable majority' || return 1
    expected=$(git_as alice -C "$source" rev-parse HEAD) || return 1
    must_fail 'two stopped voters refuse a push' 'write quorum unavailable|remote rejected|503' \
        git_with_token write -C "$source" push -q "$BASE/alice/$name.git" main || return 1
    [[ $(git_with_token read ls-remote "$BASE/alice/$name.git" refs/heads/main | cut -f1) == "$before" ]] || return 1
    start_server "${followers[0]}" || return 1
    deadline=$((SECONDS + 60))
    until git_with_token write -C "$source" push -q "$BASE/alice/$name.git" main; do
        ((SECONDS < deadline)) || { echo 'One stopped voter prevented a majority write' >&2; return 1; }
        sleep 1
    done
    [[ $(git_with_token read ls-remote "$BASE/alice/$name.git" refs/heads/main | cut -f1) == "$expected" ]] || return 1
    start_server "${followers[1]}" || return 1
}

t_cluster_storage_loss() {
    local name=quorum-survival-http source=$RUN/work/quorum-survival-http expected primary pid mode destination deadline
    for mode in clone push; do
        "$PYTHON" -I - "$source/loss-$mode.bin" <<'PYDATA' || return 1
import os, sys
with open(sys.argv[1], 'wb') as output:
    for _ in range(20):
        output.write(os.urandom(1024 * 1024))
PYDATA
        git_as alice -C "$source" add "loss-$mode.bin" || return 1
        git_as alice -C "$source" commit -qm "Storage loss during $mode" || return 1
        if [[ $mode == clone ]]; then
            git_with_token write -C "$source" push -q "$BASE/alice/$name.git" main || return 1
            expected=$(git_as alice -C "$source" rev-parse HEAD)
        fi
        # Startup publication does not clear a frontend's short unreachable-peer
        # backoff. Require a healthy, non-mutating write advertisement before the fault.
        deadline=$((SECONDS + 60))
        until git_with_token write -C "$source" push --dry-run -q "$BASE/alice/$name.git" main; do
            ((SECONDS < deadline)) || { echo 'Storage did not recover write advertisement before the fault' >&2; return 1; }
            sleep 1
        done
        primary=$(repo_node alice "$name") || return 1
        [[ $primary == store1 || $primary == store2 || $primary == store3 ]] || return 1
        pid=${SERVER_PIDS[$primary]:-}
        [[ $pid =~ ^[0-9]+$ ]] || return 1
        "$GITHUB_PYTHON" -I "$GITHUB_HELPER" --base "$BASE" --run "$RUN" \
            --storage-loss "$mode" --repository "alice/$name" --primary-pid "$pid" --source "$source" || return 1
        wait "$pid" 2>/dev/null || true
        unset "SERVER_PIDS[$primary]"
        destination=$RUN/clone-after-$mode-loss
        git_with_token read clone -q "$BASE/alice/$name.git" "$destination" || return 1
        [[ $(git_as alice -C "$destination" rev-parse HEAD) == "$expected" ]] || return 1
        start_server "$primary" || return 1
        # Read fallback proves durability, not that the primary can accept the next
        # publisher. Finish recovery before the next fault or editor write.
        wait_quorum_ready "$name" || return 1
    done
}

t_cluster_graceful_shutdown() {
    needs_ci_runs || return
    local source=$RUN/work/shutdown-ci name=quorum-survival-http publisher=$RUN/work/quorum-survival-http
    local id deadline child primary pid old new reply status
    as alice req POST /api/repos --accept json --status 201 --json '{"name":"shutdown-ci","is_private":true}'
    mkdir -p "$source/.github/workflows"
    cat >"$source/.github/workflows/shutdown.yml" <<'YAML'
name: Shutdown ownership
on: [push]
jobs:
  running:
    runs-on: self-hosted
    steps:
      - run: |
          sleep 240 &
          child=$!
          echo "OWNED_SHUTDOWN_CHILD=$child"
          wait "$child"
YAML
    git_as alice -C "$source" init -q -b main || return 1
    git_as alice -C "$source" add .github/workflows/shutdown.yml || return 1
    git_as alice -C "$source" commit -qm 'A running job owns its child during shutdown' || return 1
    git_with_token write -C "$source" push -q "$BASE/alice/shutdown-ci.git" main || return 1
    deadline=$((SECONDS + 90))
    until id=$(as alice req GET '/api/ci/jobs?owner=alice&repo=shutdown-ci' --accept json --print json:jobs.0.id 2>/dev/null) && \
          child=$(as alice req GET "/alice/shutdown-ci/actions?job=$id&view=logs" --print 're:OWNED_SHUTDOWN_CHILD=([0-9]+)' 2>/dev/null); do
        ((SECONDS < deadline)) || { echo 'The owned CI child did not start' >&2; return 1; }
        sleep 1
    done
    [[ $id =~ ^[0-9a-f-]{36}$ && $child =~ ^[0-9]+$ ]]
    as alice req GET "/api/ci/jobs/$id" --accept json --json-eq status=running
    git_with_token read -C "$publisher" fetch -q "$BASE/alice/$name.git" main || return 1
    git_as alice -C "$publisher" merge --ff-only -q FETCH_HEAD || return 1
    old=$(git_with_token read ls-remote "$BASE/alice/$name.git" refs/heads/main | cut -f1)
    # A fresh 20 MiB object guarantees the pause occurs before the push is complete.
    "$PYTHON" -I - "$publisher/shutdown.bin" <<'PYDATA' || return 1
import os, sys
with open(sys.argv[1], 'wb') as output:
    for _ in range(20):
        output.write(os.urandom(1024 * 1024))
PYDATA
    git_as alice -C "$publisher" add shutdown.bin || return 1
    git_as alice -C "$publisher" commit -qm 'A push is active during SIGTERM' || return 1
    new=$(git_as alice -C "$publisher" rev-parse HEAD)
    primary=$(repo_node alice "$name")
    pid=${SERVER_PIDS[$primary]}
    reply=$("$GITHUB_PYTHON" -I "$GITHUB_HELPER" --base "$BASE" --run "$RUN" \
        --storage-loss shutdown --repository "alice/$name" --primary-pid "$pid" \
        --runner-pid "${SERVER_PIDS[ci1]}" --source "$publisher") || return 1
    printf '%s\n' "$reply"
    wait "$pid" || return 1
    wait "${SERVER_PIDS[ci1]}" || return 1
    unset "SERVER_PIDS[$primary]" 'SERVER_PIDS[ci1]'
    as alice req GET "/api/ci/jobs/$id" --accept json --json-eq status=failed
    [[ -n $(as alice req GET "/api/ci/jobs/$id" --accept json --print json:error_message) ]]
    ! kill -0 "$child" 2>/dev/null || { echo 'CI leaked its owned child after shutdown' >&2; return 1; }
    git_with_token read clone -q "$BASE/alice/$name.git" "$RUN/clone-after-shutdown" || return 1
    status=$(git_as alice -C "$RUN/clone-after-shutdown" rev-parse HEAD)
    if [[ $reply == *'ack=True'* ]]; then
        [[ $status == "$new" ]] || return 1
    else
        [[ $status == "$old" || $status == "$new" ]] || return 1
    fi
    start_server "$primary" || return 1
    start_server ci1 || return 1
    # Another write proves both the repository locks and quorum admission were released.
    git_as alice -C "$publisher" commit -q --allow-empty -m 'Write after graceful shutdown' || return 1
    git_with_token write -C "$publisher" push -q "$BASE/alice/$name.git" main
}

t_cluster_drain_worker() {
    local sha id deadline=$((SECONDS + 45))
    cluster_cli web1 drain ci1
    until [[ $(pg_query "SELECT state FROM nodes WHERE name='ci1'") == drained ]]; do
        ((SECONDS < deadline)) || { echo 'runner drain did not finish' >&2; return 1; }
        as anon req GET /ready --accept json --json-eq status=ready
    done
    git_with_token read clone -q "$BASE/alice/ci-demo.git" "$RUN/clone-drain"
    cd "$RUN/clone-drain"
    git_as alice commit -q --allow-empty -m 'Queued while the runner is drained'
    git_with_token write push -q origin main
    sha=$(git_as alice rev-parse HEAD)
    id=$(pg_query "SELECT run_id FROM ci_runs WHERE head_sha='$sha' AND event='push'")
    [[ $id =~ ^[0-9a-f-]{36}$ ]]
    for _ in 1 2 3 4 5; do
        as anon req GET /ready --accept json --json-eq status=ready
        [[ $(pg_query "SELECT status FROM ci_runs WHERE run_id='$id'") == queued ]]
    done
    cluster_cli web1 drain ci1 --cancel
    deadline=$((SECONDS + 60))
    until [[ $(pg_query "SELECT worker IS NOT NULL FROM ci_runs WHERE run_id='$id'") == t ]]; do
        ((SECONDS < deadline)) || { echo 'the runner did not claim the run after the cancel' >&2; return 1; }
        sleep 1
    done
    assert_ci_worker "$id"
    needs_ci_runs || return
    wait_ci_run "$id"
}

t_cluster_browser() {
    "$GITHUB_PYTHON" -I "$ROOT/tests/browser/test-cluster-browser.py" "$BASE" "$RUN/jar-alice.json" ci1 \
        "http://127.0.0.1:${HTTP_PORTS[web2]}" "$RUN/jar-bob.json"
}

t_cluster_shutdown() { # inline: attempt every stop even when one fails
    local node failed=0
    local -a nodes=("$@")
    ((${#nodes[@]})) || nodes=(ci1 store1 store2 web1)
    for node in "${nodes[@]}"; do
        stop_server "$node" || failed=1
        t_log_clean "$node" || failed=1
    done
    if [[ -n ${SERVER_PIDS[store3]:-} ]]; then
        stop_server store3 || failed=1
        t_log_clean store3 || failed=1
    fi
    if [[ -n ${SERVER_PIDS[web2]:-} ]]; then
        stop_server web2 || failed=1
        t_log_clean web2 || failed=1
    fi
    ((failed == 0))
}

# ------------------------------------------------------------------ rounds --

run_round() {
    local db=$1
    ROUND=$db
    ROUND_BLOCKED=0
    STEP_NO=0
    RUN=$WORK/$db
    STATE=$RUN/state-files
    HTTP_PORT=''
    SSH_PORT=''
    BASE=''
    SSH_URL=''
    DATABASE_URL='' DATABASE_DIRECT_URL=''
    DATABASE_READ_URL=''
    SERVER_ROLES=''
    SERVER_IROH=''
    ACTION_OBJECTS_ORIGIN=''
    mkdir -p "$RUN/home" "$RUN/keys" "$RUN/work" "$STATE"
    PW_ALICE=Alice-e2e-$(random_hex)
    PW_BOB=Bob-e2e-$(random_hex)
    put master_key "$("$PYTHON" -I -c 'import secrets; print(secrets.token_hex(32))')"

    step -f -i 'database is ready' prepare_database
    if [[ $db == pgbouncer ]]; then
        local pool_mode=${E2E_PGBOUNCER_MODE:-transaction} pool_size=2
        [[ $pool_mode != session ]] || pool_size=96
        step -f -i "PgBouncer starts with $pool_mode pooling and $pool_size backend connections" start_pgbouncer
    fi
    if [[ $db == replica ]]; then step -f -i 'physical read replica starts' start_read_replica; fi
    step -f -i 'server starts and /health answers' start_server
    step -f 'ssh and test keys are generated' t_keys
    step -f 'setup: first-run admin through the HTML form' t_setup
    step -f 'users: sign up, sign out and sign in through HTML forms' t_users
    if [[ $db == pgbouncer || $db == replica ]]; then
        step -f "repositories: create public and private repositories with $db" t_repos
        step -f "ssh keys: created with $db" t_ssh_keys
        step -f "tokens: created with $db" t_tokens
        step "git: push with $db" t_push
        step "git: clone and HTTP push with $db" t_clone
        step "issues: create, read and comment with $db" t_issues
        if [[ $db == replica ]]; then step 'replica: gh reads after writes, lag and stopped replica fallback' t_read_replica; fi
        ROUND_BLOCKED=0
        step 'log: the server log has no panic' t_log_clean
        step -i 'shutdown: SIGTERM exits with status 0' stop_server
        terminate_servers
        stop_pgbouncer
        stop_read_replica
        step -i 'disposable database removed' drop_pg
        return
    fi
    if [[ ${E2E_ACTIONS_ONLY:-0} == 1 ]]; then
        step -f 'tokens: created for real actions' t_tokens
        step -f -i 'actions: restart with the upstream local test origin and real Results URL' actions_restart_server
        step 'actions: real cache save, restore, miss, repository isolation and cross-job artifacts' t_action_objects
        ROUND_BLOCKED=0
        step 'log: the server log has no panic' t_log_clean
        step -i 'shutdown: SIGTERM exits with status 0' stop_server
        terminate_servers
        step -i 'disposable database removed' drop_pg
        return
    fi
    if [[ ${E2E_GITHUB_ONLY:-0} == 1 ]]; then
        step -f 'organization: created for real GitHub clients' t_org
        step -f 'tokens: created for real GitHub clients' t_tokens
        step 'real clients: GHES gh commands, native github-script and schema-checked webhooks' t_github_clients
        gh_sweep_step
        ROUND_BLOCKED=0
        step 'log: the server log has no panic' t_log_clean
        step -i 'shutdown: SIGTERM exits with status 0' stop_server
        terminate_servers
        step -i 'disposable database removed' drop_pg
        return
    fi
    step 'csrf: forged token and foreign origin refused, profile saves' t_csrf
    step -f 'repositories: web form, JSON API, private repository hidden' t_repos
    step 'organization: created through the HTML form' t_org
    step 'organization: repository owned by it, created in the /new form' t_org_repo
    step 'ssh keys: added through the settings form' t_ssh_keys
    step 'tokens: created through the settings form; values shown once' t_tokens
    step 'git: initial commit pushed over SSH, visible in web and API' t_push
    step 'git: anonymous and token clones; chunked HTTP push; read token refused' t_clone
    if [[ $db == sqlite ]]; then
        step 'git: multi-round fetches, 150 MiB SSH/HTTP push and clone, bounded server memory' t_git_streaming
    fi
    step 'ci: workflow pushed over HTTP with a token (queues a run)' t_ci_push
    step 'editor: HTML save, stale save keeps the draft, new file' t_editor
    step 'pull request: branch and PR via API, merge via API' t_pull_request
    step 'pull request: fork branch merged into upstream through API' t_fork_pull_request
    step 'branch protection: push, editor and API refused; PR needs a writer approval' t_protection
    step 'pull request: follows its base branch; the page form merges' t_pr_base
    step 'pull request: follows its head branch (editor save and SSH push)' t_pr_head
    step 'pull request: deleted head blocks the merge; closed is not followed' t_pr_lifecycle
    step 'pull request: a new push needs a fresh approval' t_pr_approval
    step 'pull request: draft becomes ready (page form and API)' t_pr_draft
    step 'pull request: allowed merge methods, squash and rebase results, refusals by code' t_pr_merge_methods
    step 'pull request: stack of three follows main on rebase, lands in one merge' t_pr_stack
    step 'pull request: review, inline comment, resolve and reviewer requests on the pages' t_pr_review_pages
    step 'mentions: notification, links for a mention, reference and SHA, plain unknown name' t_mentions_and_references
    step 'issues: create and comment (HTML forms and API)' t_issues
    step 'avatar: reserve, PUT bytes, confirm, download' t_avatar
    step 'avatar: profile settings form sends it, remove deletes it' t_avatar_profile_form
    step 'avatar: organization page form sends it, remove deletes it' t_avatar_org_form
    step 'pages: server HTML carries the hydration contract' t_pages
    step 'bundle: generated JS and WASM are served intact' t_bundle
    step 'ci: act ran the workflow, run succeeded, logs contain hello' t_ci_result
    step 'ci: an API file save queues the push workflow' t_ci_api_save
    step 'real clients: GHES gh commands, native github-script and schema-checked webhooks' t_github_clients
    gh_sweep_step
    step -f -i 'actions: restart with the upstream local test origin and real Results URL' actions_restart_server
    step 'actions: real cache save, restore, miss, repository isolation and cross-job artifacts' t_action_objects
    ACTION_OBJECTS_ORIGIN=''
    step -i 'restart: SIGTERM stops the server cleanly' stop_server
    step -f -i 'restart: server starts again on the same data' start_server
    step 'restart: sessions, repositories, CI history and SSH host key survive' t_after_restart
    if [[ $db == postgres ]]; then
        step -f -i 'runners: the server restarts with the node connection and no runner of its own' runners_restart_server
        step 'files: single-node runner transport keeps local uploads readable' t_local_files_with_runners
        step -f 'runners: single-node mode invites runners only; one reusable runner invite' t_runner_token
        step -f -i 'runners: r1 joins with the invite' start_server r1
        step -f -i 'runners: r2 joins with the same invite' start_server r2
        step -f 'runners: a repository with a slow workflow' t_runners_repo
        step 'runners: queued runs are shared by both runners and none runs twice' t_runners_share
        step 'runners: a revoked runner cannot claim and the other keeps working' t_runner_revoked
        step -i 'runners: kill -9 mid-job fails the run after the heartbeat timeout; the other runner keeps working' t_runner_killed
        step -i 'runners: the remaining runners stop cleanly' runners_stop
    fi
    step -f 'backup: a full backup, then a second one while pushes and new issues keep arriving' t_backup_under_load
    step -f -i 'restore: the server stops' stop_server
    step -f -i 'restore: an empty installation is restored from the backup' t_restore_into_empty
    step -f -i 'restore: the restored server starts' start_server
    step 'restore: verify finds nothing; clone, issues, pull request checkout, run log and attachment work' t_restored_works
    step -f 'kept deletion: a repository is deleted after a backup' t_delete_kept
    step -f -i 'kept deletion: the server stops' stop_server
    step -f 'kept deletion: the database goes back to before the delete; verify --repair puts the repository back' t_database_before_delete
    step -f -i 'kept deletion: the server starts' start_server
    step 'kept deletion: the repository clones again' t_deleted_repo_back
    if [[ $db == postgres ]]; then
        step -f 'git newer: a pull request moves and another opens after time T' t_git_newer_prepare
        step -f -i 'git newer: the server stops' stop_server
        step -f 'git newer: PostgreSQL goes back to T; verify --repair follows Git' t_git_newer_repair
        step -f -i 'git newer: the server starts' start_server
        step 'git newer: the pull request has its new head, the later one is gone, clones work' t_git_newer_works
    fi
    step -f -i 'rebuild: the server stops' stop_server
    step -f 'rebuild: pack indexes, commit graphs and language rows deleted, then rebuilt' t_drop_derived
    step -f -i 'rebuild: the server starts' start_server
    step 'rebuild: pages, languages, search and clones work' t_rebuilt_works
    step 'log: the server log has no panic' t_log_clean
    step -i 'shutdown: SIGTERM exits with status 0' stop_server
    terminate_servers
    ROUND_BLOCKED=0
    step -i 'disposable database removed' drop_pg
    if grep -q '^FAIL|'"$db"'|' < <(printf '%s\n' "${RESULTS[@]}"); then
        echo "---- last lines of the $db server log"
        tail -n 60 "$RUN/server.log" 2>/dev/null | sed 's/^/        | /'
    fi
}

run_cluster_round() {
    CLUSTER_NODES=(web1 store1 store2 ci1)
    ROUND=cluster
    ROUND_BLOCKED=0
    STEP_NO=0
    RUN=$WORK/cluster
    STATE=$RUN/state-files
    HTTP_PORT='' SSH_PORT='' BASE='' SSH_URL='' DATABASE_URL='' DATABASE_DIRECT_URL=''
    mkdir -p "$RUN/home" "$RUN/keys" "$RUN/work" "$STATE"
    PW_ALICE=Alice-e2e-$(random_hex)
    PW_BOB=Bob-e2e-$(random_hex)

    step -f -i 'database is ready' prepare_database
    step -f -i 'cluster: isolated ports and declared store2 key' prepare_cluster
    step -f 'cluster: explicit legacy mode preserves the asynchronous-replica checks' cluster_cli web1 enable --write-quorum 1
    step -f -i 'cluster: web1 starts first (web,ssh)' start_server web1
    step -f 'cluster: store1 pairs with an invite and cluster join' t_cluster_join_store
    step -f -i 'cluster: store1 starts (storage only)' start_server store1
    step -f 'ssh and test keys are generated' t_keys
    step -f 'setup: first-run admin through the HTML form' t_setup
    step -f 'users: register and sign in through HTML forms' t_users
    step -f 'repositories: created through web1 with store1 as the only live store' t_repos
    step -f -i 'cluster: declared store2 starts (storage only)' start_server store2
    step -f 'cluster: reusable runner token' t_cluster_worker_token
    step -f -i 'cluster: ci1 starts without database credentials' start_server ci1
    step 'membership: CLI, admin nodes page, readiness and roles' t_cluster_membership
    step -f 'placement: database and disks name store1; control repo on store2' t_cluster_placement
    step -f 'ssh keys: added through settings' t_ssh_keys
    step -f 'tokens: created through settings' t_tokens
    step -f 'git: SSH push through web1 to store1' t_push
    step -f 'git: SSH and HTTP clones, token HTTP push, access refusals' t_clone
    step -f 'git: HTTP push to store1 and fresh clones see it' t_cluster_http_push
    step 'git: remote multi-round fetches, 150 MiB SSH/HTTP push and clone, bounded node memory' t_git_streaming
    step 'editor: HTML save, stale save and new file on store1' t_editor
    step 'pull request: API merge on store1' t_pull_request
    step 'fork: placement next to its source and API merge' t_fork_pull_request
    step 'protection: SSH and HTTP pushes refused, writer review permits merge' t_protection
    step -f 'ci: workflow pushed through web1 with a disposable secret' t_cluster_ci_push
    step -f 'ci: ci1 runs it; logs, page and commit status reach web1' t_cluster_ci_result
    step 'ci: fork waits for approval, runs on ci1 without secrets' t_cluster_fork_ci
    step 'organization: created for real GitHub clients' t_org
    step -i 'web loss: real gh and Git continue through web2 while web1 dies' t_cluster_github_clients
    step -f 'replica: setting 2 keeps a current copy on store2' t_replica_create
    step -f 'failure: revoke store1 to hold automatic takeover during fallback checks' cluster_cli web1 revoke store1
    step -i 'failure: store1 exits cleanly on SIGTERM' stop_server store1
    step 'failure: store1 down: a push to alice/replica is refused and the copy stays put' t_replica_outage_push
    step 'failure: store1 down and store2 revoked: alice/replica reports 503' t_cluster_outage_no_copy
    step 'failure: store1 down: a copy serves alice/replica pages, API and clones' t_replica_outage_reads
    step -f 'replica: approve the stopped store1 to allow automatic takeover' cluster_cli web1 approve store1
    step -f 'replica: store2 takes the repository over while store1 stays down' t_replica_take_over
    step -f 'replica: a clone and a push reach the promoted store2' t_replica_push
    step 'failure: store2 repositories keep working and web1 stays ready' t_cluster_outage_survivor
    step -f -i 'recovery: store1 starts on its original data' start_server store1
    step 'recovery: pages, API, SSH and HTTP work, including a new push' t_after_restart
    step -f 'replica: store1 returns as a copy and catches up' t_replica_catch_up
    step -f 'replica: a person promotes store1 again by hand' t_replica_hand_back
    step -f 'revocation: live ci1 cannot claim two new runs; both stay queued' t_cluster_revoke_worker
    step 'revocation: approving ci1 runs the queued work' t_cluster_approve_worker
    step 'drain: a drained runner takes no new jobs and cancel resumes claims' t_cluster_drain_worker
    step 'cluster pages: drain and scaling save with and without hydration' t_cluster_browser
    step -f -i 'quorum: HTTP and SSH acknowledgements survive immediate primary SIGKILL' t_cluster_quorum_primary_loss
    step -f -i 'storage loss: interrupted clone and push fail clearly; retries preserve refs' t_cluster_storage_loss
    step -f -i 'quorum: editor save, gh api branch and gh pr merge survive primary SIGKILL' t_cluster_quorum_other_publishers
    step -f -i 'SIGTERM: active CI and push stop within 90 seconds without leaked children or locks' t_cluster_graceful_shutdown
    # Shutdown is still checked after a required step fails.
    ROUND_BLOCKED=0
    step -i 'shutdown: every node exits with status 0 and no log has a panic' t_cluster_shutdown
    terminate_servers
    step -i 'disposable database removed' drop_pg
    if grep -q '^FAIL|cluster|' < <(printf '%s\n' "${RESULTS[@]}"); then
        local node
        for node in "${CLUSTER_NODES[@]}"; do
            echo "---- last lines of $node log"
            tail -n 40 "$(server_dir "$node")/server.log" 2>/dev/null | sed 's/^/        | /'
        done
    fi
}

# -------------------------------------------------------------------- main --

# Durability has its own real-process round so its storage failures do not depend on
# finished CI jobs or alter the asynchronous placement fixtures in the cluster round.
run_quorum_round() {
    ROUND=quorum
    ROUND_BLOCKED=0
    STEP_NO=0
    RUN=$WORK/quorum
    STATE=$RUN/state-files
    HTTP_PORT='' SSH_PORT='' BASE='' SSH_URL='' DATABASE_URL='' DATABASE_DIRECT_URL=''
    CLUSTER_NODES=(web1 store1 store2)
    mkdir -p "$RUN/home" "$RUN/keys" "$RUN/work" "$STATE"
    PW_ALICE=Alice-e2e-$(random_hex)
    PW_BOB=Bob-e2e-$(random_hex)
    step -f -i 'database is ready' prepare_database
    step -f -i 'quorum: isolated ports and storage identities' prepare_cluster
    step -f 'quorum: configure the empty installation' cluster_cli web1 enable --write-quorum 1
    step -f -i 'quorum: web and SSH start' start_server web1
    step -f 'quorum: first storage node pairs' t_cluster_join_store
    step -f -i 'quorum: first storage node starts' start_server store1
    step -f -i 'quorum: second storage node starts' start_server store2
    step -f 'quorum: generate client keys' t_keys
    step -f 'quorum: create administrator' t_setup
    step -f 'quorum: register second user' t_users
    step -f 'quorum: register SSH keys' t_ssh_keys
    step -f 'quorum: issue read and write tokens' t_tokens
    step -f 'quorum: issue second client token' t_github_token
    step -f -i 'quorum: three voters keep HTTP and SSH acknowledged pushes after SIGKILL' t_cluster_quorum_primary_loss
    step -f -i 'quorum: editor, branch and merge publications survive primary SIGKILL' t_cluster_quorum_other_publishers
    step -f -i 'quorum: two stopped voters refuse a push; one stopped voter permits it' t_cluster_quorum_no_majority
    ROUND_BLOCKED=0
    step -i 'quorum: every owned node stops cleanly' t_cluster_shutdown store1 store2 web1
    terminate_servers
    step -i 'quorum: remove disposable database' drop_pg
}

preflight() {
    local missing=0 name
    for name in cargo python3 git curl sha256sum base64 timeout stat awk gh openssl node; do
        command -v "$name" >/dev/null || { echo "missing tool: $name"; missing=1; }
    done
    "$GITHUB_PYTHON" -I -c 'import jsonschema, referencing' || missing=1
    PYTHON=$(command -v python3)
    SSH=$(find_tool ssh) || { echo "missing tool: ssh"; missing=1; }
    SSH_KEYGEN=$(find_tool ssh-keygen) || { echo "missing tool: ssh-keygen"; missing=1; }
    [[ -x $ACT ]] || { echo "act not found at $ACT (set E2E_ACT_BINARY)"; missing=1; }
    [[ -x ${DX:-$(command -v dx || true)} || $E2E_SKIP_BUILD == 1 ]] || { echo "dx not found (set DX)"; missing=1; }
    echo "git $(git --version | cut -d' ' -f3), $("$SSH" -V 2>&1), act $("$ACT" --version 2>/dev/null | awk '{print $3}'), $("$PYTHON" -V), $(gh --version | head -n 1)"
    ((missing == 0))
}

# The server makes each run's cgroup below its own, which is this script's. Prints why local
# CI runs cannot execute here, or nothing.
ci_runs_unavailable() {
    local current probe
    current=${CI_PROCESS_CGROUP_ROOT:-/sys/fs/cgroup/$(sed -n 's|^0::/||p' /proc/self/cgroup)}
    if [[ $(stat -f -c %T "$current" 2>/dev/null) != cgroup2fs ]]; then
        echo 'needs a unified cgroup v2 for local CI runs'
    elif [[ ! -w $current/cgroup.procs ]] || ! probe=$(mktemp -d "$current/codeyard-e2e-XXXXXX" 2>/dev/null); then
        echo "needs a writable delegated cgroup v2 for local CI runs; $current is not"
    else
        rmdir "$probe"
    fi
}

run_cells_round() {
    ROUND=cells
    ROUND_BLOCKED=0
    STEP_NO=0
    RUN=$WORK/cells
    mkdir -p "$RUN"
    step -f -i 'cells: disposable directory and two native databases' prepare_cells_databases
    if [[ $ROUND_BLOCKED == 0 ]]; then
        step -f 'cells: real public router, foreign gh and Git, revocation and outage acceptance' \
            "$GITHUB_PYTHON" -I "$ROOT/tests/e2e/e2e-cells.py" --run "$RUN" --binary "$BIN"
    fi
    step -i 'cells: remove the disposable databases' drop_pg
}

prepare_cells_databases() {
    [[ -n ${GITSERV_TEST_DATABASE_URL:-} ]] || { echo 'Cells need GITSERV_TEST_DATABASE_URL' >&2; return 1; }
    local createdb candidate kind url
    createdb=$(pg_tool createdb) || return 1
    for kind in DIRECTORY ZERO ONE; do
        candidate=gitserv_cells_$(random_hex)
        pg_command "$GITSERV_TEST_DATABASE_URL" "$createdb" --maintenance-db "$candidate" || return 1
        PG_OWNED+=("$candidate")
        url=$(pg_url "$candidate") || return 1
        export "E2E_CELLS_${kind}_URL=$url"
    done
}

summary() {
    local line status round name took pass=0 fail=0 skip=0
    echo
    echo "== e2e summary (profile $E2E_PROFILE, $((SECONDS - STARTED)) s) =="
    for line in "${RESULTS[@]}"; do
        IFS='|' read -r status round name took <<<"$line"
        printf '%-5s [%s] %s (%ss)\n' "$status" "$round" "$name" "$took"
        case $status in PASS) pass=$((pass + 1)) ;; FAIL) fail=$((fail + 1)) ;; *) skip=$((skip + 1)) ;; esac
    done
    echo "$pass passed, $fail failed, $skip skipped"
    ((fail == 0 && skip == 0 && pass > 0))
}

preflight || { echo "e2e: tools are missing" >&2; exit 2; }
CI_RUNS_UNAVAILABLE=$(ci_runs_unavailable)
[[ -z $CI_RUNS_UNAVAILABLE ]] || echo "e2e: this host $CI_RUNS_UNAVAILABLE; checks that need a finished run are skipped"
echo "e2e: temporary directory $WORK"

if [[ $E2E_SKIP_BUILD == 1 ]]; then
    step 'build: committed tailwind.css matches what the views generate' check_css
    step -f 'build: existing server binary and bundle are present' check_build_artifacts
else
    step -f 'build: browser bundle (Tailwind, dx)' build_bundle
    step -f -i 'build: server binary (cargo)' build_server
fi

if [[ $E2E_BUILD_ONLY != 1 && $ROUND_BLOCKED == 0 ]]; then
    for database in $E2E_DATABASES; do
        if [[ $database == quorum ]]; then
            run_quorum_round
        elif [[ $database == cells ]]; then
            run_cells_round
        elif [[ $database == cluster ]]; then
            [[ ${E2E_GITHUB_ONLY:-0} == 1 || ${E2E_ACTIONS_ONLY:-0} == 1 ]] || run_cluster_round
        else
            run_round "$database"
        fi
    done
fi

if summary; then exit 0; else exit 1; fi
