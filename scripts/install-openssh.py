#!/usr/bin/env python3
"""Explicit, fail-closed native-host OpenSSH setup. No application startup here."""
import argparse
import dataclasses
import fnmatch
import fcntl
import glob
import json
import os
from pathlib import Path
import pwd
import re
import shlex
import stat
import subprocess
import sys
import tempfile


ROOT_UID = 0
ROOT_GID = 0
MARKER = "# Managed by codeyard install-openssh.py; schema 1\n"
BEGIN = "# BEGIN codeyard OpenSSH schema 1"
END = "# END codeyard OpenSSH schema 1"
NAME = re.compile(r"[a-z_][a-z0-9_-]{0,31}\Z", re.ASCII)
PATH = re.compile(r"/[A-Za-z0-9_./-]+\Z", re.ASCII)
CONFLICT = "Choose a separate Git username; do not replace Forgejo or another SSH integration."
UNSET = object()


class Refusal(Exception):
    pass


@dataclasses.dataclass(frozen=True)
class Options:
    binary: Path = Path("/usr/local/bin/codeyard")
    service_user: str = "gitserv"
    git_user: str = "git"
    socket: Path = Path("/run/codeyard/ssh.sock")
    helper_dir: Path = Path("/etc/codeyard")
    sshd_config: Path = Path("/etc/ssh/sshd_config")
    sshd: Path = Path("/usr/sbin/sshd")
    admin_users: tuple = ("root",)


@dataclasses.dataclass(frozen=True)
class Host:
    # Fixed tools, not PATH searches. Alternate layouts require manual integration.
    app_dir: Path = Path("/etc/codeyard")
    sudoers: Path = Path("/etc/sudoers")
    sudoers_dir: Path = Path("/etc/sudoers.d")
    sudo: Path = Path("/usr/bin/sudo")
    visudo: Path = Path("/usr/sbin/visudo")
    systemctl: Path = Path("/usr/bin/systemctl")
    python: Path = Path("/usr/bin/python3")
    nixos: Path = Path("/etc/NIXOS")
    systemd: Path = Path("/run/systemd/system")
    ssh_include_base: Path = Path("/etc/ssh")
    proc: Path = Path("/proc")


def path_value(value):
    text = str(value)
    if (not PATH.fullmatch(text) or "//" in text or text.endswith("/")
            or any(p in (".", "..") for p in text.split("/"))):
        raise Refusal(f"Unsafe absolute path: {text!r}")
    return Path(text)


def validate_options(o):
    for user in (o.git_user, o.service_user, *o.admin_users):
        if not NAME.fullmatch(user):
            raise Refusal(f"Unsafe account name: {user!r}")
    if o.git_user == o.service_user or {o.git_user, o.service_user}.intersection(o.admin_users):
        raise Refusal("Git, service and administrative identities must be separate.")
    for value in (o.binary, o.socket, o.helper_dir, o.sshd_config, o.sshd):
        path_value(value)
    if len(os.fsencode(o.socket)) > 107:
        raise Refusal("Socket path exceeds the Linux Unix-socket path limit.")
    if o.socket.parent == Path("/"):
        raise Refusal("The broker requires its own service-user-owned 0700 directory.")


def root_path(path, *, missing=False, executable=False, directory=False, public=False):
    """Reject symlinks and writable ancestors, including the final component."""
    path = path_value(path)
    for part in (*reversed(path.parents), path):
        try:
            info = part.lstat()
        except FileNotFoundError:
            if missing and part == path:
                return None
            raise Refusal(f"Required root-owned parent/path is missing: {part}")
        if stat.S_ISLNK(info.st_mode):
            raise Refusal(f"Refusing symlink: {part} (declarative/store layouts need manual setup)")
        if info.st_uid != ROOT_UID or info.st_mode & 0o022:
            raise Refusal(f"Not root-owned and non-group/world-writable: {part}")
        if part != path and not stat.S_ISDIR(info.st_mode):
            raise Refusal(f"Not a directory: {part}")
        if (public or executable) and (directory or part != path) and not info.st_mode & 0o001:
            raise Refusal(f"Service/Git users cannot traverse root-owned directory: {part}")
    if directory:
        if not stat.S_ISDIR(info.st_mode):
            raise Refusal(f"Not a directory: {path}")
    elif not stat.S_ISREG(info.st_mode):
        raise Refusal(f"Not a regular file: {path}")
    if executable and (not info.st_mode & 0o001 or info.st_mode & 0o6000):
        raise Refusal(f"Expected a non-setid, root-owned, world-executable binary: {path}")
    return info


