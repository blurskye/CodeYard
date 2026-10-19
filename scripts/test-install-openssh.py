#!/usr/bin/env python3
"""Disposable filesystem + mocked subprocess fixtures; never touch host SSH/sudo."""
import contextlib
import dataclasses
import importlib.util
import io
import json
import os
from pathlib import Path
import stat
import subprocess
import sys
import tempfile
from types import SimpleNamespace
import unittest
from unittest import mock


SPEC = importlib.util.spec_from_file_location("install_openssh", Path(__file__).with_name("install-openssh.py"))
installer = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = installer
SPEC.loader.exec_module(installer)
REAL_ACCOUNTS = installer.accounts


class Fixture(unittest.TestCase):
    def setUp(self):
        self.stack = contextlib.ExitStack()
        self.addCleanup(self.stack.close)
        self.root = Path(self.stack.enter_context(tempfile.TemporaryDirectory()))
        # The builder forbids setting setid bits. Model those metadata bits only;
        # fixture executables are never run, and policy still checks every mode.
        setid = {}
        original_chmod = Path.chmod
        def fixture_chmod(path, mode, **kwargs):
            setid[path] = mode & 0o6000
            return original_chmod(path, mode & ~0o6000, **kwargs)
        self.stack.enter_context(mock.patch.object(Path, "chmod", fixture_chmod))
        self.calls = []
        self.fail_syntax = False
        self.fail_reload = False
        self.admin_fault = False
        self.git_fault = False
        self.on_reload_failure = None
        self.reload_count = 0
        self.sudo_version = "1.9.15p5"
        self.fail_visudo = False
        self.h = installer.Host(
            app_dir=self.root / "etc/codeyard", sudoers=self.root / "etc/sudoers",
            sudoers_dir=self.root / "etc/sudoers.d", sudo=self.root / "usr/bin/sudo",
            visudo=self.root / "usr/sbin/visudo", systemctl=self.root / "usr/bin/systemctl",
            python=self.root / "usr/bin/python3", nixos=self.root / "etc/NIXOS",
            systemd=self.root / "systemd/system", ssh_include_base=self.root / "etc/ssh",
            proc=self.root / "proc")
        self.o = installer.Options(binary=self.root / "usr/local/bin/codeyard",
            socket=self.root / "run/codeyard/ssh.sock", helper_dir=self.h.app_dir,
            sshd_config=self.root / "etc/ssh/sshd_config", sshd=self.root / "usr/sbin/sshd",
            admin_users=("root", "alice"))
        for path in (self.h.sudoers_dir, self.h.systemd, self.o.socket.parent.parent,
                     self.root / "home/git", self.root / "proc/77"):
            path.mkdir(parents=True, exist_ok=True)
        for path in (self.o.binary, self.o.sshd, self.h.visudo, self.h.systemctl,
                     self.h.python, self.root / "usr/bin/sh"):
            self.put(path, b"fixture executable; never executed\n", 0o755)
        self.put(self.h.sudo, b"fixture sudo; never executed\n", 0o4755)
        self.put(self.h.sudoers, f"@includedir {self.h.sudoers_dir}\n".encode(), 0o600)
        self.original = b"# admin policy remains unchanged\nPasswordAuthentication yes\nPermitRootLogin prohibit-password\n"
        self.put(self.o.sshd_config, self.original, 0o600)
        (self.h.proc / "77/exe").symlink_to(self.o.sshd)
        self.put(self.h.proc / "77/environ", b"SSHD_OPTS=\0DATABASE_URL=must-not-leak\0", 0o600)
        self.root.chmod(0o755)
        for path in self.root.rglob("*"):
            if path.is_dir() and not path.is_symlink():
                path.chmod(0o755)
        self.exec_args = f"{self.o.sshd} -D -f {self.o.sshd_config}"
        self.service = SimpleNamespace(pw_uid=os.getuid(), pw_gid=os.getgid(), pw_dir=str(self.root / "home/service"))
        self.git = SimpleNamespace(pw_uid=12345, pw_gid=12345, pw_dir=str(self.root / "home/git"),
                                   pw_shell=str(self.root / "usr/bin/sh"))
        # An unprivileged test can emulate root ownership inside its own fixture.
        # Only ancestors ABOVE the fixture are masked; every fixture component,
        # permission, symlink, content and atomic operation is checked normally.
        original_lstat = Path.lstat
        ancestors = set(self.root.parents)
        def fixture_lstat(path, *args, **kwargs):
            info = original_lstat(path, *args, **kwargs)
            if setid.get(path):
                values = list(info)
                values[0] |= setid[path]
                info = os.stat_result(values)
            if path in ancestors:
                values = list(info)
                values[0] = (values[0] & ~0o022) | 0o111
                values[4], values[5] = os.getuid(), os.getgid()
                return os.stat_result(values)
            return info
        self.stack.enter_context(mock.patch.object(Path, "lstat", fixture_lstat))
        self.stack.enter_context(mock.patch.object(installer, "ROOT_UID", os.getuid()))
        self.stack.enter_context(mock.patch.object(installer, "ROOT_GID", os.getgid()))
        self.stack.enter_context(mock.patch.object(installer.os, "geteuid", return_value=0))
        self.stack.enter_context(mock.patch.object(installer, "accounts", return_value=(self.service, self.git)))
        self.process = self.stack.enter_context(mock.patch.object(installer.subprocess, "run", side_effect=self.fake_process))

    def put(self, path, content, mode=0o644):
        missing = []
        parent = path.parent
        while not parent.exists():
            missing.append(parent)
            parent = parent.parent
        path.parent.mkdir(parents=True, exist_ok=True)
        for parent in missing:
            parent.chmod(0o755)
        path.write_bytes(content)
        path.chmod(mode)

    def tree(self):
        return {str(path.relative_to(self.root)): (path.lstat().st_mode,
                path.lstat().st_mtime_ns if not path.is_dir() else None,
                os.readlink(path) if path.is_symlink() else path.read_bytes() if path.is_file() else None)
                for path in self.root.rglob("*")}

    def policy(self, user, installed):
        values = dict(authenticationmethods="any", pubkeyauthentication="yes", passwordauthentication="yes",
            kbdinteractiveauthentication="yes", hostbasedauthentication="no", authorizedkeysfile=".ssh/authorized_keys .ssh/authorized_keys2",
            authorizedkeyscommand="none", authorizedkeyscommanduser="none", forcecommand="none",
            disableforwarding="no", permittty="yes", permituserrc="yes", permituserenvironment="no",
            authorizedprincipalscommand="none", authorizedprincipalsfile="none", trustedusercakeys="none",
            permitrootlogin="without-password", port="22", acceptenv="LANG LC_*")
        if user == self.o.git_user and installed:
            values.update(authenticationmethods="publickey", passwordauthentication="no", kbdinteractiveauthentication="no",
                authorizedkeysfile="none", authorizedkeyscommand=f"{self.o.helper_dir / 'ssh-keys'} %u %t %k",
                authorizedkeyscommanduser=self.o.service_user, disableforwarding="yes", permittty="no", permituserrc="no",
                acceptenv="GIT_PROTOCOL")
            if self.git_fault:
                values["disableforwarding"] = "no"
            if getattr(self, "env_fault", False):
                values["acceptenv"] = "GIT_PROTOCOL LD_*"
        if user in self.o.admin_users and installed and self.admin_fault:
            values["passwordauthentication"] = "no"
        return "".join(f"{key} {value}\n" for key, value in values.items())

    def fake_process(self, args, **kwargs):
        self.calls.append(args)
        self.assertNotIn("shell", kwargs)
        self.assertEqual(kwargs["cwd"], "/")
        self.assertEqual(kwargs["env"], {"PATH": "/usr/sbin:/usr/bin", "LC_ALL": "C"})
        installed = installer.BEGIN.encode() in self.o.sshd_config.read_bytes()
        code, output, error = 0, "", ""
        if args[0] == str(self.h.systemctl):
            if args[1] == "show":
                state = "active" if args[2] == "ssh.service" else "inactive"
                output = (f"ActiveState={state}\nCanReload=yes\nId={args[2]}\nNeedDaemonReload=no\nMainPID=77\n"
                          f"ExecStart={{ path={self.o.sshd} ; argv[]={self.exec_args} ; ignore_errors=no ; }}\n")
            elif args[1] == "reload":
                self.reload_count += 1
                if self.reload_count == 1:
                    self.assertFalse((self.h.app_dir / "openssh.json").exists())
                if self.fail_reload and self.reload_count == 1:
                    if self.on_reload_failure:
                        self.on_reload_failure()
                    code, error = 1, "fixture reload failure"
            else:
                self.fail(f"Unexpected systemctl call: {args}")
        elif args[0] == str(self.o.sshd):
            if args[1] == "-T":
                user = args[-1].split(",", 1)[0].removeprefix("user=")
                output = self.policy(user, installed)
            elif args[1] == "-t":
                if self.fail_syntax and installed:
                    code, error = 1, "fixture sshd validation failure"
            else:
                self.fail(f"Unexpected sshd call: {args}")
        elif args[0] == str(self.h.sudo):
            self.assertEqual(args[1:], ["-V"])
            output = f"Sudo version {self.sudo_version}\n"
        elif args[0] == str(self.h.visudo):
            if "-f" in args:
                self.assertTrue(Path(args[-1]).name.startswith(".codeyard.check."))
                self.assertIn(b"^ssh serve", Path(args[-1]).read_bytes())
                if self.fail_visudo:
                    code, error = 1, "fixture visudo refusal"
        else:
            self.fail(f"Unexpected subprocess: {args}")
        return subprocess.CompletedProcess(args, code, output, error)

    def cli(self, *extra):
        args = ["--binary", str(self.o.binary), "--socket", str(self.o.socket),
                "--helper-dir", str(self.o.helper_dir), "--sshd-config", str(self.o.sshd_config),
                "--sshd", str(self.o.sshd), "--admin-user", "alice", *extra]
        output, error = io.StringIO(), io.StringIO()
        with mock.patch.object(installer, "Host", return_value=self.h), \
                mock.patch.dict(os.environ, {"SUDO_USER": ""}), \
                contextlib.redirect_stdout(output), contextlib.redirect_stderr(error):
            result = installer.main(args)
        return result, output.getvalue(), error.getvalue()

    def test_dry_run_is_read_only_and_does_not_execute_commands(self):
        before = self.tree()
        result, output, error = self.cli()
        self.assertEqual(result, 0, error)
        self.assertIn("DRY-RUN plan (no mutation)", output)
        self.assertIn("SSH_SOCKET_PATH MUST match", output)
        self.assertNotIn("DATABASE_URL", output)
        self.assertEqual(self.tree(), before)
        self.process.assert_not_called()

    def test_hostile_names_paths_and_aliases_are_rejected(self):
        for field, value in (("git_user", "git\nMatch all"), ("git_user", "-root"), ("service_user", "gît"),
                ("binary", Path("/tmp/x;id")), ("binary", Path("/tmp/a b")), ("binary", Path("/tmp/a%u")),
                ("socket", Path("relative.sock")), ("socket", Path("/run/a/../ssh.sock")),
                ("helper_dir", Path("/etc/foo\nInclude")), ("socket", Path("/run/" + "x" * 110))):
            with self.subTest(field=field, value=value), self.assertRaises(installer.Refusal):
                installer.validate_options(dataclasses.replace(self.o, **{field: value}))
        with self.assertRaises(installer.Refusal):
            installer.validate_options(dataclasses.replace(self.o, service_user="git"))
        with self.assertRaises(installer.Refusal):
            installer.validate_options(dataclasses.replace(self.o, admin_users=("git",)))

    def test_existing_integrations_and_access_lists_are_not_replaced(self):
        for text in ("Match User git\n ForceCommand /usr/bin/forgejo serv\n", "AllowUsers root git\n",
                "AllowGroups admins\n", "DenyUsers git\n", "DenyGroups git\n",
                "AuthorizedKeysCommand /usr/bin/forgejo keys %u %t %k\n", "ForceCommand /usr/bin/forgejo serv\n",
                "TrustedUserCAKeys /etc/ssh/ca.pub\n", "AuthorizedPrincipalsCommand /usr/local/bin/lookup\n",
                "PermitUserEnvironment yes\n"):
            with self.subTest(text=text):
                self.o.sshd_config.write_text(text)
                with self.assertRaises(installer.Refusal):
                    installer.preflight(self.o, self.h)
                self.assertEqual(self.o.sshd_config.read_text(), text)
        self.process.assert_not_called()

    def test_existing_authorized_keys_require_separate_username(self):
        path = Path(self.git.pw_dir) / ".ssh/authorized_keys"
        self.put(path, b'command="forgejo serv key-1" ssh-ed25519 fixture\n')
        with self.assertRaisesRegex(installer.Refusal, "separate Git username"):
            installer.preflight(self.o, self.h)
        path.unlink()
        custom = self.root / "etc/ssh/git.keys"
        self.put(custom, b"existing integration\n")
        self.o.sshd_config.write_text(f"AuthorizedKeysFile {custom}\n")
        with self.assertRaises(installer.Refusal):
            installer.preflight(self.o, self.h)

    def test_include_is_at_eof_and_prior_wildcard_is_refused(self):
        desired = installer.render(self.o, self.h)
        snippet = desired[self.h.app_dir / "sshd.conf"][0]
        self.assertTrue(snippet.startswith(installer.MARKER.encode()))
        self.assertIn(b"Match User git\n", snippet)
        self.assertIn(b"AuthorizedKeysFile none", snippet)
        self.assertTrue(snippet.endswith(b"Match all\n"))
        suffix = installer.inclusion(self.h)
        self.assertEqual(installer.unmanaged_main(self.original + suffix, self.h), self.original)
        with self.assertRaises(installer.Refusal):
            installer.unmanaged_main(self.original + suffix + b"Port 22\n", self.h)
        self.o.sshd_config.write_text(f"Include {self.h.app_dir}/*.conf\nPort 22\n")
        with self.assertRaisesRegex(installer.Refusal, "before EOF"):
            installer.preflight(self.o, self.h)

    def test_symlinks_writable_parents_and_binary_modes_are_refused(self):
        target = self.root / "usr/bin/other"
        self.put(target, b"not the approved binary\n", 0o755)
        self.o.binary.unlink()
        self.o.binary.symlink_to(target)
        with self.assertRaisesRegex(installer.Refusal, "symlink"):
            installer.preflight(self.o, self.h)
        self.o.binary.unlink()
        self.put(self.o.binary, b"fixture\n", 0o755)
        for mode in (0o777, 0o644, 0o4755):
            with self.subTest(mode=mode):
                self.o.binary.chmod(mode)
                with self.assertRaises(installer.Refusal):
                    installer.preflight(self.o, self.h)
        self.o.binary.chmod(0o755)
        self.o.binary.parent.chmod(0o777)
        with self.assertRaises(installer.Refusal):
            installer.preflight(self.o, self.h)
        self.o.binary.parent.chmod(0o755)
        self.h.app_dir.symlink_to(self.root / "etc/ssh", target_is_directory=True)
        with self.assertRaises(installer.Refusal):
            installer.preflight(self.o, self.h)

    def test_wrong_owner_and_managed_file_collision_are_refused(self):
        lstat = Path.lstat
        def wrong_owner(path, *args, **kwargs):
            info = lstat(path, *args, **kwargs)
            if path == self.o.binary:
                values = list(info)
                values[4] = installer.ROOT_UID + 1
                return os.stat_result(values)
            return info
        with mock.patch.object(Path, "lstat", wrong_owner), self.assertRaises(installer.Refusal):
            installer.preflight(self.o, self.h)
        self.put(self.h.app_dir / "ssh-serve", b"unrelated owner content\n", 0o755)
        with self.assertRaisesRegex(installer.Refusal, "collision"):
            installer.preflight(self.o, self.h)
        self.assertEqual((self.h.app_dir / "ssh-serve").read_bytes(), b"unrelated owner content\n")

    def test_socket_directory_never_chmods_or_follows_unrelated_path(self):
        self.o.socket.parent.mkdir(mode=0o755)
        self.o.socket.parent.chmod(0o755)  # Make the unsafe fixture independent of the umask.
        self.assertEqual(stat.S_IMODE(self.o.socket.parent.stat().st_mode), 0o755)
        before = self.o.socket.parent.stat().st_mode
        with self.assertRaisesRegex(installer.Refusal, "no chmod"):
            installer.preflight(self.o, self.h)
        self.assertEqual(self.o.socket.parent.stat().st_mode, before)
        self.o.socket.parent.rmdir()
        self.o.socket.parent.symlink_to(self.root / "run", target_is_directory=True)
        with self.assertRaises(installer.Refusal):
            installer.preflight(self.o, self.h)

    def test_accounts_are_existing_distinct_nonroot_with_working_git_shell(self):
        service = SimpleNamespace(pw_uid=1001)
        git = SimpleNamespace(pw_uid=1002, pw_shell=str(self.root / "usr/bin/sh"))
        def lookup(name):
            return service if name == "gitserv" else git if name == "git" else SimpleNamespace(pw_uid=0 if name == "root" else 1003)
        with mock.patch.object(installer.pwd, "getpwnam", side_effect=lookup):
            self.assertEqual(REAL_ACCOUNTS(self.o), (service, git))
            service.pw_uid = 0
            with self.assertRaises(installer.Refusal):
                REAL_ACCOUNTS(self.o)
            service.pw_uid = 1001
            git.pw_shell = "/usr/sbin/nologin"
            with self.assertRaises(installer.Refusal):
                REAL_ACCOUNTS(self.o)
        with mock.patch.object(installer.pwd, "getpwnam", side_effect=KeyError("missing")), self.assertRaises(installer.Refusal):
            REAL_ACCOUNTS(self.o)

    def test_declarative_or_non_systemd_prints_manual_plan_without_mutation(self):
        self.put(self.h.nixos, b"declarative\n")
        before = self.tree()
        result, output, error = self.cli()
        self.assertEqual(result, 1)
        self.assertIn("Manual/declarative hosts", output)
        self.assertIn("NixOS/declarative", error)
        self.assertEqual(self.tree(), before)
        self.process.assert_not_called()

    def test_old_sudo_or_invalid_sudoers_fails_before_installation(self):
        before = self.tree()
        self.sudo_version = "1.9.9"
        with self.assertRaisesRegex(installer.Refusal, "1.9.10"):
            installer.apply(self.o, self.h)
        self.assertEqual(self.tree(), before)
        self.sudo_version = "1.9.15"
        self.fail_visudo = True
        with self.assertRaisesRegex(installer.Refusal, "visudo refusal"):
            installer.apply(self.o, self.h)
        self.assertEqual(self.tree(), before)
        self.assertEqual(self.reload_count, 0)

    def test_root_safe_system_tool_symlink_but_not_writable_target(self):
        target = self.root / "usr/bin/python-version"
        self.put(target, b"fixture interpreter\n", 0o755)
        self.h.python.unlink()
        self.h.python.symlink_to(target.name)
        installer.preflight(self.o, self.h)
        target.chmod(0o777)
        with self.assertRaises(installer.Refusal):
            installer.preflight(self.o, self.h)

    def test_apply_holds_the_installation_lock_through_preflight_and_rollback(self):
        def assert_locked():
            descriptor = os.open(self.h.ssh_include_base, os.O_RDONLY | os.O_DIRECTORY)
            try:
                with self.assertRaises(BlockingIOError):
                    installer.fcntl.flock(descriptor, installer.fcntl.LOCK_EX | installer.fcntl.LOCK_NB)
            finally:
                os.close(descriptor)
        preflight = installer.preflight
        def checked_preflight(*args):
            assert_locked()
            return preflight(*args)
        self.fail_reload = True
        self.on_reload_failure = assert_locked
        with mock.patch.object(installer, "preflight", side_effect=checked_preflight):
            with self.assertRaises(installer.Refusal):
                installer.apply(self.o, self.h)
        # The descriptor closes on failure, so the next installation can proceed.
        self.fail_reload = False
        installer.apply(self.o, self.h)

    def test_competing_apply_is_refused_before_preflight(self):
        descriptor = os.open(self.h.ssh_include_base, os.O_RDONLY | os.O_DIRECTORY)
        try:
            installer.fcntl.flock(descriptor, installer.fcntl.LOCK_EX)
            with self.assertRaisesRegex(installer.Refusal, "Another OpenSSH installation"):
                installer.apply(self.o, self.h)
            self.assertEqual(self.calls, [])
        finally:
            os.close(descriptor)

    def test_apply_and_repeat_are_idempotent_with_manifest_written_last(self):
        unit = installer.apply(self.o, self.h)
        self.assertEqual(unit, "ssh.service")
        self.assertEqual(self.reload_count, 1)
        self.assertEqual(self.o.sshd_config.read_bytes(), self.original + installer.inclusion(self.h))
        backup = Path(str(self.o.sshd_config) + ".codeyard.bak")
        self.assertEqual(backup.read_bytes(), self.original)
        self.assertEqual(stat.S_IMODE(backup.stat().st_mode), 0o600)
        for path, (content, mode) in installer.render(self.o, self.h).items():
            self.assertEqual(path.read_bytes(), content)
            self.assertEqual(stat.S_IMODE(path.stat().st_mode), mode)
        manifest = json.loads((self.h.app_dir / "openssh.json").read_text())
        self.assertEqual(manifest, dict(schema=1, git_user="git", service_user="gitserv", socket=str(self.o.socket),
                         serve_command=str(self.o.helper_dir / "ssh-serve"), binary=str(self.o.binary)))
        self.assertEqual(stat.S_IMODE(self.o.socket.parent.stat().st_mode), 0o700)
        before = self.tree()
        installer.apply(self.o, self.h)
        self.assertEqual(self.reload_count, 1)
        self.assertEqual(self.tree(), before)
        self.assertFalse(any(".codeyard.check." in path for path in self.tree()))

    def test_validation_failure_rolls_back_without_reload_or_socket_creation(self):
        for field in ("fail_syntax", "admin_fault", "git_fault", "env_fault"):
            with self.subTest(field=field):
                setattr(self, field, True)
                with self.assertRaises(installer.Refusal):
                    installer.apply(self.o, self.h)
                self.assertEqual(self.o.sshd_config.read_bytes(), self.original)
                self.assertFalse(self.h.app_dir.exists())
                self.assertFalse((self.h.sudoers_dir / "codeyard").exists())
                self.assertFalse(self.o.socket.parent.exists())
                self.assertEqual(self.reload_count, 0)
                setattr(self, field, False)
        self.assertTrue(Path(str(self.o.sshd_config) + ".codeyard.bak").exists())

    def test_reload_failure_restores_exact_files_and_reloads_restored_config(self):
        self.fail_reload = True
        with self.assertRaisesRegex(installer.Refusal, "reload failure"):
            installer.apply(self.o, self.h)
        self.assertEqual(self.reload_count, 2)
        self.assertEqual(self.o.sshd_config.read_bytes(), self.original)
        self.assertFalse(self.h.app_dir.exists())
        self.assertFalse(self.o.socket.parent.exists())
        self.assertFalse((self.h.sudoers_dir / "codeyard").exists())

    def test_rollback_does_not_remove_concurrently_changed_owned_file(self):
        self.fail_reload = True
        path = self.h.app_dir / "ssh-serve"
        self.on_reload_failure = lambda: path.write_bytes(b"concurrent administrator edit\n")
        with self.assertRaisesRegex(installer.Refusal, "leaving.*untouched"):
            installer.apply(self.o, self.h)
        self.assertEqual(path.read_bytes(), b"concurrent administrator edit\n")
        self.assertEqual(self.o.sshd_config.read_bytes(), self.original)
        self.assertFalse((self.h.app_dir / "openssh.json").exists())

    def test_rollback_keeps_a_concurrently_changed_directory(self):
        self.fail_reload = True
        self.on_reload_failure = lambda: self.o.socket.parent.chmod(0o701)
        with self.assertRaisesRegex(installer.Refusal, "Concurrent directory change"):
            installer.apply(self.o, self.h)
        self.assertTrue(self.o.socket.parent.is_dir())
        self.assertEqual(stat.S_IMODE(self.o.socket.parent.stat().st_mode), 0o701)
        self.assertEqual(self.o.sshd_config.read_bytes(), self.original)

    def test_incomplete_existing_manifest_is_not_accepted(self):
        content, mode = installer.render(self.o, self.h)[self.h.app_dir / "openssh.json"]
        self.put(self.h.app_dir / "openssh.json", content, mode)
        with self.assertRaisesRegex(installer.Refusal, "incomplete installation"):
            installer.preflight(self.o, self.h)

    def test_effective_policy_keeps_all_repeated_directives(self):
        with mock.patch.object(installer, "run", return_value="AcceptEnv LANG\nacceptenv LC_*\nPasswordAuthentication yes\n"):
            actual = installer.effective(self.o, "root")
        self.assertEqual(actual["acceptenv"], ("LANG", "LC_*"))
        self.assertEqual(actual["passwordauthentication"], ("yes",))

    def test_safe_service_selection_and_debian_empty_sshd_opts(self):
        self.exec_args += " $SSHD_OPTS"
        self.assertEqual(installer.active_service(self.o, self.h), "ssh.service")
        self.put(self.h.proc / "77/environ", b"UNRELATED=value\0", 0o600)
        with self.assertRaisesRegex(installer.Refusal, "Cannot verify"):
            installer.active_service(self.o, self.h)
        self.put(self.h.proc / "77/environ", b"SSHD_OPTS=-o AuthorizedKeysCommand=/evil\0", 0o600)
        with self.assertRaises(installer.Refusal):
            installer.active_service(self.o, self.h)
        self.exec_args = "/bin/sh -c arbitrary"
        with self.assertRaises(installer.Refusal):
            installer.active_service(self.o, self.h)

    def run_helper(self, name, argv, environment):
        text = installer.render(self.o, self.h)[self.o.helper_dir / name][0].decode()
        self.assertTrue(text.startswith(f"#!{self.h.python} -I\n"))
        class Executed(Exception):
            pass
        with mock.patch.object(sys, "argv", [name, *argv]), mock.patch.dict(os.environ, environment, clear=True), \
                mock.patch.object(os, "chdir") as chdir, mock.patch.object(os, "execve", side_effect=Executed) as execute:
            try:
                exec(compile(text, str(self.o.helper_dir / name), "exec"), {})
            except Executed:
                chdir.assert_called_once_with("/")
                return execute.call_args.args
        self.fail("helper did not execute")

    def test_serve_helper_only_preserves_two_vars_and_fixed_command(self):
        executable, args, environment = self.run_helper("ssh-serve", ["123"],
            {"SSH_ORIGINAL_COMMAND": "git-upload-pack 'owner/repo.git'", "GIT_PROTOCOL": "version=2",
             "DATABASE_URL": "secret", "PATH": "/evil", "PYTHONPATH": "/evil"})
        self.assertEqual(executable, str(self.h.sudo))
        self.assertEqual(args, [str(self.h.sudo), "-n", "-u", "gitserv",
            "--preserve-env=SSH_ORIGINAL_COMMAND,GIT_PROTOCOL", "--", str(self.o.binary),
            "ssh", "serve", "--socket", str(self.o.socket), "--key-id", "123"])
        self.assertEqual(environment, {"SSH_ORIGINAL_COMMAND": "git-upload-pack 'owner/repo.git'", "GIT_PROTOCOL": "version=2"})
        for argv in (["0"], ["-1"], ["01"], ["1", "--database-url", "evil"], ["1;id"], ["١"], ["9223372036854775808"]):
            with self.subTest(argv=argv), self.assertRaises(SystemExit):
                self.run_helper("ssh-serve", argv, {"SSH_ORIGINAL_COMMAND": "x"})

    def test_keys_helper_uses_frozen_contract_and_empty_environment(self):
        executable, args, environment = self.run_helper("ssh-keys", ["git", "ssh-ed25519", "YWJj="], {"DATABASE_URL": "secret"})
        self.assertEqual(executable, str(self.o.binary))
        self.assertEqual(args, [str(self.o.binary), "ssh", "keys", "--socket", str(self.o.socket),
            "--expected-user", "git", "--serve-command", str(self.o.helper_dir / "ssh-serve"), "git", "ssh-ed25519", "YWJj="])
        self.assertEqual(environment, {})
        for argv in (["root", "ssh-ed25519", "YWJj="], ["git", "--evil", "x;id"], ["git", "ssh-ed25519", "a", "extra"]):
            with self.subTest(argv=argv), self.assertRaises(SystemExit):
                self.run_helper("ssh-keys", argv, {})
        text = installer.render(self.o, self.h)[self.h.sudoers_dir / "codeyard"][0].decode()
        self.assertIn(" ^ssh serve --socket ", text)
        self.assertIn("--key-id [1-9][0-9]*$", text)
        self.assertIn("NOPASSWD: NOSETENV:", text)
        self.assertNotIn("SETENV: ALL", text)


if __name__ == "__main__":
    unittest.main()
