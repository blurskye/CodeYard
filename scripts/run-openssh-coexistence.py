#!/usr/bin/env python3
"""One-shot privileged rehearsal ONLY in an explicitly provisioned disposable VM.

No provisioning, account changes, service starts, SSH sessions, or host deployment.
See scripts/install-openssh.py for the installer whose coexistence model this checks.
Without --execute this performs static read-only checks, not an acceptance gate.
"""
import argparse
import ctypes
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import pwd
import re
import resource
import signal
import stat
import subprocess
import sys
import time


MARKER = Path('/etc/codeyard-coexistence-guest.json')
PURPOSE = 'codeyard-disposable-openssh-coexistence'
ACCOUNT_NAMES = {'admin': 'coexadmin', 'service': 'coexservice', 'git': 'coexgit'}
SSH_UNIT = Path('/etc/systemd/system/ssh.service')
BROKER_UNIT = Path('/etc/systemd/system/codeyard-coexistence-broker.service')
SYSTEMCTL = Path('/usr/bin/systemctl')
SSHD = Path('/usr/sbin/sshd')
SSH = Path('/usr/bin/ssh')
SSH_KEYGEN = Path('/usr/bin/ssh-keygen')
GIT = Path('/usr/bin/git')
VIRT = Path('/usr/bin/systemd-detect-virt')
HEX = re.compile(r'[0-9a-f]{64}\Z')
UUID = re.compile(r'[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}\Z')
ENV = {'PATH': '/usr/sbin:/usr/bin:/bin', 'LC_ALL': 'C', 'HOME': '/',
       'GIT_CONFIG_NOSYSTEM': '1', 'GIT_CONFIG_GLOBAL': '/dev/null',
       'GIT_CONFIG_SYSTEM': '/dev/null', 'GIT_TERMINAL_PROMPT': '0'}
INPUT_FILES = {'sshd_config', 'reload-guard.py', 'broker-bootstrap.json',
               'keys/host', 'keys/host.pub', 'keys/admin', 'keys/admin.pub',
               'keys/coexadmin.authorized_keys', 'keys/git', 'keys/git.pub',
               'keys/unknown', 'keys/unknown.pub', 'keys/outsider', 'keys/outsider.pub'}


class Refusal(RuntimeError):
    """Only fixed operation labels, never tool output or credentials, are public."""


def require(condition, label):
    if not condition:
        raise Refusal(label)


def digest(path):
    with path.open('rb') as stream:
        result = hashlib.sha256()
        for block in iter(lambda: stream.read(65536), b''):
            result.update(block)
        return result.hexdigest()


def root_path(path, directory=False, executable=False, private=False):
    path = Path(path)
    require(path.is_absolute() and str(path) == os.path.normpath(str(path))
            and re.fullmatch(r'/[A-Za-z0-9_./-]+', str(path)), 'noncanonical path refused')
    for component in (*reversed(path.parents), path):
        info = component.lstat()
        require(not stat.S_ISLNK(info.st_mode) and info.st_uid == 0
                and not info.st_mode & 0o022, 'root ownership/symlink check failed')
        if component != path or directory:
            require(stat.S_ISDIR(info.st_mode), 'directory ownership check failed')
            if executable:
                require(info.st_mode & 0o001, 'fixture executable ancestor must be world-traversable')
        else:
            require(stat.S_ISREG(info.st_mode) and info.st_nlink == 1, 'regular single-link input required')
    info = path.lstat()
    if executable:
        require(info.st_mode & 0o001 and not info.st_mode & 0o6000, 'non-setid executable required')
    if private:
        require(stat.S_IMODE(info.st_mode) == 0o600, 'private root fixture input required')
    return info


def tool_path(path, setuid=False):
    # Distribution tool symlinks only; fixture-selected inputs never follow links.
    current = path
    for _ in range(16):
        for component in (*reversed(current.parents), current):
            info = component.lstat()
            require(info.st_uid == 0 and (stat.S_ISLNK(info.st_mode) or not info.st_mode & 0o022),
                    'unsafe system tool link/parent')
        if not current.is_symlink():
            info = root_path(current.resolve(strict=True))
            require(info.st_mode & 0o001 and not info.st_mode & 0o2000
                    and bool(info.st_mode & stat.S_ISUID) == setuid, 'unsafe system tool mode')
            return
        target = Path(os.readlink(current))
        current = Path(os.path.normpath(target if target.is_absolute() else current.parent / target))
    raise Refusal('system tool symlink bound exceeded')


def load_json(path):
    require(path.stat().st_size <= 32768, 'contract input size bound exceeded')
    def unique(pairs):
        value = {}
        for key, item in pairs:
            require(key not in value, 'duplicate contract key')
            value[key] = item
        return value
    return json.loads(path.read_text(), object_pairs_hook=unique)


def server_config(root, port):
    return (f'Port {port}\nAddressFamily inet\nListenAddress 127.0.0.1\n'
            f'HostKey {root}/keys/host\nPidFile {root}/sshd.pid\n'
            'UsePAM no\nPasswordAuthentication no\nKbdInteractiveAuthentication no\n'
            'PubkeyAuthentication yes\nAuthenticationMethods publickey\nPermitRootLogin no\n'
            f'AuthorizedKeysFile {root}/keys/%u.authorized_keys\nAuthorizedKeysCommand none\n'
            'StrictModes yes\nPermitUserEnvironment no\nPermitUserRC no\nUseDNS no\nLogLevel ERROR\n').encode()