def tool_path(path, *, setuid=False):
    # Distribution interpreter/sudo symlinks are allowed only through root-safe
    # ancestors to root-safe targets. Operator-selected app/config paths are not.
    current = path_value(path)
    for _ in range(16):
        for parent in reversed(current.parents):
            info = parent.lstat()
            if info.st_uid != ROOT_UID or (not stat.S_ISLNK(info.st_mode) and info.st_mode & 0o022):
                raise Refusal(f"Unsafe system-tool parent: {parent}")
        root_path(current.parent.resolve(strict=True), directory=True, public=True)
        info = current.lstat()
        if info.st_uid != ROOT_UID or (not stat.S_ISLNK(info.st_mode) and info.st_mode & 0o022):
            raise Refusal(f"Unsafe system tool: {current}")
        if not stat.S_ISLNK(info.st_mode):
            root_path(current.parent.resolve(strict=True) / current.name, public=True)
            if not info.st_mode & 0o001 or info.st_mode & 0o2000:
                raise Refusal(f"Not a safe executable: {current}")
            if bool(info.st_mode & stat.S_ISUID) != setuid:
                raise Refusal(f"Unexpected setuid state for system tool: {current}")
            return
        target = Path(os.readlink(current))
        current = Path(os.path.normpath(target if target.is_absolute() else current.parent / target))
    raise Refusal(f"Too many symlinks in system tool: {path}")


def accounts(o):
    try:
        service = pwd.getpwnam(o.service_user)
        git = pwd.getpwnam(o.git_user)
        admins = [pwd.getpwnam(user) for user in o.admin_users]
    except KeyError as error:
        raise Refusal(f"Account must already exist: {error}") from error
    if service.pw_uid == 0 or git.pw_uid == 0 or service.pw_uid == git.pw_uid:
        raise Refusal("Git and service accounts need distinct non-root UIDs.")
    if any(admin.pw_uid in (service.pw_uid, git.pw_uid) for admin in admins):
        raise Refusal("Administrative accounts must not alias the Git/service UIDs.")
    if Path(git.pw_shell).name in ("false", "nologin"):
        raise Refusal("Git account needs a working login shell for forced commands; change it explicitly.")
    tool_path(Path(git.pw_shell))
    return service, git


