#!/usr/bin/env python3
"""Real sshd parser checks. Never starts/reloads a daemon or reads host config."""
import importlib.util
import os
from pathlib import Path
import pwd
import shutil
import subprocess
import sys
import tempfile
import unittest

spec = importlib.util.spec_from_file_location("gitserv_openssh_install", Path(__file__).with_name("install-openssh.py"))
installer = importlib.util.module_from_spec(spec)
sys.modules[spec.name] = installer
spec.loader.exec_module(installer)


def tool(name):
    value = shutil.which(name)
    if not value:
        candidate = Path("/run/current-system/sw/bin") / name
        if candidate.is_file():
            value = str(candidate)
    if not value:
        raise RuntimeError(f"Real {name} is required for the disposable parser checks")
    return value


def invoke(args):
    return subprocess.run(args, stdin=subprocess.DEVNULL, capture_output=True, text=True, timeout=15, check=False)


def fields(text):
    result = {}
    for line in text.splitlines():
        key, _, value = line.partition(" ")
        result.setdefault(key.lower(), []).append(value.strip())
    return result


class OpenSshParserTests(unittest.TestCase):
    def test_real_visudo_accepts_the_narrow_command_grant(self):
        with tempfile.TemporaryDirectory(prefix="gitserv-sudoers-config-") as temporary:
            host = installer.Host()
            options = installer.Options()
            config = Path(temporary) / "sudoers"
            config.write_bytes(installer.render(options, host)[host.sudoers_dir / "codeyard"][0])
            config.chmod(0o440)
            result = invoke([tool("visudo"), "-c", "-f", str(config)])
            self.assertEqual(result.returncode, 0, result.stderr)

    def test_real_parser_accepts_git_match_and_preserves_admin_policy(self):
        sshd, keygen = tool("sshd"), tool("ssh-keygen")
        with tempfile.TemporaryDirectory(prefix="gitserv-sshd-config-") as temporary:
            root = Path(temporary)
            key = root / "host_key"
            generated = invoke([keygen, "-q", "-t", "ed25519", "-N", "", "-f", str(key)])
            self.assertEqual(generated.returncode, 0, generated.stderr)
            # Pure renderer/parser fixture: these accounts never authenticate and no
            # installer preflight/apply is called. No port is opened, not even loopback.
            git_user = pwd.getpwuid(os.getuid()).pw_name
            self.assertNotEqual(git_user, "root", "run parser tests as an unprivileged user")
            options = installer.Options(git_user=git_user, service_user="nobody")
            host = installer.Host()
            snippet = installer.render(options, host)[host.app_dir / "sshd.conf"][0].decode()
            base = (f"HostKey {key}\nUsePAM no\nPermitUserEnvironment no\n"
                    "AcceptEnv LANG LC_*\nPasswordAuthentication yes\n"
                    "AuthorizedKeysFile .ssh/authorized_keys\n")
            before, after = root / "before.conf", root / "after.conf"
            before.write_text(base)
            after.write_text(base + snippet)

            def effective(path, user):
                result = invoke([sshd, "-T", "-f", str(path), "-C",
                                 f"user={user},host=localhost,addr=127.0.0.1,laddr=127.0.0.1,lport=2224"])
                self.assertEqual(result.returncode, 0, result.stderr)
                return fields(result.stdout)

            self.assertEqual(effective(before, "root"), effective(after, "root"))
            git = effective(after, git_user)
            for name, value in {
                "authenticationmethods": "publickey", "passwordauthentication": "no",
                "authorizedkeysfile": "none", "authorizedkeyscommanduser": "nobody",
                "disableforwarding": "yes", "permittty": "no", "permituserrc": "no",
                "permituserenvironment": "no", "acceptenv": "GIT_PROTOCOL",
            }.items():
                self.assertEqual(git[name], [value], name)
            self.assertNotIn("PermitUserEnvironment", snippet)
            bad = root / "invalid.conf"
            bad.write_text(base + f"Match User {git_user}\n    PermitUserEnvironment no\n")
            rejected = invoke([sshd, "-T", "-f", str(bad)])
            self.assertNotEqual(rejected.returncode, 0)
            self.assertIn("not allowed within a Match block", rejected.stderr)


if __name__ == "__main__":
    unittest.main()