def reload_guard(root):
    # Operator installs this exact text before starting the dedicated guest unit.
    # The one-shot sentinel is consumed only with exact root ownership/content.
    return f'''#!/usr/bin/python3 -I
import os
from pathlib import Path
import signal
import stat
import sys
if len(sys.argv) != 2 or not sys.argv[1].isdigit() or int(sys.argv[1]) <= 1:
    sys.exit(76)
pid = int(sys.argv[1])
handle = os.pidfd_open(pid, 0)
process = Path('/proc') / str(pid)
# OpenSSH rewrites its process title; /proc/cmdline is NOT its original argv.
if (os.readlink(process / 'exe') != '/usr/sbin/sshd' or process.stat().st_uid != 0
        or (process / 'cgroup').read_text().splitlines() != ['0::/system.slice/ssh.service']):
    sys.exit(76)
flag = Path({str(root / 'reload.fail')!r})
try:
    info = flag.lstat()
except FileNotFoundError:
    pass
else:
    if (not stat.S_ISREG(info.st_mode) or info.st_uid != 0 or info.st_nlink != 1
            or stat.S_IMODE(info.st_mode) != 0o600
            or info.st_size != {len(('coexistence reload failure ' + root.name + chr(10)).encode())}
            or flag.read_bytes() != {('coexistence reload failure ' + root.name + chr(10)).encode()!r}):
        sys.exit(76)
    def version(value):
        return (value.st_dev, value.st_ino, value.st_mode, value.st_uid, value.st_gid,
                value.st_nlink, value.st_size, value.st_mtime_ns, value.st_ctime_ns)
    if version(flag.lstat()) != version(info):
        sys.exit(76)
    flag.unlink()
    sys.exit(75)
signal.pidfd_send_signal(handle, signal.SIGHUP, None, 0)
os.close(handle)
'''.encode()


def ssh_unit(root):
    return (f'[Unit]\nDescription=Disposable codeyard coexistence {root.name}\n'
            '[Service]\nType=simple\nUser=root\nRestart=no\nKillMode=control-group\n'
            f'ExecStart=/usr/sbin/sshd -D -e -f {root}/sshd_config\n'
            f'ExecReload={root}/reload-guard.py $MAINPID\n').encode()