def render(o, h):
    keys = o.helper_dir / "ssh-keys"
    serve = o.helper_dir / "ssh-serve"
    # -I excludes user Python paths, startup hooks and PYTHON* environment.
    keys_text = f'''#!{h.python} -I
{MARKER}import os
import re
import sys

if (len(sys.argv) != 4 or sys.argv[1] != {o.git_user!r}
        or not re.fullmatch(r"[A-Za-z0-9@._+-]{{1,64}}", sys.argv[2], re.ASCII)
        or not re.fullmatch(r"[A-Za-z0-9+/]{{1,32768}}={{0,2}}", sys.argv[3], re.ASCII)):
    sys.exit(1)
os.chdir("/")
os.execve({str(o.binary)!r}, [{str(o.binary)!r}, "ssh", "keys", "--socket", {str(o.socket)!r},
    "--expected-user", {o.git_user!r}, "--serve-command", {str(serve)!r}, *sys.argv[1:]], {{}})
'''
    serve_text = f'''#!{h.python} -I
{MARKER}import os
import re
import sys

if (len(sys.argv) != 2 or not re.fullmatch(r"[1-9][0-9]{{0,18}}", sys.argv[1], re.ASCII)
        or int(sys.argv[1]) > 9223372036854775807):
    sys.exit(1)
original = os.environ.get("SSH_ORIGINAL_COMMAND", "")
protocol = os.environ.get("GIT_PROTOCOL")
if not original or len(original) > 8192 or (protocol is not None and len(protocol) > 4096):
    sys.exit(1)
environment = {{"SSH_ORIGINAL_COMMAND": original}}
if protocol is not None:
    environment["GIT_PROTOCOL"] = protocol
os.chdir("/")
os.execve({str(h.sudo)!r}, [{str(h.sudo)!r}, "-n", "-u", {o.service_user!r},
    "--preserve-env=SSH_ORIGINAL_COMMAND,GIT_PROTOCOL", "--", {str(o.binary)!r},
    "ssh", "serve", "--socket", {str(o.socket)!r}, "--key-id", sys.argv[1]], environment)
'''
    # Sudo >= 1.9.10 supports anchored argument regexes. No wildcard that can
    # consume whitespace, options, other subcommands or arbitrary shell commands.
    socket_regex = str(o.socket).replace(".", "[.]")
    sudoers = (MARKER
        + f"Cmnd_Alias GIT_SERV_37_SSH = {o.binary} ^ssh serve --socket {socket_regex} --key-id [1-9][0-9]*$\n"
        + 'Defaults!GIT_SERV_37_SSH env_reset, !setenv, !requiretty, env_keep += "SSH_ORIGINAL_COMMAND GIT_PROTOCOL"\n'
        + f"{o.git_user} ALL=({o.service_user}) NOPASSWD: NOSETENV: GIT_SERV_37_SSH\n")
    snippet = (MARKER + f"Match User {o.git_user}\n"
        + "    AuthenticationMethods publickey\n    PubkeyAuthentication yes\n"
        + "    PasswordAuthentication no\n    KbdInteractiveAuthentication no\n"
        + "    HostbasedAuthentication no\n    AuthorizedKeysFile none\n"
        + f"    AuthorizedKeysCommand {keys} %u %t %k\n"
        + f"    AuthorizedKeysCommandUser {o.service_user}\n"
        + "    DisableForwarding yes\n    PermitTTY no\n    PermitUserRC no\n"
        + "    AcceptEnv GIT_PROTOCOL\nMatch all\n")
    manifest = json.dumps(dict(schema=1, git_user=o.git_user, service_user=o.service_user,
        socket=str(o.socket), serve_command=str(serve), binary=str(o.binary)), sort_keys=True, indent=2) + "\n"
    return {
        keys: (keys_text.encode(), 0o755),
        serve: (serve_text.encode(), 0o755),
        h.sudoers_dir / "codeyard": (sudoers.encode(), 0o440),
        h.app_dir / "sshd.conf": (snippet.encode(), 0o644),
        h.app_dir / "openssh.json": (manifest.encode(), 0o644),
    }


def inclusion(h):
    return f"\n{BEGIN}\nInclude {h.app_dir / 'sshd.conf'}\n{END}\n".encode()


def unmanaged_main(content, h):
    suffix = inclusion(h)
    if BEGIN.encode() in content or END.encode() in content:
        if (content.count(BEGIN.encode()) != 1 or content.count(END.encode()) != 1
                or not content.endswith(suffix)):
            raise Refusal("Managed main-config block changed or is not at EOF; refusing to rewrite it.")
        return content[:-len(suffix)]
    return content


def config_lines(path, h, snippet, content=None, seen=None):
    """Read only; refuse ambiguous contexts instead of implementing sshd's parser."""
    if seen is None:
        seen = set()
    if path in seen or len(seen) >= 128:
        raise Refusal("Cyclic or excessive sshd includes; manual integration required.")
    seen.add(path)
    root_path(path)
    text = (path.read_bytes() if content is None else content).decode("utf-8")
    for number, line in enumerate(text.splitlines(), 1):
        line = re.sub(r"^\s*([A-Za-z]+)=", r"\1 ", line)
        try:
            words = shlex.split(line, comments=True)
        except ValueError as error:
            raise Refusal(f"Cannot safely parse {path}:{number}: {error}") from error
        if not words:
            continue
        name, values = words[0].lower(), words[1:]
        if name == "include":
            if not values:
                raise Refusal(f"Empty Include at {path}:{number}")
            for pattern in values:
                if not re.fullmatch(r"[A-Za-z0-9_./*?\[\]-]+", pattern):
                    raise Refusal(f"Unsupported Include at {path}:{number}")
                full = Path(pattern) if pattern.startswith("/") else h.ssh_include_base / pattern
                if any(part in (".", "..") for part in str(full).split("/")) or "//" in str(full):
                    raise Refusal(f"Noncanonical Include at {path}:{number}")
                # Prevent a wildcard/top Include from activating the new Match
                # before following global directives, even when it is absent now.
                if fnmatch.fnmatchcase(str(snippet), str(full)):
                    raise Refusal("Managed snippet is already included before EOF; manual integration required.")
                for child in sorted(glob.glob(str(full))):
                    yield from config_lines(Path(child), h, snippet, seen=seen)
        else:
            yield name, values, f"{path}:{number}"
    seen.remove(path)


