#!/usr/bin/env bash
# One disposable PostgreSQL node; use an already built binary, never build here.
set -euo pipefail
ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
export BENCH_ROOT=$ROOT
exec timeout --signal=TERM --kill-after=120 "${BENCH_TIMEOUT:-1800}" python3 -I - <<'PY'
import base64
import concurrent.futures
import json
import math
import os
import pathlib
import secrets
import shutil
import signal
import socket
import subprocess
import tempfile
import threading
import time
import urllib.parse as u

root = pathlib.Path(os.environ['BENCH_ROOT'])
binary = pathlib.Path(os.environ.get('BENCH_BINARY', root / 'target/release/codeyard')).resolve()
actions = pathlib.Path(os.environ.get('CODEYARD_ACTIONS_BIN', binary.with_name('codeyard-actions'))).resolve()
upstream = os.environ.get('GITSERV_TEST_DATABASE_URL', '')
duration = int(os.environ.get('BENCH_SECONDS', '30'))
if not upstream.startswith(('postgres://', 'postgresql://')):
    raise SystemExit('Set GITSERV_TEST_DATABASE_URL to PostgreSQL with CREATEDB permission')
if not 10 <= duration <= 300:
    raise SystemExit('BENCH_SECONDS must be between 10 and 300')
if not binary.is_file() or not os.access(binary, os.X_OK):
    raise SystemExit('BENCH_BINARY must name an already built codeyard binary')
if not actions.is_file() or not os.access(actions, os.X_OK):
    raise SystemExit('CODEYARD_ACTIONS_BIN must name an already built native runner')
for tool in ('curl', 'git', 'createdb', 'dropdb', 'timeout'):
    if not shutil.which(tool):
        raise SystemExit(f'Missing tool: {tool}')
work = pathlib.Path(tempfile.mkdtemp(prefix='codeyard-bench-'))
work.chmod(0o700)
(work / '.env').touch()
database = 'codeyard_bench_' + secrets.token_hex(6)
server = None
read_process = None
owned = False
stop = threading.Event()
client_env = {key: value for key, value in os.environ.items() if not key.startswith('GIT_')}
client_env.update(GIT_CONFIG_NOSYSTEM='1', GIT_CONFIG_GLOBAL='/dev/null', GIT_TERMINAL_PROMPT='0',
                  GIT_AUTHOR_NAME='bench', GIT_AUTHOR_EMAIL='bench@example.test',
                  GIT_COMMITTER_NAME='bench', GIT_COMMITTER_EMAIL='bench@example.test')

def command(args, *, seconds=60, **kwargs):
    try:
        return subprocess.run(['timeout', '--kill-after=5', str(seconds), *map(str, args)],
                              check=True, **kwargs)
    except subprocess.CalledProcessError as error:
        raise RuntimeError(f'{args[0]} failed with exit status {error.returncode}') from None

def pg(tool, *args):
    parts = u.urlsplit(upstream)
    query = []
    env = os.environ.copy()
    if parts.password is not None:
        env['PGPASSWORD'] = u.unquote(parts.password)
    for key, value in u.parse_qsl(parts.query):
        if key == 'password':
            env['PGPASSWORD'] = value
        elif key != 'statement-cache-capacity':
            query.append(('sslmode' if key == 'ssl-mode' else key, value))
    netloc = parts.netloc
    if parts.password is not None:
        user, host = netloc.rsplit('@', 1)
        netloc = user.split(':', 1)[0] + '@' + host
    url = parts._replace(netloc=netloc, query=u.urlencode(query)).geturl()
    command([tool, '--maintenance-db=' + url, *args], env=env, stdout=subprocess.DEVNULL)

def ports():
    sockets = [socket.socket() for _ in range(2)]
    try:
        for sock in sockets:
            sock.bind(('127.0.0.1', 0))
        return [sock.getsockname()[1] for sock in sockets]
    finally:
        for sock in sockets:
            sock.close()

def terminate(process):
    if process is None:
        return
    if process.poll() is None:
        process.terminate()
        try:
            process.wait(timeout=95)
        except subprocess.TimeoutExpired:
            process.kill()
    process.wait()