def static_contract(args):
    require(sys.platform == 'linux' and os.getuid() == 0 and os.geteuid() == 0
            and sys.flags.isolated, 'Linux guest root and Python -I required; no elevation is performed')
    require(not any(os.environ.get(key) for key in
                    ('SSH_CONNECTION', 'SSH_CLIENT', 'SSH_TTY', 'SSH_AUTH_SOCK', 'SUDO_USER',
                     'SUDO_UID', 'SUDO_GID', 'container', 'KUBERNETES_SERVICE_HOST', 'LD_PRELOAD',
                     'LD_LIBRARY_PATH', 'PYTHONPATH', 'PYTHONHOME')),
            'borrowed remote/root session or injected environment refused; use guest console')
    require(not any(Path(path).exists() for path in
                    ('/.dockerenv', '/run/.containerenv', '/run/systemd/container', '/etc/NIXOS')),
            'container/declarative host refused; provision a separate VM')
    require(Path('/proc/1/comm').read_text().strip() == 'systemd'
            and Path('/run/systemd/system').is_dir(), 'real guest systemd PID1 required')
    require(hasattr(os, 'pidfd_open') and hasattr(signal, 'pidfd_send_signal'), 'guest pidfd support required')
    require(re.fullmatch(r'[0-9a-f]{32}', args.expected_machine_id)
            and re.fullmatch(r'[0-9a-f]{32}', args.forbidden_machine_id)
            and UUID.fullmatch(args.expected_guest_uuid) and UUID.fullmatch(args.forbidden_guest_uuid),
            'independent guest/forbidden host identity inputs required')
    machine = Path('/etc/machine-id').read_text().strip()
    guest = Path('/sys/class/dmi/id/product_uuid').read_text().strip().lower()
    require(machine == args.expected_machine_id and machine != args.forbidden_machine_id
            and guest == args.expected_guest_uuid and guest != args.forbidden_guest_uuid,
            'guest identity mismatch or forbidden host identity')
    for value in (args.expected_marker_sha256, args.expected_source_sha256,
                  args.expected_binary_sha256, args.expected_installer_sha256):
        require(HEX.fullmatch(value), 'explicit independent hash input required')
    root_path(MARKER, private=True)
    require(digest(MARKER) == args.expected_marker_sha256, 'operator marker hash mismatch')
    marker = load_json(MARKER)
    require(isinstance(marker, dict) and set(marker) == {'schema', 'purpose', 'disposable', 'fixture_only', 'guest_id',
                           'machine_id', 'guest_uuid', 'source_sha256', 'binary_sha256',
                           'installer_sha256', 'runner_sha256', 'port', 'accounts', 'owned_paths',
                           'installer_temporary_prefixes', 'files', 'ssh_unit_sha256',
                           'broker_unit_sha256', 'key_id', 'outsider_key_id', 'allowed_head',
                           'denied_head'}, 'marker schema mismatch')
    require(type(marker['schema']) is int and marker['schema'] == 1 and marker['purpose'] == PURPOSE
            and marker['disposable'] is True and marker['fixture_only'] is True
            and marker['machine_id'] == machine and marker['guest_uuid'] == guest,
            'explicit disposable fixture provisioning marker required')
    require(isinstance(marker['guest_id'], str)
            and re.fullmatch(r'[0-9a-f]{16}', marker['guest_id']), 'invalid guest resource identity')
    root = Path('/opt/codeyard-coexistence') / marker['guest_id']
    for directory in (root, root / 'keys', root / 'homes'):
        info = root_path(directory, directory=True)
        require(stat.S_IMODE(info.st_mode) == 0o755, 'fixture parent must be root-owned 0755')
    require(marker['owned_paths'] == [str(root), '/etc/codeyard', '/etc/sudoers.d/codeyard']
            and marker['installer_temporary_prefixes'] ==
            ['/etc/sudoers.d/.codeyard.check.', '/etc/sudoers.d/.codeyard.'], 'path allowlist mismatch')
    # No guest resources on shared host filesystems, including an ancestor mount.
    for line in Path('/proc/self/mountinfo').read_text().splitlines():
        left, right = line.split(' - ', 1)
        require(right.split()[0] not in ('9p', 'virtiofs', 'nfs', 'nfs4', 'cifs', 'fuse.sshfs'),
                'shared/remote guest filesystem refused')
        mount = left.split()[4]
        require('\\' not in mount, 'escaped mount layout requires manual review')
        require(not (mount in ('/opt', '/etc') or mount.startswith(str(root)))
                or Path(mount).resolve() == Path(mount), 'ambiguous guest mount refused')
    require(not any((path / '.env').exists() or (path / '.env').is_symlink()
                    for path in (root, *root.parents)), 'dotenv input refused')
    for name in ('run', 'reload.fail'):
        require(not os.path.lexists(root / name), 'one-shot run/output collision')
    for path in (Path('/etc/codeyard'), Path('/etc/sudoers.d/codeyard'),
                 Path(str(root / 'sshd_config') + '.codeyard.bak')):
        require(not os.path.lexists(path), 'existing installer output/backup refused')
    require(not list(Path('/etc/sudoers.d').glob('.codeyard*')), 'installer temporary-name collision')
    require(type(marker['port']) is int and 49152 <= marker['port'] <= 65535,
            'test-owned high loopback SSH port required; never port22')
    require(all(type(marker[key]) is int and 0 < marker[key] <= 9223372036854775807
                for key in ('key_id', 'outsider_key_id')) and marker['key_id'] != marker['outsider_key_id']
            and all(re.fullmatch(r'[0-9a-f]{40}', marker[key]) for key in ('allowed_head', 'denied_head')),
            'synthetic separate actor/key/ref bindings required')
    for key, expected in (('source_sha256', args.expected_source_sha256),
                          ('binary_sha256', args.expected_binary_sha256),
                          ('installer_sha256', args.expected_installer_sha256)):
        require(marker[key] == expected, 'source/artifact hash binding mismatch')
    for path, key in ((root / 'codeyard', 'binary_sha256'),
                      (root / 'install-openssh.py', 'installer_sha256'),
                      (Path(__file__).absolute(), 'runner_sha256'),
                      (SSH_UNIT, 'ssh_unit_sha256'), (BROKER_UNIT, 'broker_unit_sha256')):
        root_path(path, executable=key == 'binary_sha256')
        require(HEX.fullmatch(marker[key]) and digest(path) == marker[key], 'pinned input hash mismatch')
    require(Path(__file__).absolute() == root / 'run-openssh-coexistence.py', 'runner must be provisioned inside guest')
    require(isinstance(marker['files'], dict) and set(marker['files']) == INPUT_FILES, 'fixture file allowlist mismatch')
    for name, expected in marker['files'].items():
        root_path(root / name, private=name in ('keys/host', 'keys/admin', 'keys/git', 'keys/unknown', 'keys/outsider'))
        require(HEX.fullmatch(expected) and digest(root / name) == expected, 'fixture file hash mismatch')
        if name.endswith('.pub') or name in ('keys/coexadmin.authorized_keys', 'broker-bootstrap.json'):
            info = (root / name).lstat()
            require(stat.S_IMODE(info.st_mode) == 0o644 and info.st_size <= 16384,
                    'public fixture input must be bounded and root-owned 0644')
    require((root / 'sshd_config').read_bytes() == server_config(root, marker['port'])
            and (root / 'reload-guard.py').read_bytes() == reload_guard(root)
            and SSH_UNIT.read_bytes() == ssh_unit(root), 'exact guest SSH/reload policy required')
    root_path(root / 'reload-guard.py', executable=True)
    require(isinstance(marker['accounts'], dict) and set(marker['accounts']) == set(ACCOUNT_NAMES), 'account role allowlist mismatch')
    identities = []
    for role, name in ACCOUNT_NAMES.items():
        expected = marker['accounts'][role]
        require(isinstance(expected, dict) and set(expected) == {'name', 'uid', 'gid', 'home', 'shell'}
                and expected['name'] == name and type(expected['uid']) is int
                and type(expected['gid']) is int and expected['uid'] >= 1000 and expected['gid'] >= 1000,
                'preprovisioned fixture account contract required')
        account = pwd.getpwnam(name)  # Never create/unlock users or change a shell.
        require(dict(name=account.pw_name, uid=account.pw_uid, gid=account.pw_gid,
                     home=account.pw_dir, shell=account.pw_shell) == expected
                and account.pw_dir == str(root / 'homes' / name)
                and account.pw_shell == '/bin/sh', 'existing account identity differs from provisioning')
        home = Path(account.pw_dir).lstat()
        require(stat.S_ISDIR(home.st_mode) and home.st_uid == account.pw_uid
                and home.st_gid == account.pw_gid and stat.S_IMODE(home.st_mode) == 0o700,
                'preprovisioned account home ownership mismatch')
        require(os.getgrouplist(name, account.pw_gid) == [account.pw_gid], 'fixture account has supplementary authority')
        identities.append(account.pw_uid)
    require(len(set(identities)) == 3, 'fixture identities must have separate non-root UIDs')
    tool_path(Path('/bin/sh'))
    for path in (SYSTEMCTL, SSH, SSH_KEYGEN, GIT, VIRT, Path('/usr/bin/id'), Path('/usr/bin/python3'), Path('/usr/sbin/visudo')):
        tool_path(path)
    tool_path(Path('/usr/bin/sudo'), setuid=True)
    root_path(SSHD, executable=True)
    for path in (root / 'keys/coexgit.authorized_keys', root / 'keys/coexservice.authorized_keys'):
        require(not os.path.lexists(path), 'local Git/service authorized-key bypass refused')
    root_path(root / 'keys', directory=True)
    require((root / 'keys/coexadmin.authorized_keys').read_bytes() == (root / 'keys/admin.pub').read_bytes(),
            'admin authentication must use only the provisioned synthetic key')
    root_path(Path('/etc/sudoers'))
    root_path(Path('/etc/sudoers.d'), directory=True)
    require(re.search(r'(?m)^\s*(?:@includedir|#includedir)\s+/etc/sudoers.d\s*(?:#.*)?$',
                      Path('/etc/sudoers').read_text()), 'operator must provision sudoers inclusion')
    spec = importlib.util.spec_from_file_location('coexistence_installer', root / 'install-openssh.py')
    installer = importlib.util.module_from_spec(spec)
    sys.modules[spec.name] = installer
    spec.loader.exec_module(installer)
    options = installer.Options(binary=root / 'codeyard', service_user='coexservice', git_user='coexgit',
                                socket=root / 'broker/ssh.sock', sshd_config=root / 'sshd_config',
                                sshd=SSHD, admin_users=('root', 'coexadmin'))
    host = installer.Host()  # Real guest sudo/config/tools; no fake Host or results.
    require(load_json(root / 'broker-bootstrap.json') == json.loads(
        installer.render(options, host)[host.app_dir / 'openssh.json'][0]), 'isolated broker bootstrap manifest mismatch')
    try:
        installer.preflight(options, host)  # Read-only, including real existing account checks.
    except installer.Refusal as error:
        raise Refusal('installer static guest preflight unavailable') from error
    return marker, root, installer, options, host, args.expected_marker_sha256