def existing_keys(o, git, templates):
    templates = templates or [".ssh/authorized_keys", ".ssh/authorized_keys2"]
    for template in templates:
        if template.lower() == "none":
            continue
        value = template.replace("%%", "\0").replace("%u", o.git_user).replace("%h", git.pw_dir)
        value = value.replace("\0", "%")
        if "%" in value:
            raise Refusal("Unsupported AuthorizedKeysFile token; manual conflict check required.")
        path = Path(value)
        if not path.is_absolute():
            path = Path(git.pw_dir) / path
        try:
            info = path.lstat()
        except FileNotFoundError:
            continue
        if not stat.S_ISREG(info.st_mode) or info.st_size:
            raise Refusal(f"Existing Git authorized-keys integration at {path}. {CONFLICT}")


def socket_directory(o, service):
    root_path(o.socket.parent.parent, directory=True, public=True)
    try:
        info = o.socket.parent.lstat()
    except FileNotFoundError:
        return False
    if (not stat.S_ISDIR(info.st_mode) or info.st_uid != service.pw_uid
            or stat.S_IMODE(info.st_mode) != 0o700):
        raise Refusal(f"Existing socket directory must belong to {o.service_user} with mode 0700; no chmod: {o.socket.parent}")
    try:
        sock = o.socket.lstat()
    except FileNotFoundError:
        return True
    if not stat.S_ISSOCK(sock.st_mode) or sock.st_uid != service.pw_uid or sock.st_mode & 0o077:
        raise Refusal(f"Unsafe existing broker socket: {o.socket}")
    return True


def preflight(o, h):
    validate_options(o)
    if h.nixos.exists() or not h.systemd.is_dir():
        raise Refusal("NixOS/declarative or non-systemd host: use the printed manual snippets, not --apply.")
    service, git = accounts(o)
    root_path(o.binary, executable=True)
    root_path(o.sshd, executable=True)
    root_path(o.sshd_config)
    root_path(h.sudoers)
    root_path(h.sudoers_dir, directory=True)
    for directory in {h.app_dir, o.helper_dir}:
        root_path(directory, missing=True, directory=True, public=True)
    for tool, setuid in ((h.sudo, True), (h.python, False), (h.visudo, False), (h.systemctl, False)):
        tool_path(tool, setuid=setuid)
    if not re.search(r"(?m)^\s*(?:@includedir|#includedir)\s+" + re.escape(str(h.sudoers_dir)) + r"\s*(?:#.*)?$",
                     h.sudoers.read_text()):
        raise Refusal(f"sudoers must already include {h.sudoers_dir}; refusing to alter global sudo policy.")
    main_state = snapshot(o.sshd_config)
    main = main_state[0]
    original = unmanaged_main(main, h)
    key_files = []
    for name, values, location in config_lines(o.sshd_config, h, h.app_dir / "sshd.conf", original):
        if name == "match":
            raise Refusal(f"Prior Match block at {location}; manual integration required. {CONFLICT}")
        if name in ("allowusers", "allowgroups", "denyusers", "denygroups"):
            raise Refusal(f"Existing {name} at {location}; refusing to weaken host access policy.")
        if name in ("authorizedkeyscommand", "forcecommand", "authorizedprincipalscommand",
                    "authorizedprincipalsfile", "trustedusercakeys") and values != ["none"]:
            raise Refusal(f"Existing {name} at {location}. {CONFLICT}")
        if name == "permituserenvironment" and values != ["no"]:
            raise Refusal(f"Permissive PermitUserEnvironment at {location}; refusing to change global environment policy.")
        if name == "authorizedkeysfile" and not key_files:
            key_files = values
    existing_keys(o, git, key_files)
    desired = render(o, h)
    for path, (content, mode) in desired.items():
        # The managed directory can be absent; its already-checked parent is safe.
        if not path.parent.exists():
            continue
        info = root_path(path, missing=True)
        if info is not None and (path.read_bytes() != content or stat.S_IMODE(info.st_mode) != mode
                                 or info.st_gid != ROOT_GID):
            raise Refusal(f"Managed-name collision or changed installation: {path}; no overwrite.")
    backup = Path(str(o.sshd_config) + ".codeyard.bak")
    info = root_path(backup, missing=True)
    if info is not None and (backup.read_bytes() != original or stat.S_IMODE(info.st_mode) != 0o600):
        raise Refusal(f"Unrelated/changed backup exists: {backup}; refusing to overwrite.")
    if main != original and info is None:
        raise Refusal(f"Managed main config has no verified backup: {backup}; manual recovery required.")
    if (h.app_dir / "openssh.json").exists() and (main == original or any(not path.exists() for path in desired)):
        raise Refusal("Existing manifest describes an incomplete installation; manual recovery required.")
    socket_directory(o, service)
    # All installation outputs must be distinct from input/tool paths and socket.
    inputs = {o.binary, o.sshd, o.sshd_config, backup, o.socket, h.sudoers,
              h.sudo, h.python, h.visudo, h.systemctl}
    if len(desired) != 5 or inputs.intersection(desired):
        raise Refusal("Installation paths collide with inputs.")
    return service, desired, main_state, original, backup