def interrupted(signum, frame):
    stop.set()
    raise SystemExit(128 + signum)

signal.signal(signal.SIGTERM, interrupted)
signal.signal(signal.SIGINT, interrupted)
try:
    pg('createdb', database)
    owned = True
    parts = u.urlsplit(upstream)
    query = [(k, v) for k, v in u.parse_qsl(parts.query) if k != 'dbname']
    url = parts._replace(path='/' + database, query=u.urlencode(query)).geturl()
    http_port, ssh_port = ports()
    base = f'http://127.0.0.1:{http_port}'
    env = {key: os.environ[key] for key in ('PATH', 'LANG', 'TZ') if key in os.environ}
    env.update(DATABASE_URL=url, GITSERV_STATE_DIR=str(work / 'state'), REPOS_PATH=str(work / 'repos'),
               HOME=str(work), HOST_KEY_PATH=str(work / 'host_key'), GITSERV_NODE_ROLES='server,runner',
               HTTP_ADDR=f'127.0.0.1:{http_port}', SSH_ADDR=f'127.0.0.1:{ssh_port}', SSH_MODE='builtin',
               PUBLIC_URL=base, PUBLIC_HTTP_CLONE_BASE=base, COOKIE_SECURE='false',
               SECRETS_MASTER_KEY=secrets.token_hex(32), RUST_LOG='warn',
               CI_EXECUTION_MODE='local', CI_ACTIONS_BINARY=str(actions),
               CI_MAX_ACTIVE_RUNS='4', CI_RUN_TIMEOUT_SECONDS='120',
               WEBUI_DIST=str(pathlib.Path(os.environ.get('WEBUI_DIST', root / 'dist/webui')).resolve()))
    if os.environ.get('CI_PROCESS_CGROUP_ROOT'):
        env['CI_PROCESS_CGROUP_ROOT'] = os.environ['CI_PROCESS_CGROUP_ROOT']
    with (work / 'server.log').open('wb') as log:
        server = subprocess.Popen([str(binary), 'serve'], cwd=work, env=env, stdout=log, stderr=log)
    for attempt in range(120):
        if server.poll() is not None:
            raise RuntimeError('Server exited during startup; inspect server.log')
        ready = subprocess.run(['curl', '-q', '--noproxy', '*', '-fsS', '--max-time', '1',
                                base + '/ready'], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        if ready.returncode == 0:
            break
        time.sleep(0.5)
    else:
        raise RuntimeError('Server did not become ready')

    def form(path, action, fields, status='303', output=()):
        args = ['python3', '-I', root / 'scripts/e2e-web.py', 'form', '--base', base,
                '--jar', work / 'jar.json', '--who', 'bench', path, '--action', action, '--status', status]
        if fields.get('action') == 'create':
            args += ['--has', 'action=create']
        for key, value in fields.items():
            args += ['--set', f'{key}={value}']
        return command([*args, *output], stdout=subprocess.PIPE, stderr=subprocess.DEVNULL).stdout.decode().strip()

    password = secrets.token_hex(24)
    form('/setup', '/setup', dict(username='bench', email='bench@example.test', password=password,
                                confirm_password=password, node_mode='public'))
    token = form('/settings?tab=tokens', '/settings/tokens',
                 dict(name='benchmark', scope='write', expiry='never'), status='200',
                 output=['--print', r're:(?s)Copy this token now.*?(cy_pat_[0-9a-f]{64})'])
    curl_config = work / 'auth.curl'
    curl_config.write_text(f'header = "Authorization: Bearer {token}"\n')
    curl_config.chmod(0o600)

    def rest(path, data=None):
        payload = ['--data-binary', json.dumps(data)] if data is not None else []
        result = command(['curl', '-q', '--noproxy', '*', '--config', curl_config, '-fsS',
                          '--max-time', '30', '--max-filesize', '1048576',
                          '-H', 'Content-Type: application/json', *payload,
                          base + '/api/v3' + path], stdout=subprocess.PIPE)
        return json.loads(result.stdout)

    print('Preparing 50 organizations and REST issue fixtures; readers are 500 concurrent clients.', flush=True)
    for index in range(50):
        org = f'bench-org-{index}'
        # Organization creation has no REST endpoint; use its native CSRF form.
        form('/settings/organizations', '/settings/organizations', dict(action='create', slug=org,
                                                                      display_name=org))
        rest(f'/orgs/{org}/repos', dict(name='bench', private=False))
        rest(f'/repos/{org}/bench/issues', dict(title='Benchmark issue', body='Read workload'))

    auth = base64.b64encode(f'bench:{token}'.encode()).decode()
    git_env = dict(client_env, GIT_CONFIG_COUNT='1', GIT_CONFIG_KEY_0='http.extraHeader',
                   GIT_CONFIG_VALUE_0='Authorization: Basic ' + auth)
    source = work / 'source'
    source.mkdir()
    def git(*args):
        command(['git', *args], seconds=120, env=git_env, stdout=subprocess.DEVNULL,
                stderr=subprocess.DEVNULL)
    git('-C', source, 'init', '-q', '-b', 'main')
    (source / 'payload.bin').write_bytes(os.urandom(1024 * 1024))
    workflow = source / '.github/workflows/ci.yml'
    workflow.parent.mkdir(parents=True)
    workflow.write_text('name: Cell benchmark\non: [push]\njobs:\n  measure:\n    runs-on: ubuntu-latest\n    steps:\n      - run: printf \"benchmark job\\n\"\n')
    git('-C', source, 'add', '.')
    git('-C', source, 'commit', '-qm', 'Initial payload')
    remote = base + '/bench-org-0/bench.git'
    git('-C', source, 'push', '-q', remote, 'main')

    # Curl owns at most 500 simultaneous transfers; each response is discarded and bounded.
    reads = work / 'read.curl'
    with reads.open('w') as config:
        for index in range(500):
            org = f'bench-org-{index % 50}'
            for kind, path in [('api', f'/api/v3/repos/{org}/bench/issues?per_page=10'),
                               ('page', f'/{org}/bench/issues')]:
                config.write(f'url = "{base}{path}"\noutput = "/dev/null"\n')
    samples = {'push': [], 'clone': [], 'api': [], 'page': [], 'invitation': [], 'ci': []}
    clone_bytes = 0
    started = time.monotonic()
    deadline = started + duration

    def readers():
        global read_process
        for batch in range(100):
            if stop.is_set():
                return
            with (work / 'read-times').open('wb') as output, (work / 'curl.log').open('ab') as errors:
                read_process = subprocess.Popen(['curl', '-q', '--noproxy', '*', '-fsS', '--parallel',
                    '--parallel-immediate', '--parallel-max', '500', '--max-time', '30',
                    '--max-filesize', '2097152', '--write-out', '%{url_effective} %{http_code} %{time_total}\\n',
                    '--config', str(reads)], stdout=output, stderr=errors)
                try:
                    code = read_process.wait(timeout=45)
                except subprocess.TimeoutExpired:
                    terminate(read_process)
                    raise RuntimeError('Reader batch timed out')
            if code != 0:
                raise RuntimeError('A reader failed; inspect curl.log')
            lines = (work / 'read-times').read_text().splitlines()
            if len(lines) != 1000:
                raise RuntimeError('Incomplete reader batch')
            for line in lines:
                read_url, status, seconds = line.split()
                kind = 'api' if u.urlsplit(read_url).path.startswith('/api/v3/') else 'page'
                if status != '200':
                    raise RuntimeError(f'{kind} read returned HTTP {status}')
                samples[kind].append(float(seconds))
        if not stop.is_set():
            raise RuntimeError('Reader sample limit reached before workload finished')

    def invitations():
        barrier = threading.Barrier(50, timeout=30)
        def invite(index):
            barrier.wait()
            before = time.monotonic()
            reply = rest(f'/orgs/bench-org-{index}/invitations',
                         dict(email=f'invite-{index}@example.test', role='direct_member'))
            if not isinstance(reply.get('id'), int):
                raise RuntimeError('Invitation was not persisted')
            return time.monotonic() - before
        with concurrent.futures.ThreadPoolExecutor(max_workers=50) as inviting:
            return list(inviting.map(invite, range(50)))

    print(f'Measuring for at least {duration}s with 500 concurrent reader slots, CI and 50 simultaneous invitations.', flush=True)
    with concurrent.futures.ThreadPoolExecutor(max_workers=2) as executor:
        future = executor.submit(readers)
        invites = executor.submit(invitations)
        try:
            for index in range(1000):
                if future.done():
                    future.result()
                    raise RuntimeError('Readers stopped before Git workload')
                (source / 'counter').write_text(str(index))
                git('-C', source, 'add', 'counter')
                git('-C', source, 'commit', '-qm', f'Push {index}')
                before = time.monotonic()
                git('-C', source, 'push', '-q', remote, 'main')
                samples['push'].append(time.monotonic() - before)
                clone = work / 'clone.git'
                before = time.monotonic()
                git('clone', '-q', '--bare', remote, clone)
                samples['clone'].append(time.monotonic() - before)
                clone_bytes += sum(path.stat().st_size for path in (clone / 'objects').rglob('*') if path.is_file())
                shutil.rmtree(clone)
                if time.monotonic() >= deadline:
                    break
            else:
                raise RuntimeError('Git sample limit reached before measurement duration')
        finally:
            stop.set()
        future.result()
        samples['invitation'].extend(invites.result())
    # An enqueued run is not a completed CI sample. Require every measured push to finish.
    ci_deadline = time.monotonic() + 300
    for attempt in range(300):
        runs = []
        for page in range(1, 12):
            batch = rest(f'/repos/bench-org-0/bench/actions/runs?per_page=100&page={page}')['workflow_runs']
            runs.extend(batch)
            if len(batch) < 100:
                break
        expected = len(samples['push']) + 1
        if len(runs) < expected:
            if time.monotonic() >= ci_deadline:
                raise RuntimeError('CI admission did not create every expected run')
        elif all(run['status'] == 'completed' for run in runs):
            from datetime import datetime
            if any(run['conclusion'] != 'success' for run in runs):
                raise RuntimeError('A benchmark CI job failed')
            for run in runs:
                begin = datetime.fromisoformat(run['created_at'].replace('Z', '+00:00'))
                end = datetime.fromisoformat(run['updated_at'].replace('Z', '+00:00'))
                samples['ci'].append((end - begin).total_seconds())
            break
        if time.monotonic() >= ci_deadline:
            raise RuntimeError('Benchmark CI jobs did not finish')
        threading.Event().wait(1)
    else:
        raise RuntimeError('CI sample limit reached')
    elapsed = time.monotonic() - started
    print(f'Client host: {os.uname().nodename}; CPUs: {os.cpu_count()}; duration: {elapsed:.2f}s')
    print('PostgreSQL hardware is external; record its CPU, RAM, disk and network with these results.')
    print(f'{"operation":<12} {"samples":>8} {"p50 ms":>10} {"p95 ms":>10} {"p99 ms":>10} {"ops/s":>10}')
    for kind, values in samples.items():
        if not values:
            raise RuntimeError(f'No {kind} samples')
        ordered = sorted(values)
        percentiles = [ordered[math.ceil(len(ordered) * p) - 1] * 1000 for p in (0.50, 0.95, 0.99)]
        print(f'{kind:<12} {len(values):8d} ' + ' '.join(f'{value:10.2f}' for value in percentiles)
              + f' {len(values) / elapsed:10.2f}')
    print(f'Clone object throughput: {clone_bytes / (1024 * 1024) / elapsed:.2f} MiB/s (not wire bytes)')
finally:
    stop.set()
    terminate(read_process)
    terminate(server)
    try:
        if owned:
            pg('dropdb', '--if-exists', '--force', database)
    finally:
        if os.environ.get('BENCH_KEEP') == '1':
            print(f'Kept private fixture and logs: {work}')
        else:
            shutil.rmtree(work)
PY
