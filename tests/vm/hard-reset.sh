#!/usr/bin/env bash
# Push from outside a disposable guest, reset its kernel without syncing, then clone.
set -euo pipefail
ROOT=$(cd "$(dirname "$0")/../.." && pwd)
[[ $# == 2 && -x $1 && -d $2 ]] || {
    echo "Usage: $0 /absolute/path/to/codeyard /absolute/path/to/webui-dist" >&2
    exit 2
}
export CODEYARD_VM_BINARY=$(realpath "$1") CODEYARD_VM_WEBUI=$(realpath "$2") CODEYARD_VM_SOURCE=$ROOT
RUN=$(mktemp -d)
vm_pid=''
stop_vm() {
    if [[ -n $vm_pid ]]; then
        kill -TERM -- "-$vm_pid" 2>/dev/null || true
        for ((attempt=0; attempt<50; attempt++)); do
            kill -0 -- "-$vm_pid" 2>/dev/null || break
            sleep 0.1
        done
        kill -KILL -- "-$vm_pid" 2>/dev/null || true
        wait "$vm_pid" 2>/dev/null || true
        vm_pid=''
    fi
}
cleanup() {
    stop_vm
    rm -rf "$RUN"
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM
ssh-keygen -q -t ed25519 -N '' -f "$RUN/key"
export CODEYARD_VM_PUBLIC_KEY=$(cat "$RUN/key.pub")
for database in sqlite postgres; do
    export CODEYARD_VM_DATABASE=$database
    read -r CODEYARD_VM_SSH_PORT CODEYARD_VM_HTTP_PORT < <(python3 - <<'PY'
import socket
with socket.socket() as first, socket.socket() as second:
    first.bind(('127.0.0.1', 0))
    second.bind(('127.0.0.1', 0))
    print(first.getsockname()[1], second.getsockname()[1])
PY
)
    export CODEYARD_VM_SSH_PORT CODEYARD_VM_HTTP_PORT
    vm=$(nix build --impure --no-link --print-out-paths --expr '
      let
        root = builtins.getEnv "CODEYARD_VM_SOURCE";
        flake = builtins.getFlake ("path:" + root);
        pkgs = flake.inputs.nixpkgs.legacyPackages.x86_64-linux;
      in import (root + "/tests/vm/guest.nix") {
        inherit pkgs;
        codeyardBinary = builtins.toPath (builtins.getEnv "CODEYARD_VM_BINARY");
        webuiDist = builtins.toPath (builtins.getEnv "CODEYARD_VM_WEBUI");
        sshPublicKey = builtins.getEnv "CODEYARD_VM_PUBLIC_KEY";
        database = builtins.getEnv "CODEYARD_VM_DATABASE";
        sshPort = builtins.fromJSON (builtins.getEnv "CODEYARD_VM_SSH_PORT");
        httpPort = builtins.fromJSON (builtins.getEnv "CODEYARD_VM_HTTP_PORT");
      }')
    mkdir "$RUN/$database"
    (
        cd "$RUN/$database"
        exec setsid "$vm/bin/run-codeyard-durability-vm"
    ) >"$RUN/$database/console.log" 2>&1 &
    vm_pid=$!
    if ! python3 - "$ROOT" "$RUN/$database" "$RUN/key" "$database" <<'PY'
import base64
import json
import os
from pathlib import Path
import secrets
import shlex
import subprocess
import sys
import time
import urllib.request

root, work, key = map(Path, sys.argv[1:4])
database = sys.argv[4]
base = 'http://127.0.0.1:' + os.environ['CODEYARD_VM_HTTP_PORT']
ssh = ['ssh', '-F', '/dev/null', '-i', str(key), '-p', os.environ['CODEYARD_VM_SSH_PORT'],
       '-o', 'BatchMode=yes', '-o', 'IdentitiesOnly=yes', '-o', 'StrictHostKeyChecking=accept-new',
       '-o', f'UserKnownHostsFile={work / "known_hosts"}', '-o', 'GlobalKnownHostsFile=/dev/null',
       '-o', 'ConnectTimeout=5', '-o', 'ConnectionAttempts=1',
       '-o', 'ServerAliveInterval=5', '-o', 'ServerAliveCountMax=3',
       '-o', f'ControlPath={work / "ssh.sock"}', '-o', 'ControlMaster=auto',
       '-o', 'ControlPersist=600', 'root@127.0.0.1']

def command(args, **kwargs):
    result = subprocess.run(list(map(str, args)), timeout=180,
                            stdout=subprocess.PIPE, stderr=subprocess.PIPE, **kwargs)
    if result.returncode:
        raise RuntimeError(f'{args[0]} exited {result.returncode}: {result.stderr.decode()[-4000:]}')
    return result.stdout.decode().strip()

def wait_ready(previous=None):
    deadline = time.monotonic() + 600
    while time.monotonic() < deadline:
        try:
            boot = command([*ssh, 'cat /proc/sys/kernel/random/boot_id'])
            if boot != previous:
                with urllib.request.urlopen(base + '/ready', timeout=5) as response:
                    if response.status == 200:
                        return boot
        except (OSError, RuntimeError, subprocess.SubprocessError):
            pass
        time.sleep(1)
    raise RuntimeError('Guest did not boot and become ready within 10 minutes')

def form(path, action, fields, status='303', output=()):
    args = ['python3', '-I', root / 'tests/e2e/e2e-web.py', 'form', '--base', base,
            '--jar', work / 'cookies.json', '--who', 'vm', path, '--action', action,
            '--status', status]
    for name, value in fields.items():
        args += ['--set', f'{name}={value}']
    return command([*args, *output])

boot = wait_ready()
filesystem = command([*ssh, 'findmnt -n -o FSTYPE,OPTIONS -T /var/lib/codeyard'])
print(f'{database}: guest filesystem {filesystem}; virtual disk cache=writeback', flush=True)
password = secrets.token_hex(24)
form('/setup', '/setup', dict(username='vmtest', email='vm@example.test', password=password,
                            confirm_password=password, node_mode='public'))
token = form('/settings?tab=tokens', '/settings/tokens',
             dict(name='hard reset', scope='write', expiry='never'), status='200',
             output=['--print', r're:(?s)Copy this token now.*?(cy_pat_(?:[0-9]{1,5}_)?[0-9a-f]{64})'])

def rest(path, data=None, method=None):
    request = urllib.request.Request(base + '/api/v3' + path,
        data=json.dumps(data).encode() if data is not None else None, method=method,
        headers={'Authorization': 'Bearer ' + token, 'Content-Type': 'application/json'})
    with urllib.request.urlopen(request, timeout=60) as response:
        return json.load(response)

repo = rest('/user/repos', dict(name='durable', private=False))
repo_api = '/repos/vmtest/durable'
issue = rest(repo_api + '/issues', dict(title='Persisted database metadata', body=database))
source = work / 'source'
source.mkdir()
auth = base64.b64encode(f'vmtest:{token}'.encode()).decode()
env = dict(os.environ, GIT_CONFIG_NOSYSTEM='1', GIT_CONFIG_GLOBAL='/dev/null',
           GIT_TERMINAL_PROMPT='0', GIT_CONFIG_COUNT='1',
           GIT_CONFIG_KEY_0='http.extraHeader', GIT_CONFIG_VALUE_0='Authorization: Basic ' + auth)

def git(*args):
    return command(['git', *args], env=env)

git('-C', source, 'init', '-q', '-b', 'main')
git('-C', source, 'config', 'user.name', 'VM test')
git('-C', source, 'config', 'user.email', 'vm@example.test')
remote = base + '/vmtest/durable.git'
def hard_reset(previous):
    reset = subprocess.run([*ssh, 'echo b > /proc/sysrq-trigger'], timeout=30,
                           stdout=subprocess.PIPE, stderr=subprocess.PIPE)
    assert reset.returncode in (0, 255), reset.stderr.decode()
    return wait_ready(previous=previous)

def verify_recovery(label, expected, payload, default_branch):
    clone = work / f'clone-{label}'
    try:
        git('clone', '-q', remote, clone)
    except RuntimeError:
        print(command([*ssh, 'find /var/lib/codeyard/repos -maxdepth 3 -type f | head -80; '
                       'journalctl -u codeyard -n 30 --no-pager']), flush=True)
        print(command([*ssh, 'id codeyard; find /var/lib/codeyard/repos -maxdepth 2 '
                       '-printf "%U:%G %m %s %p\\n"']), flush=True)
        for head in command([*ssh, 'find /var/lib/codeyard/repos -type f -name HEAD']).splitlines():
            bare = shlex.quote(str(Path(head).parent))
            print(command([*ssh, f'head -c 256 {shlex.quote(head)}; '
                           f'runuser -u codeyard -- git --git-dir={bare} fsck --full; '
                           f'runuser -u codeyard -- git --git-dir={bare} show-ref; true']), flush=True)
        raise
    assert git('-C', clone, 'rev-parse', 'HEAD') == expected
    assert git('-C', clone, 'symbolic-ref', '--short', 'HEAD') == default_branch
    assert git('-C', clone, 'rev-parse', 'refs/remotes/origin/topic/nested') == expected
    assert (clone / 'payload.bin').read_bytes() == payload
    git('-C', clone, 'fsck', '--full')
    heads = command([*ssh, 'find /var/lib/codeyard/repos -type f -name HEAD']).splitlines()
    assert len(heads) == 1, f'Expected one bare repository, found {heads}'
    bare = str(Path(heads[0]).parent)
    command([*ssh, f'runuser -u codeyard -- git --git-dir={shlex.quote(bare)} fsck --full'])
    metadata = rest(repo_api)
    assert metadata['id'] == repo['id']
    assert metadata['default_branch'] == default_branch
    assert rest(repo_api + '/branches/main')['commit']['sha'] == expected
    assert rest(repo_api + '/issues/1')['id'] == issue['id']
    assert rest(repo_api + '/issues/1')['body'] == database
    print(f'PASS {database}: {label}, commit {expected}, survived guest hard reset; '
          'clone and bare fsck, objects, nested refs, HEAD and database metadata match', flush=True)

for round_number in (1, 2):
    payload = secrets.token_bytes(1024 * 1024)
    (source / 'payload.bin').write_bytes(payload)
    git('-C', source, 'add', '.')
    git('-C', source, 'commit', '-qm', f'Before hard reset {round_number}')
    expected = git('-C', source, 'rev-parse', 'HEAD')
    # Keep SSH connected before pushing so a new handshake cannot delay the reset.
    command([*ssh, 'true'])
    git('-C', source, 'push', '-q', remote, 'main', 'HEAD:refs/heads/topic/nested')
    boot = hard_reset(boot)
    verify_recovery(f'push-{round_number}', expected, payload, 'main')

# A default-branch edit replaces symbolic HEAD independently of receive-pack.
command([*ssh, 'true'])
rest(repo_api, dict(default_branch='topic/nested'), method='PATCH')
boot = hard_reset(boot)
verify_recovery('default-branch-change', expected, payload, 'topic/nested')
# Close the owned SSH master before the guest is terminated.
subprocess.run([*ssh[:-1], '-O', 'exit', ssh[-1]], timeout=10,
               stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
PY
    then
        tail -n 150 "$RUN/$database/console.log" >&2
        exit 1
    fi
    stop_vm
done