def run(args, *, missing_unit=False):
    try:
        result = subprocess.run([str(a) for a in args], stdin=subprocess.DEVNULL,
            stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True,
            timeout=30, check=False, cwd="/", env={"PATH": "/usr/sbin:/usr/bin", "LC_ALL": "C"})
    except (OSError, subprocess.TimeoutExpired) as error:
        raise Refusal(f"Command failed: {args[0]}: {error}") from error
    if result.returncode and not (missing_unit and "LoadState=not-found" in result.stdout.splitlines()):
        raise Refusal(f"Command failed ({result.returncode}): {shlex.join([str(a) for a in args])}\n{result.stderr.strip()}")
    return result.stdout


def active_service(o, h):
    active = []
    for unit in ("ssh.service", "sshd.service"):
        output = run([h.systemctl, "show", unit,
                      "--property=LoadState,ActiveState,CanReload,ExecStart,Id,NeedDaemonReload,MainPID"], missing_unit=True)
        values = dict(line.split("=", 1) for line in output.splitlines() if "=" in line)
        if values.get("ActiveState") != "active":
            continue
        # Alias unit names can describe the same daemon. Never pick arbitrary units.
        identity = values.get("Id")
        if identity not in ("ssh.service", "sshd.service"):
            raise Refusal(f"Unexpected SSH service identity: {identity!r}")
        if values.get("CanReload") != "yes" or values.get("NeedDaemonReload") != "no":
            raise Refusal(f"{identity} is not safely reloadable; manual integration required.")
        command = re.fullmatch(r"\{ path=([^ ;]+) ; argv\[\]=([^;]+) ;[^{}]*\}", values.get("ExecStart", ""))
        if not command or command[1] != str(o.sshd):
            raise Refusal("Unsupported systemd SSH ExecStart; must directly execute the selected sshd.")
        pid = values.get("MainPID", "")
        if not re.fullmatch(r"[1-9][0-9]*", pid):
            raise Refusal("Active SSH service has no live MainPID.")
        process = h.proc / pid
        if os.readlink(process / "exe") != str(o.sshd):
            raise Refusal("Active SSH executable differs from the installed binary; reload manually first.")
        words = shlex.split(command[2].strip())
        if not words or words.pop(0) != str(o.sshd):
            raise Refusal("Unsupported systemd SSH command.")
        if "$SSHD_OPTS" in words:
            # Common Debian/Ubuntu units use this one variable. Read only its
            # value from the running daemon, never source EnvironmentFile or
            # expose any other process environment in logs/helpers.
            options = [item[len(b"SSHD_OPTS="):] for item in (process / "environ").read_bytes().split(b"\0")
                       if item.startswith(b"SSHD_OPTS=")]
            if len(options) != 1:
                raise Refusal("Cannot verify running SSHD_OPTS; manual integration required.")
            value = options[0].decode("ascii")
            if not re.fullmatch(r"[A-Za-z0-9_./\s-]*", value, re.ASCII):
                raise Refusal("Complex SSHD_OPTS requires manual integration.")
            words = [part for word in words for part in (value.split() if word == "$SSHD_OPTS" else [word])]
        config = Path("/etc/ssh/sshd_config")
        while words:
            flag = words.pop(0)
            if flag in ("-D", "-e", "-q"):
                continue
            if flag == "-f" and words:
                config = Path(words.pop(0))
            else:
                raise Refusal("SSHD command-line overrides/environment expansion need manual integration.")
        if config != o.sshd_config:
            raise Refusal("Selected sshd config differs from the active systemd service.")
        active.append(identity)
    if len(set(active)) != 1:
        raise Refusal("Need exactly one active ssh/sshd systemd service; no start/restart is authorized.")
    return active[0]