def children():
    return [int(value) for value in Path(f'/proc/self/task/{os.getpid()}/children').read_text().split()]


def group_members(pgid):
    result = []
    for path in Path('/proc').iterdir():
        if not path.name.isdigit():
            continue
        try:
            fields = (path / 'stat').read_text().rsplit(') ', 1)[1].split()
        except (FileNotFoundError, ProcessLookupError):
            continue
        if int(fields[2]) == pgid and fields[0] != 'Z':
            result.append(int(path.name))
    return result


class Commands:
    """Synchronous real commands; unreaped leaders anchor only OUR child groups."""
    def __init__(self, root):
        self.root, self.records = root, []
        require(not children(), 'runner must begin without borrowed children')
        require(ctypes.CDLL(None, use_errno=True).prctl(36, 1, 0, 0, 0) == 0, 'child subreaper unavailable')

    def reap_owned(self):
        clean, deadline = True, time.monotonic() + 10
        while children():
            require(time.monotonic() < deadline, 'owned child drain uncertain')
            for pid in children():
                # An unreaped direct/adopted child cannot be a reused or unrelated PID.
                if os.waitid(os.P_PID, pid, os.WEXITED | os.WNOHANG | os.WNOWAIT) is None:
                    clean = False
                    os.kill(pid, signal.SIGKILL)
                else:
                    os.waitpid(pid, 0)
            time.sleep(0.02)
        return clean

    def run(self, argv, label, user=None, environment=None, seconds=30):
        require(argv and Path(argv[0]).is_absolute(), 'absolute command required')
        paths = [self.root / f'command-{len(self.records):04d}-{name}' for name in ('out', 'err')]
        streams, process = [], None
        record = {'label': label, 'group_drained': False}
        self.records.append(record)
        def bounds():
            resource.setrlimit(resource.RLIMIT_CORE, (0, 0))
            resource.setrlimit(resource.RLIMIT_FSIZE, (1048576, 1048576))
        try:
            for path in paths:
                fd = os.open(path, os.O_RDWR | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW, 0o600)
                streams.append(os.fdopen(fd, 'w+b'))
            identity = {} if user is None else dict(user=user.pw_uid, group=user.pw_gid, extra_groups=[])
            process = subprocess.Popen([str(value) for value in argv], stdin=subprocess.DEVNULL,
                stdout=streams[0], stderr=streams[1], cwd='/', env={**ENV, **(environment or {})},
                start_new_session=True, preexec_fn=bounds, **identity)
            record['pid'] = process.pid
            record['start_ticks'] = Path(f'/proc/{process.pid}/stat').read_text().rsplit(') ', 1)[1].split()[19]
            deadline = time.monotonic() + seconds
            while os.waitid(os.P_PID, process.pid, os.WEXITED | os.WNOHANG | os.WNOWAIT) is None:
                require(time.monotonic() < deadline, 'owned command timed out')
                time.sleep(0.02)
            require(not group_members(process.pid), 'owned descendants outlived command leader')
        finally:
            if process is not None:
                # Do not poll()/communicate()/wait() before this group kill: PID stays anchored.
                try:
                    os.killpg(process.pid, signal.SIGKILL)
                except ProcessLookupError:
                    pass
                record['returncode'] = process.wait(timeout=10)
                record['group_drained'] = self.reap_owned() and not group_members(process.pid)
            data = []
            for stream, path in zip(streams, paths):
                stream.seek(0)
                data.append(stream.read(1048577))
                info = os.fstat(stream.fileno())
                current = path.lstat()
                require((info.st_dev, info.st_ino) == (current.st_dev, current.st_ino), 'command output ownership changed')
                stream.close()
                path.unlink()
        require(record['group_drained'] and all(len(value) <= 1048576 for value in data), 'owned command cleanup/output bound failed')
        return record['returncode'], data[0], data[1]

    def success(self, argv, label, **kwargs):
        code, output, _ = self.run(argv, label, **kwargs)
        require(code == 0, label + ' failed')
        return output


def properties(output):
    return dict(line.split('=', 1) for line in output.decode().splitlines() if '=' in line)


def private_file(path, content):
    fd = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW, 0o600)
    with os.fdopen(fd, 'wb') as stream:
        stream.write(content)
        stream.flush()
        os.fsync(stream.fileno())