def effective(o, user):
    output = run([o.sshd, "-T", "-f", o.sshd_config, "-C",
                  f"user={user},host=localhost,addr=127.0.0.1,laddr=127.0.0.1,lport=22"])
    values = {}
    for line in output.splitlines():
        parts = line.split(None, 1)
        if parts:
            values.setdefault(parts[0].lower(), []).append(parts[1] if len(parts) == 2 else "")
    # sshd -T emits repeated keys (e.g. AcceptEnv). Preserve every value/order
    # for the administrative-policy comparison, not just the last one.
    return {key: tuple(items) for key, items in values.items()}


def validate_effective(o, h, admins):
    run([o.sshd, "-t", "-f", o.sshd_config])
    actual = effective(o, o.git_user)
    expected = {
        "authenticationmethods": "publickey", "pubkeyauthentication": "yes",
        "passwordauthentication": "no", "kbdinteractiveauthentication": "no",
        "hostbasedauthentication": "no", "authorizedkeysfile": "none",
        "authorizedkeyscommand": f"{o.helper_dir / 'ssh-keys'} %u %t %k",
        "authorizedkeyscommanduser": o.service_user, "forcecommand": "none",
        "disableforwarding": "yes", "permittty": "no", "permituserrc": "no",
        "permituserenvironment": "no",
        "authorizedprincipalscommand": "none", "authorizedprincipalsfile": "none",
        "trustedusercakeys": "none",
    }
    for key, value in expected.items():
        if actual.get(key) != (value,):
            raise Refusal(f"Git effective sshd policy differs: {key}: expected {value!r}, got {actual.get(key)!r}")
    accepted = [item for line in actual.get("acceptenv", ()) for item in line.split()]
    if accepted != ["GIT_PROTOCOL"]:
        raise Refusal("Git effective sshd policy must accept only GIT_PROTOCOL, not shell/loader variables.")
    for user, before in admins.items():
        if effective(o, user) != before:
            raise Refusal(f"Administrative SSH policy changed for {user}; refusing acceptance.")


def snapshot(path):
    try:
        info = path.lstat()
    except FileNotFoundError:
        return None
    if not stat.S_ISREG(info.st_mode):
        raise Refusal(f"Not a regular file: {path}")
    return (path.read_bytes(), stat.S_IMODE(info.st_mode), info.st_uid, info.st_gid,
            info.st_dev, info.st_ino, info.st_mtime_ns)


def directory_state(path):
    info = path.lstat()
    return (stat.S_IFMT(info.st_mode), stat.S_IMODE(info.st_mode), info.st_uid,
            info.st_gid, info.st_dev, info.st_ino)


def atomic_file(path, content, mode, expected, gid=None):
    if gid is None:
        gid = ROOT_GID
    if snapshot(path) != expected:
        raise Refusal(f"Concurrent change to {path}; no overwrite.")
    fd, name = tempfile.mkstemp(prefix=".codeyard.", dir=path.parent)
    try:
        with os.fdopen(fd, "wb") as stream:
            stream.write(content)
            os.fchmod(stream.fileno(), mode)
            os.fchown(stream.fileno(), ROOT_UID, gid)
            stream.flush()
            os.fsync(stream.fileno())
        if snapshot(path) != expected:
            raise Refusal(f"Concurrent change to {path}; no overwrite.")
        if expected is None:
            os.link(name, path)  # Atomic no-clobber creation, unlike replace().
        else:
            os.replace(name, path)
    finally:
        Path(name).unlink(missing_ok=True)


class Changes:
    """Only exact writes from this invocation may be rolled back."""
    def __init__(self):
        self.files = []
        self.dirs = []

    def directory(self, path, mode, uid=None, gid=None):
        if uid is None:
            uid = ROOT_UID
        if gid is None:
            gid = ROOT_GID
        if path.exists():
            return
        path.mkdir(mode=mode)
        self.dirs.append((path, directory_state(path)))
        os.chown(path, uid, gid)
        self.dirs[-1] = (path, directory_state(path))
        os.chmod(path, mode)
        self.dirs[-1] = (path, directory_state(path))

    def file(self, path, content, mode, gid=None, *, expected=UNSET):
        if gid is None:
            gid = ROOT_GID
        before = snapshot(path)
        if expected is not UNSET and before != expected:
            raise Refusal(f"Concurrent change to {path}; no overwrite.")
        if before is not None and before[:4] == (content, mode, ROOT_UID, gid):
            return
        if before is not None and expected is UNSET:
            raise Refusal(f"Managed-name collision during install: {path}; no overwrite.")
        atomic_file(path, content, mode, before, gid)
        self.files.append((path, before, snapshot(path)))

    def rollback(self):
        errors = []
        for path, before, after in reversed(self.files):
            try:
                if snapshot(path) != after:
                    raise Refusal(f"Concurrent change: leaving {path} untouched")
                if before is None:
                    path.unlink()
                else:
                    atomic_file(path, before[0], before[1], after, before[3])
            except (OSError, Refusal) as error:
                errors.append(str(error))
        for path, expected in reversed(self.dirs):
            try:
                if directory_state(path) != expected:
                    raise Refusal(f"Concurrent directory change: leaving {path} untouched")
                path.rmdir()  # Never remove contents, sockets or unrelated files.
            except OSError as error:
                errors.append(f"Left directory {path}: {error}")
            except Refusal as error:
                errors.append(str(error))
        return errors


def apply(o, h):
    if os.geteuid() != 0:
        raise Refusal("--apply requires root; the app never elevates or installs automatically.")
    # All installations share the root-owned SSH configuration directory.
    root_path(h.ssh_include_base, directory=True)
    lock = os.open(h.ssh_include_base, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW | os.O_CLOEXEC)
    try:
        try:
            fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
        except BlockingIOError as error:
            raise Refusal("Another OpenSSH installation is running; retry after it finishes.") from error
        return install(o, h)
    finally:
        os.close(lock)


def install(o, h):
    service, desired, main_state, original, backup = preflight(o, h)
    main = main_state[0]
    unit = active_service(o, h)
    run([o.sshd, "-t", "-f", o.sshd_config])
    admins = {user: effective(o, user) for user in o.admin_users}
    version = re.search(r"(?m)^Sudo version (\d+)\.(\d+)\.(\d+)", run([h.sudo, "-V"]))
    if not version or tuple(map(int, version.groups())) < (1, 9, 10):
        raise Refusal("sudo >= 1.9.10 is required for narrow anchored argument regexes; use manual setup.")
    run([h.visudo, "-c"])
    # A dot-prefixed temporary file is ignored by sudoers includedir. Validate
    # before installing any sudo permission; unsupported argument regexes fail.
    sudo_path = h.sudoers_dir / "codeyard"
    fd, name = tempfile.mkstemp(prefix=".codeyard.check.", dir=h.sudoers_dir)
    try:
        with os.fdopen(fd, "wb") as stream:
            stream.write(desired[sudo_path][0])
        run([h.visudo, "-c", "-f", name])
    finally:
        Path(name).unlink(missing_ok=True)
    changes = Changes()
    reload_attempted = False
    try:
        for directory in sorted({h.app_dir, o.helper_dir}, key=str):
            changes.directory(directory, 0o755)
        for path, (content, mode) in desired.items():
            if path != h.app_dir / "openssh.json":
                changes.file(path, content, mode)
        if snapshot(o.sshd_config) != main_state:
            raise Refusal("Main sshd config changed during preflight; no overwrite.")
        target = original + inclusion(h)
        if main != target:
            if snapshot(backup) is None:
                # Retain the verified pre-install backup even after rollback.
                atomic_file(backup, original, 0o600, None)
            changes.file(o.sshd_config, target, main_state[1], main_state[3], expected=main_state)
        run([h.visudo, "-c"])
        validate_effective(o, h, admins)
        # No creation/chmod of an unrelated directory; only a new leaf, after
        # full config validation. App startup must recreate /run after reboot.
        if not socket_directory(o, service):
            changes.directory(o.socket.parent, 0o700, service.pw_uid, service.pw_gid)
        if active_service(o, h) != unit:
            raise Refusal("Active SSH service changed before acceptance.")
        manifest = h.app_dir / "openssh.json"
        # Exact files without a manifest may precede an interrupted reload;
        # validating disk configuration alone does not prove daemon activation.
        if changes.files or not manifest.exists():
            reload_attempted = True
            run([h.systemctl, "reload", unit])
        changes.file(manifest, *desired[manifest])
    except BaseException as error:
        errors = changes.rollback()
        if reload_attempted:
            try:
                run([o.sshd, "-t", "-f", o.sshd_config])
                run([h.systemctl, "reload", unit])
            except Refusal as recovery:
                errors.append(f"Restored-config reload failed; retain admin session and recover manually: {recovery}")
        detail = "\n".join(errors)
        if isinstance(error, KeyboardInterrupt):
            raise Refusal("Interrupted; rolled back own exact changes.\n" + detail) from error
        raise Refusal(f"Installation rejected; rollback of own exact changes attempted: {error}\n{detail}") from error
    return unit