class Rehearsal:
    def __init__(self, marker, root, installer, options, host, marker_sha256):
        self.m, self.root, self.i, self.o, self.h = marker, root, installer, options, host
        self.marker_sha256 = marker_sha256
        self.original = installer.snapshot(options.sshd_config)
        require(self.original[0] == server_config(root, marker['port']), 'initial config changed after contract validation')
        self.main_expected = self.original
        self.installed_files, self.installed_dir = {}, None
        self.checks, self.services = [], {}
        self.guard_inputs()
        (root / 'run').mkdir(mode=0o700)
        self.commands = Commands(root / 'run')
        self.phase = 'runtime preflight'
        installer.run = self.installer_command  # Concrete owned real executor, never simulated results.

    def guard_inputs(self):
        root_path(MARKER, private=True)
        require(digest(MARKER) == self.marker_sha256 and load_json(MARKER) == self.m,
                'operator marker changed; no further mutation authorized')
        for path, key in ((self.o.binary, 'binary_sha256'), (self.root / 'install-openssh.py', 'installer_sha256'),
                          (self.root / 'run-openssh-coexistence.py', 'runner_sha256'),
                          (SSH_UNIT, 'ssh_unit_sha256'), (BROKER_UNIT, 'broker_unit_sha256')):
            root_path(path, executable=key == 'binary_sha256')
            require(digest(path) == self.m[key], 'pinned source/artifact/unit changed')
        for name, expected in self.m['files'].items():
            if name != 'sshd_config':
                root_path(self.root / name)
                require(digest(self.root / name) == expected, 'provisioned fixture input changed')

    def installer_command(self, argv, missing_unit=False):
        self.guard_inputs()
        argv = [str(value) for value in argv]
        tool = Path(argv[0])
        allowed = False
        if tool == SYSTEMCTL:
            allowed = argv[1:2] == ['show'] and argv[2] in ('ssh.service', 'sshd.service')
            allowed |= argv[1:] == ['reload', 'ssh.service']
        elif tool == SSHD:
            allowed = argv[1:4] in (['-t', '-f', str(self.o.sshd_config)], ['-T', '-f', str(self.o.sshd_config)])
        elif tool == self.h.sudo:
            allowed = argv[1:] == ['-V']
        elif tool == self.h.visudo:
            allowed = argv[1:] == ['-c'] or (len(argv) == 4 and argv[1:3] == ['-c', '-f']
                and Path(argv[3]).parent == self.h.sudoers_dir and Path(argv[3]).name.startswith('.codeyard.check.'))
        require(allowed, 'installer attempted an operation outside guest allowlist')
        if argv == [str(SYSTEMCTL), 'reload', 'ssh.service']:
            require(not os.path.lexists(self.h.app_dir / 'openssh.json'),
                    'installer published manifest before real reload acceptance')
        code, output, _ = self.commands.run(argv, 'installer:' + tool.name + ':' + argv[1])
        if code and not (missing_unit and b'LoadState=not-found' in output.splitlines()):
            raise self.i.Refusal('real guest command failed: ' + tool.name + ':' + argv[1])
        return output.decode()

    def service(self, unit, binary, uid):
        output = self.commands.success([SYSTEMCTL, 'show', unit,
            '--property=Id,LoadState,ActiveState,MainPID,User,ControlGroup,KillMode,NeedDaemonReload,CanReload'], 'service identity')
        values = properties(output)
        require(values.get('Id') == unit and values.get('LoadState') == 'loaded'
                and values.get('ActiveState') == 'active' and values.get('NeedDaemonReload') == 'no'
                and values.get('KillMode') == 'control-group'
                and values.get('User') == ('root' if uid == 0 else 'coexservice'), 'dedicated guest service identity failed')
        pid = values.get('MainPID', '')
        require(re.fullmatch(r'[1-9][0-9]*', pid), 'guest service MainPID missing')
        process = Path('/proc') / pid
        require(os.readlink(process / 'exe') == str(binary) and process.stat().st_uid == uid
                and digest(process / 'exe') == digest(binary), 'guest service executable/UID mismatch')
        cgroup = values.get('ControlGroup', '')
        require(cgroup == '/system.slice/' + unit, 'dedicated guest cgroup required')
        ticks = (process / 'stat').read_text().rsplit(') ', 1)[1].split()[19]
        return {'pid': int(pid), 'cgroup': cgroup, 'start_ticks': ticks}

    def drained_services(self):
        deadline = time.monotonic() + 10
        while True:
            settled = True
            for unit, identity in self.services.items():
                ticks = Path(f'/proc/{identity["pid"]}/stat').read_text().rsplit(') ', 1)[1].split()[19]
                require(ticks == identity['start_ticks'], 'original guest service leader changed')
                root = Path('/sys/fs/cgroup') / identity['cgroup'].lstrip('/')
                procs = set()
                for path in (root / 'cgroup.procs', *root.rglob('cgroup.procs')):
                    procs.update(int(pid) for pid in path.read_text().split())
                settled &= procs == {identity['pid']}
            if settled:
                return
            require(time.monotonic() < deadline, 'guest SSH/broker session producer drain uncertain; no service kill authorized')
            time.sleep(0.05)

    def listeners(self):
        require(Path('/sys/fs/cgroup/cgroup.controllers').is_file(), 'guest cgroup v2 required')
        rows = []
        for family in ('tcp', 'tcp6'):
            for line in Path('/proc/net/' + family).read_text().splitlines()[1:]:
                fields = line.split()
                if fields[3] == '0A':
                    require(int(fields[1].rsplit(':', 1)[1], 16) != 22, 'guest has port22 listener; refusing')
                    rows.append((family, fields[1], fields[9]))
        pid = self.services['ssh.service']['pid']
        sockets = {os.readlink(path) for path in Path(f'/proc/{pid}/fd').iterdir()}
        owned = [(family, address) for family, address, inode in rows if f'socket:[{inode}]' in sockets]
        require(owned == [('tcp', f'0100007F:{self.m["port"]:04X}')], 'sshd must own exactly the marker high loopback listener')
        service = pwd.getpwnam('coexservice')
        info = self.o.socket.lstat()
        require(stat.S_ISSOCK(info.st_mode) and info.st_uid == service.pw_uid and not info.st_mode & 0o077,
                'private native broker socket ownership failed')
        pid = self.services['codeyard-coexistence-broker.service']['pid']
        sockets = {os.readlink(path) for path in Path(f'/proc/{pid}/fd').iterdir()}
        entries = [line.split() for line in Path('/proc/net/unix').read_text().splitlines()[1:]]
        require(any(len(row) == 8 and row[7] == str(self.o.socket) and row[5] == '01'
                    and f'socket:[{row[6]}]' in sockets for row in entries), 'socket listener must belong to pinned native broker')

    def ssh_config(self, name, user, key):
        path = self.root / 'run' / (name + '.ssh')
        private_file(path, (f'Host fixture\n HostName 127.0.0.1\n Port {self.m["port"]}\n User {user}\n'
            f' IdentityFile {self.root}/keys/{key}\n IdentityAgent none\n IdentitiesOnly yes\n BatchMode yes\n'
            ' PasswordAuthentication no\n KbdInteractiveAuthentication no\n PreferredAuthentications publickey\n'
            ' StrictHostKeyChecking yes\n HostKeyAlias coexistence-owned\n'
            f' UserKnownHostsFile {self.root}/run/known_hosts\n GlobalKnownHostsFile /dev/null\n'
            ' ControlMaster no\n ControlPath none\n ForwardAgent no\n ClearAllForwardings yes\n'
            ' ConnectTimeout 5\n ConnectionAttempts 1\n').encode())
        return path

    def ssh_request(self, config, command=None, extra=()):
        argv = [SSH, '-F', config, '-T', *extra, 'fixture']
        if command is not None:
            argv.append(command)
        result = self.commands.run(argv, 'fresh loopback SSH request', seconds=15)
        self.drained_services()
        return result

    def admin(self):
        code, output, _ = self.ssh_request(self.admin_config, '/usr/bin/id -u')
        require(code == 0 and output == f'{self.m["accounts"]["admin"]["uid"]}\n'.encode(), 'fresh separate admin authentication failed')
        require(self.i.effective(self.o, 'coexadmin') == self.admin_policy
                and self.i.effective(self.o, 'root') == self.root_policy, 'complete administrative SSH policy changed')
        require(digest(self.root / 'keys/coexadmin.authorized_keys') == self.m['files']['keys/coexadmin.authorized_keys'],
                'separate admin authorized keys changed')

    def main_file(self, content):
        self.guard_inputs()
        self.i.atomic_file(self.o.sshd_config, content, self.original[1], self.main_expected, self.original[3])
        self.main_expected = self.i.snapshot(self.o.sshd_config)

    def reload(self):
        self.guard_inputs()
        self.commands.success([SSHD, '-t', '-f', self.o.sshd_config], 'owned sshd syntax')
        self.commands.success([SYSTEMCTL, 'reload', 'ssh.service'], 'owned sshd reload')
        self.drained_services()
        self.listeners()

    def absent_outputs(self):
        require(all(not os.path.lexists(path) for path in self.i.render(self.o, self.h))
                and not self.h.app_dir.exists(), 'rejected installation left managed output; ownership uncertain')

    def exercise(self):
        virt = self.commands.success([VIRT, '--vm'], 'independent VM detection').strip()
        require(virt in (b'kvm', b'qemu', b'vmware', b'microsoft', b'oracle', b'xen', b'parallels', b'bochs'),
                'supported real VM identity evidence required')
        self.services = {'ssh.service': self.service('ssh.service', SSHD, 0),
            'codeyard-coexistence-broker.service': self.service('codeyard-coexistence-broker.service',
                self.o.binary, self.m['accounts']['service']['uid'])}
        self.drained_services()
        self.listeners()
        require(self.i.active_service(self.o, self.h) == 'ssh.service', 'installer must target dedicated guest ssh.service')
        for name in ('host', 'admin', 'git', 'unknown', 'outsider'):
            public = (self.root / 'keys' / (name + '.pub')).read_bytes().split()
            require(len(public) in (2, 3) and public[0] == b'ssh-ed25519', 'synthetic Ed25519 fixture key required')
            actual = self.commands.success([SSH_KEYGEN, '-y', '-P', '', '-f', self.root / 'keys' / name],
                'synthetic private/public fixture key binding').split()
            require(len(actual) in (2, 3) and actual[:2] == public[:2], 'invalid/encrypted/mismatched fixture key pair')
        host = (self.root / 'keys/host.pub').read_text().split()
        require(len(host) in (2, 3) and host[0] == 'ssh-ed25519'
                and re.fullmatch(r'[A-Za-z0-9+/]+={0,2}', host[1]), 'synthetic host public key required')
        private_file(self.root / 'run/known_hosts', ('coexistence-owned ' + ' '.join(host[:2]) + '\n').encode())
        code, output, error = self.commands.run(['/usr/bin/sudo', '-n', '-l', '-U', 'coexgit'], 'Git account has no prior sudo grant')
        require(code == 1 and b'not allowed to run sudo' in output + error, 'pre-existing Git sudo authority or unverifiable sudo policy')
        self.admin_config = self.ssh_config('admin', 'coexadmin', 'admin')
        self.git_config = self.ssh_config('git', 'coexgit', 'git')
        self.unknown_config = self.ssh_config('unknown', 'coexgit', 'unknown')
        self.outsider_config = self.ssh_config('outsider', 'coexgit', 'outsider')
        self.admin_policy = self.i.effective(self.o, 'coexadmin')
        self.root_policy = self.i.effective(self.o, 'root')
        self.admin()
        self.checks.append('fresh separate admin authentication and full policy baseline')

        self.phase = 'loaded conflicting integration refusal'
        conflict = self.original[0] + b'Match User coexgit\n ForceCommand /usr/bin/false\nMatch all\n'
        self.main_file(conflict)
        self.reload()  # Existing conflicting forced-command policy is actually loaded.
        self.admin()
        count = len(self.commands.records)
        try:
            self.i.apply(self.o, self.h)
        except self.i.Refusal as error:
            require('Prior Match block' in str(error), 'wrong conflicting-policy refusal')
        else:
            raise Refusal('installer replaced an existing integration')
        require(len(self.commands.records) == count and self.i.snapshot(self.o.sshd_config) == self.main_expected,
                'conflict refusal mutated/reloaded policy')
        self.absent_outputs()
        self.main_file(self.original[0])
        self.reload()
        self.admin()
        self.checks.append('loaded Forgejo-equivalent forced-command policy refused without overwrite')

        self.phase = 'real systemd reload failure rollback'
        flag = self.root / 'reload.fail'
        private_file(flag, ('coexistence reload failure ' + self.root.name + '\n').encode())
        count = len(self.commands.records)
        try:
            self.i.apply(self.o, self.h)
        except self.i.Refusal:
            pass
        else:
            raise Refusal('installer accepted failed systemd reload')
        reloads = [record['returncode'] for record in self.commands.records[count:]
                   if record['label'] == 'installer:systemctl:reload']
        require(len(reloads) == 2 and reloads[0] != 0 and reloads[1] == 0
                and not os.path.lexists(flag), 'real reload failure/recovery was not exercised')
        self.absent_outputs()
        require(self.i.snapshot(self.o.sshd_config)[:4] == self.original[:4], 'failed reload did not restore original config')
        self.main_expected = self.i.snapshot(self.o.sshd_config)
        self.admin()
        self.checks.append('real systemd reload failure, no manifest, exact rollback and recovery reload')

        self.phase = 'successful privileged install and idempotence'
        require(self.i.apply(self.o, self.h) == 'ssh.service', 'installer acceptance failed')
        self.main_expected = self.i.snapshot(self.o.sshd_config)
        self.installed_dir = self.i.directory_state(self.h.app_dir)
        for path, (content, mode) in self.i.render(self.o, self.h).items():
            state = self.i.snapshot(path)
            require(state[:4] == (content, mode, 0, 0), 'successful installer output mismatch')
            self.installed_files[path] = state
        require(self.main_expected[0] == self.original[0] + self.i.inclusion(self.h), 'installed main config mismatch')
        before = dict(self.installed_files)
        count = len(self.commands.records)
        self.i.apply(self.o, self.h)
        require(all(self.i.snapshot(path) == expected for path, expected in before.items())
                and self.i.snapshot(self.o.sshd_config) == self.main_expected
                and not any(record['label'] == 'installer:systemctl:reload' for record in self.commands.records[count:]),
                'exact repeated install was not idempotent')
        self.admin()
        self.checks.append('actual privileged installer, manifest-last validation and exact idempotence')

        self.phase = 'native broker and actual installed sudo forced command'
        public = (self.root / 'keys/git.pub').read_text().split()
        require(len(public) in (2, 3) and public[0] == 'ssh-ed25519', 'synthetic registered Git key required')
        line = self.commands.success([self.o.binary, 'ssh', 'keys', '--socket', self.o.socket,
            '--expected-user', 'coexgit', '--serve-command', self.o.helper_dir / 'ssh-serve',
            'coexgit', *public[:2]], 'native broker registered key', user=pwd.getpwnam('coexservice'))
        expected = (f'restrict,command="{self.o.helper_dir}/ssh-serve {self.m["key_id"]}" '
                    + ' '.join(public[:2]) + '\n').encode()
        require(line == expected, 'native broker did not issue expected restricted forced command')
        git_env = {'GIT_SSH_COMMAND': f'{SSH} -F {self.git_config}', 'GIT_SSH_VARIANT': 'ssh'}
        url = f'ssh://coexgit@fixture:{self.m["port"]}/coexistence/allowed.git'
        refs = self.commands.success([GIT, 'ls-remote', '--', url, 'refs/heads/main'], 'actual SSH Git read', environment=git_env)
        self.drained_services()
        require(refs == f'{self.m["allowed_head"]}\trefs/heads/main\n'.encode(), 'synthetic native Git ref mismatch')
        outsider = (self.root / 'keys/outsider.pub').read_text().split()
        require(len(outsider) in (2, 3) and outsider[0] == 'ssh-ed25519', 'synthetic outsider key required')
        line = self.commands.success([self.o.binary, 'ssh', 'keys', '--socket', self.o.socket,
            '--expected-user', 'coexgit', '--serve-command', self.o.helper_dir / 'ssh-serve',
            'coexgit', *outsider[:2]], 'native broker outsider key', user=pwd.getpwnam('coexservice'))
        require(line == (f'restrict,command="{self.o.helper_dir}/ssh-serve {self.m["outsider_key_id"]}" '
                         + ' '.join(outsider[:2]) + '\n').encode(), 'outsider native identity binding mismatch')
        denied_url = f'ssh://coexgit@fixture:{self.m["port"]}/outsider/denied.git'
        refs = self.commands.success([GIT, 'ls-remote', '--', denied_url, 'refs/heads/main'], 'private fixture existence proof',
            environment={'GIT_SSH_COMMAND': f'{SSH} -F {self.outsider_config}', 'GIT_SSH_VARIANT': 'ssh'})
        self.drained_services()
        require(refs == f'{self.m["denied_head"]}\trefs/heads/main\n'.encode(), 'actual private denied repository fixture missing')
        for command in (None, 'echo COEXISTENCE_SHELL_ESCAPE', "git-upload-pack '../../../etc/passwd'",
                        "git-upload-pack 'coexistence/allowed.git'; echo COEXISTENCE_SHELL_ESCAPE",
                        "git-upload-pack 'outsider/denied.git'"):
            code, output, _ = self.ssh_request(self.git_config, command)
            require(code not in (0, 255) and not output, 'authenticated native deny path was not exercised')
        code, output, error = self.ssh_request(self.unknown_config, "git-upload-pack 'coexistence/allowed.git'")
        require(code == 255 and not output and b'Permission denied' in error, 'unknown signed key was not denied by actual sshd')
        code, output, error = self.ssh_request(self.git_config, 'echo denied', extra=('-tt',))
        require(code != 0 and not output and b'PTY allocation request failed' in error, 'Git PTY restriction failed')
        # ClearAllForwardings is disabled only for this deliberately denied request.
        code, output, error = self.ssh_request(self.git_config, extra=('-N', '-o', 'ClearAllForwardings=no',
            '-o', 'ExitOnForwardFailure=yes', '-R', f'127.0.0.1:0:127.0.0.1:{self.m["port"]}'))
        require(code == 255 and not output and b'remote port forwarding failed' in error, 'Git forwarding restriction failed')
        self.admin()
        self.listeners()
        self.checks.append('actual native broker/sudo Git ref, shell/injection/traversal/private-repo/unknown-key/PTY/forwarding denials')

    def cleanup(self):
        # Never stop pre-existing services or kill their children to manufacture drain.
        self.drained_services()
        self.guard_inputs()
        require(self.i.snapshot(self.o.sshd_config) == self.main_expected, 'main config concurrent change; manual guest recovery required')
        if self.main_expected[:4] != self.original[:4]:
            self.main_file(self.original[0])
            self.reload()
        if hasattr(self, 'admin_config'):
            self.admin()
        for path, expected in reversed(list(self.installed_files.items())):
            require(self.i.snapshot(path) == expected, 'managed file concurrent change; leaving untouched')
            path.unlink()
        if self.installed_dir is not None:
            require(self.i.directory_state(self.h.app_dir) == self.installed_dir, 'managed directory changed; leaving untouched')
            self.h.app_dir.rmdir()  # Only empty, still-identical directory; never recursive.
        flag = self.root / 'reload.fail'
        # A surviving failure sentinel is deliberately retained for operator review.
        require(not os.path.lexists(flag), 'failure sentinel retained; manual guest recovery required')
        self.absent_outputs()
        backup = Path(str(self.o.sshd_config) + '.codeyard.bak')
        if backup.exists():
            require(self.i.snapshot(backup)[:4] == (self.original[0], 0o600, 0, 0), 'retained original backup differs')
        require(not list(self.h.sudoers_dir.glob('.codeyard*')), 'installer temporary file cleanup uncertain')
        require(self.commands.reap_owned() and all(record['group_drained'] for record in self.commands.records),
                'owned child group cleanup uncertain')
        self.drained_services()
        self.listeners()
        require(self.service('ssh.service', SSHD, 0) == self.services['ssh.service']
                and self.service('codeyard-coexistence-broker.service', self.o.binary,
                    self.m['accounts']['service']['uid']) == self.services['codeyard-coexistence-broker.service'],
                'preprovisioned service identity changed')
        return {'owned_children_drained': True, 'service_cgroups_at_original_leaders': True,
                'original_sshd_config_restored': True, 'managed_outputs_removed': True,
                'preprovisioned_services_retained_not_stopped': sorted(self.services),
                'original_backup_retained': str(backup) if backup.exists() else None}

    def execute(self):
        error, cleanup = None, {'proved': False}
        try:
            self.exercise()
        except BaseException:
            error = self.phase + ' failed; inspect disposable guest via console'
        finally:
            signal.signal(signal.SIGINT, signal.SIG_IGN)
            signal.signal(signal.SIGTERM, signal.SIG_IGN)
            try:
                cleanup = {'proved': True, **self.cleanup()}
            except BaseException:
                error = (error or self.phase + ' failed') + '; cleanup unproved, retain guest for manual recovery'
        receipt = {'schema': 1, 'purpose': PURPOSE, 'passed': error is None,
                   'marker_sha256': self.marker_sha256,
                   'source_sha256': self.m['source_sha256'], 'binary_sha256': self.m['binary_sha256'],
                   'installer_sha256': self.m['installer_sha256'], 'runner_sha256': self.m['runner_sha256'],
                   'machine_id': self.m['machine_id'], 'guest_uuid': self.m['guest_uuid'],
                   'guest_id': self.m['guest_id'], 'port': self.m['port'], 'checks': self.checks,
                   'retained_service_identities': self.services,
                   'commands': self.commands.records, 'cleanup': cleanup, 'failure': error}
        private_file(self.root / 'run/receipt.json.pending', (json.dumps(receipt, indent=2) + '\n').encode())
        os.link(self.root / 'run/receipt.json.pending', self.root / 'run/receipt.json')
        (self.root / 'run/receipt.json.pending').unlink()
        fd = os.open(self.root / 'run', os.O_DIRECTORY)
        try:
            os.fsync(fd)
        finally:
            os.close(fd)
        print(json.dumps({'passed': receipt['passed'], 'receipt': str(self.root / 'run/receipt.json'), 'failure': error}))
        return 0 if receipt['passed'] else 1


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--execute', action='store_true', help='Authorize ONLY the marker-owned disposable guest rehearsal')
    for name in ('expected-machine-id', 'forbidden-machine-id', 'expected-guest-uuid', 'forbidden-guest-uuid',
                 'expected-marker-sha256', 'expected-source-sha256', 'expected-binary-sha256', 'expected-installer-sha256'):
        parser.add_argument('--' + name, required=True)
    args = parser.parse_args(argv)
    def interrupted(_number, _frame):
        raise Refusal('rehearsal interrupted')
    try:
        contract = static_contract(args)
        if not args.execute:
            print('Static read-only guest contract accepted. NO runtime/installer acceptance was executed.')
            return 0
        signal.signal(signal.SIGINT, interrupted)
        signal.signal(signal.SIGTERM, interrupted)
        return Rehearsal(*contract).execute()
    except (Refusal, OSError, ValueError, KeyError, TypeError):
        print('REFUSED: disposable guest contract/operation unavailable. No gate pass.', file=sys.stderr)
        return 1


if __name__ == '__main__':
    sys.exit(main())