def print_plan(o, h, *, applied=False):
    print("Explicit --apply plan" if applied else "DRY-RUN plan (no mutation)")
    print(f"Existing accounts: Git={o.git_user}, broker={o.service_user}; installed binary={o.binary}")
    print(f"Broker socket={o.socket}; daemon SSH_SOCKET_PATH MUST match exactly.")
    print("Preserved administrative identities: " + ", ".join(o.admin_users))
    for path, (content, mode) in render(o, h).items():
        print(f"\n--- {path} (root-owned {mode:04o}; exact-content/no-clobber) ---")
        print(content.decode(), end="")
    print(f"\nAppend ONLY at EOF of {o.sshd_config} after backing up to {o.sshd_config}.codeyard.bak:")
    print(inclusion(h).decode(), end="")
    print(f"Create ONLY a missing {o.socket.parent} ({o.service_user}:0700), after validation.")
    print("Apply: visudo -c; sshd -t; sshd -T -C for Git/admins; reload ONE active ssh/sshd unit; then manifest.")
    print("Never create users, change shells/authorized keys, start/restart app/sshd, or bind port 22.")
    print("Manual/declarative hosts: install these root-owned helpers and scoped sudoers (sudo >=1.9.10),")
    print("put the Match block at the END of generated sshd config, validate effective policy for Git/admins,")
    print("and publish the manifest only after successful validation/reload. Do not edit Nix store symlinks.")


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--apply", action="store_true", help="Authorize the printed files, validation and sshd reload")
    parser.add_argument("--binary", default="/usr/local/bin/codeyard")
    parser.add_argument("--service-user", default="gitserv")
    parser.add_argument("--git-user", default="git")
    parser.add_argument("--socket", default="/run/codeyard/ssh.sock")
    parser.add_argument("--helper-dir", default="/etc/codeyard")
    parser.add_argument("--sshd-config", default="/etc/ssh/sshd_config")
    parser.add_argument("--sshd", default="/usr/sbin/sshd")
    parser.add_argument("--admin-user", action="append", default=[], help="Additional existing admin identity to preserve (repeatable)")
    args = parser.parse_args(argv)
    try:
        admins = ["root", *args.admin_user]
        if os.environ.get("SUDO_USER"):
            admins.append(os.environ["SUDO_USER"])
        o = Options(path_value(args.binary), args.service_user, args.git_user, path_value(args.socket),
                    path_value(args.helper_dir), path_value(args.sshd_config), path_value(args.sshd), tuple(dict.fromkeys(admins)))
        validate_options(o)
        h = Host()
        if args.apply:
            print("EXPLICIT --apply: host files/config validation/reload are authorized.")
        print_plan(o, h, applied=args.apply)
        if args.apply:
            unit = apply(o, h)
            print(f"Installed and validated; {unit} reloaded only if needed. Application unchanged.")
        else:
            preflight(o, h)
            print("Static preflight passed. No subprocess, file mutation, validation command or reload was performed.")
        return 0
    except (Refusal, OSError, UnicodeError) as error:
        print(f"REFUSED: {error}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    sys.exit(main())
