#!/usr/bin/env python3
"""Offline harness tests: only a temporary fake gh executable is ever launched."""
import contextlib
import importlib
import io
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import threading
import time
import unittest
from unittest.mock import patch

from gh_sweep import harness as h

FAKE = '''#!{python}
import json, os, subprocess, sys, time
args = sys.argv[1:]
if args[0] == "environment":
    print(json.dumps(dict(os.environ)))
elif args[0] == "echo":
    print(json.dumps({{"input": sys.stdin.read(), "cwd": os.getcwd()}}))
elif args[0] == "hang":
    child = subprocess.Popen([sys.executable, "-c", "import time; time.sleep(60)"])
    with open(args[1], "w") as f:
        json.dump([os.getpid(), child.pid], f)
    print("started", flush=True)
    time.sleep(60)
elif args[0] == "fail":
    print("HTTP 404: Not Found", file=sys.stderr)
    sys.exit(1)
else:
    print(json.dumps({{"number": 1, "title": "sample"}}))
'''


class SweepHarnessTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory(prefix="fake-gh-")
        self.directory = Path(self.temporary.name)
        self.fake = self.directory / "gh"
        self.fake.write_text(FAKE.format(python=sys.executable))
        self.fake.chmod(0o700)
        self.saved_cases = h.CASES[:]
        h.CASES.clear()
        self.sweep = h.Sweep(None, accounts=2, gh=self.fake)
        self.sweep.__enter__()

    def tearDown(self):
        self.sweep.__exit__(None, None, None)
        h.CASES[:] = self.saved_cases
        self.temporary.cleanup()

    def test_registry_and_fresh_names(self):
        @h.case(command="repo view", variant="json", needs=("repo",), serial_group="shared")
        def example(ctx):
            pass
        self.assertIs(h.CASES[0].function, example)
        self.assertEqual(h.CASES[0].needs, ("repo",))
        with self.assertRaises(ValueError):
            h.case(command="repo view", variant="json")(example)
        with self.assertRaises(ValueError):
            h.case(command="api", needs=("unknown",))(example)
        first = h.Ctx(self.sweep, self.sweep.accounts[0], h.CASES[0])
        second = h.Ctx(self.sweep, self.sweep.accounts[0], h.CASES[0])
        self.assertNotEqual(first.directory, second.directory)
        self.assertEqual(len({first.name("repo") for _ in range(50)}), 50)

    def test_environment_isolation_and_io(self):
        poisoned = {"GH_TOKEN": "owner-secret", "GITHUB_TOKEN": "owner-secret", "GH_HOST": "github.com",
                    "GH_ENTERPRISE_TOKEN": "owner-secret", "GITHUB_ENTERPRISE_TOKEN": "owner-secret",
                    "GIT_DIR": "/owner/repo", "XDG_CONFIG_DIRS": "/owner/config", "ALL_PROXY": "owner-proxy"}
        with patch.dict(os.environ, poisoned):
            envs = [self.sweep.gh(a, "environment").json() for a in self.sweep.accounts]
        for env in envs:
            self.assertNotIn("owner-secret", json.dumps(env))
            for key in ("GH_TOKEN", "GITHUB_TOKEN", "GH_ENTERPRISE_TOKEN", "GITHUB_ENTERPRISE_TOKEN", "GIT_DIR", "ALL_PROXY", "XDG_CONFIG_DIRS"):
                self.assertNotIn(key, env)
            for key in ("HOME", "GH_CONFIG_DIR", "XDG_CONFIG_HOME", "XDG_DATA_HOME", "XDG_CACHE_HOME", "XDG_STATE_HOME", "XDG_RUNTIME_DIR"):
                self.assertTrue(Path(env[key]).is_relative_to(self.sweep.directory), (key, env[key]))
            for key in ("GH_PROMPT_DISABLED", "NO_COLOR", "GH_NO_UPDATE_NOTIFIER"):
                self.assertEqual(env[key], "1")
            self.assertEqual(env["GH_HOST"], "localhost.invalid")
        self.assertNotEqual(envs[0]["GH_CONFIG_DIR"], envs[1]["GH_CONFIG_DIR"])
        result = self.sweep.gh(self.sweep.accounts[0], "echo", input="hello\n", cwd=self.directory)
        self.assertEqual(result.json(), {"input": "hello\n", "cwd": str(self.directory)})

    def test_assertions_and_expected_errors(self):
        result = self.sweep.gh(self.sweep.accounts[0], "fail", ok=None)
        h.expect_unsupported(result)
        with self.assertRaises(AssertionError):
            self.sweep.gh(self.sweep.accounts[0], "fail")
        h.expect_json(h.Result(0, '[{"number":1,"extra":true}]', ""), [{"number": int}])
        for result in (h.Result(0, "", "HTTP 404: Not Found"), h.Result(-11, "", "HTTP 404: Not Found"),
                       h.Result(1, "", "Not Found"), h.Result(1, "", "HTTP 404: Not Found panic:"),
                       h.Result(1, "", "HTTP 404: Not Found", True)):
            with self.assertRaises(AssertionError):
                h.expect_unsupported(result)
        with self.assertRaises(AssertionError):
            h.expect_json(h.Result(0, '{"number":"one"}', ""), {"number": int})

    def test_timeout_kills_parent_and_descendant(self):
        pid_file = self.directory / "pids.json"
        start = time.monotonic()
        with self.assertRaises(TimeoutError):
            self.sweep.gh(self.sweep.accounts[0], "hang", pid_file, timeout=0.3)
        self.assertLess(time.monotonic() - start, 3)
        pids = json.loads(pid_file.read_text())
        for pid in pids:
            # Orphan zombies may await the container's init reaper, but cannot run.
            stat = Path(f"/proc/{pid}/stat")
            deadline = time.monotonic() + 2
            while stat.exists() and stat.read_text().split()[2] != "Z" and time.monotonic() < deadline:
                time.sleep(0.01)
            self.assertTrue(not stat.exists() or stat.read_text().split()[2] == "Z", pid)
        self.assertFalse(self.sweep.processes)

    def test_coverage_tracks_real_calls_not_variants_or_fixtures(self):
        inventory = {"": {"children": ["repo", "api"], "flags": ["--help"]},
                     "repo": {"children": ["view"], "flags": []},
                     "repo view": {"children": [], "flags": ["--json", "--repo", "--web"]},
                     "api": {"children": [], "flags": ["--method"]}}
        cases = [h.Case("repo view", "--web", lambda _: None)]
        classification = {"repo view": {"kind": "covered"}}
        records = [{"case": "one", "argv": ["repo", "view", "-R", "owner/repo", "--json=name", "--help"]},
                   {"case": None, "argv": ["api", "user", "--method", "GET"]}]
        report = h.coverage_report(cases=cases, inventory=inventory, classification=classification,
                                   records=records, waivers={"version": 1, "flags": {"repo view": {"--web": "browser lane"}}})
        self.assertEqual(report["missing_cases"], ["api"])
        self.assertEqual(report["missing_classification"], ["api"])
        self.assertEqual(report["missing_flags"], [("", "--help"), ("api", "--method")])
        self.assertFalse(report["ok"])
        self.assertEqual(report["waived"], 1)
        for waiver in ({"repo view": {"--web": ""}}, {"repo view": {"--bogus": "reason"}}):
            with self.assertRaises(ValueError):
                h.coverage_report(inventory=inventory, classification=classification,
                                  waivers={"version": 1, "flags": waiver})
        records.append({"case": "one", "argv": ["api", "user", "--method", "GET"]})
        records.append({"case": "one", "argv": ["--help"]})
        report = h.coverage_report(cases=cases + [h.Case("api", "get", None)], records=records,
                                   inventory=inventory, classification={**classification, "api": {"kind": "covered"}},
                                   waivers={"version": 1, "flags": {}})
        self.assertTrue(report["ok"])
        self.assertEqual(report["missing_flags"], [("repo view", "--web")])

    def test_classification_validation(self):
        path = self.directory / "classification.json"
        for row in ({"kind": "local_only", "reason": "no server"},
                    {"kind": "interactive_only", "reason": "TTY prompt", "expected": {"rc": 1, "contains": "terminal"}},
                    {"kind": "unsupported_on_ghes", "reason": "cloud only", "expected": {"status": 404, "contains": "Not Found"}}):
            path.write_text(json.dumps({"version": 1, "commands": {"api": row}}))
            self.assertEqual(h.load_classification(path)["api"], row)
        for command, row in (("api", {"kind": "local_only"}), ("api", {"kind": "unsupported_on_ghes", "reason": "x"}),
                             ("repo", {"kind": "covered"}), ("not a command", {"kind": "covered"})):
            path.write_text(json.dumps({"version": 1, "commands": {command: row}}))
            with self.assertRaises(ValueError):
                h.load_classification(path)

    def test_runner_table_failure_redaction_and_recording(self):
        self.sweep.accounts[0].token = "private-fake-token"
        def good(ctx):
            h.expect_json(ctx.gh("repo", "view", "--json", "number"), {"number": int})
        def bad(ctx):
            raise AssertionError("private-fake-token failed")
        cases = [h.Case("repo view", "json", good), h.Case("api", "bad", bad)]
        output = io.StringIO()
        with contextlib.redirect_stdout(output):
            rc = self.sweep.run(cases, workers=2)
        self.assertEqual(rc, 1)
        self.assertIn("RESULT  SECONDS  CASE", output.getvalue())
        self.assertRegex(output.getvalue(), r"SWEEP passed=1 failed=1 skipped=0 waived=\d+")
        self.assertNotIn("private-fake-token", output.getvalue())
        self.assertEqual(self.sweep.records[-1]["argv"], ["repo", "view", "--json", "number"])
        self.assertTrue(self.sweep.records[-1]["case"].startswith("sw-"))

    def test_case_timeout_is_bounded_and_independent_case_continues(self):
        def hang(ctx):
            ctx.gh("hang", self.directory / "runner-pids.json", timeout=60)
        def pass_case(ctx):
            ctx.gh("repo", "view")
        output = io.StringIO()
        start = time.monotonic()
        with contextlib.redirect_stdout(output):
            rc = self.sweep.run([h.Case("api", "hang", hang, timeout=0.25),
                                 h.Case("repo view", "next", pass_case)], workers=1)
        self.assertEqual(rc, 1)
        self.assertLess(time.monotonic() - start, 3)
        self.assertRegex(output.getvalue(), r"SWEEP passed=1 failed=1 skipped=0 waived=\d+")

    def test_python_body_timeout_does_not_hold_runner_open(self):
        finished = threading.Event()
        def sleepy(ctx):
            finished.wait(1)
            with self.assertRaises(TimeoutError):
                ctx.gh("repo", "view")
        with contextlib.redirect_stdout(io.StringIO()):
            start = time.monotonic()
            self.assertEqual(self.sweep.run([h.Case("api", "body", sleepy, timeout=0.05)]), 1)
            self.assertLess(time.monotonic() - start, 0.8)
        finished.set()

    def test_parallelism_account_limits_and_serial_groups(self):
        lock = threading.Lock()
        active, peaks = {}, {}
        def work(ctx):
            keys = ["all", ctx.account.name]
            if ctx.definition.serial_group:
                keys.append(ctx.definition.serial_group)
            with lock:
                for key in keys:
                    active[key] = active.get(key, 0) + 1
                    peaks[key] = max(peaks.get(key, 0), active[key])
            time.sleep(0.05)
            ctx.gh("repo", "view")
            with lock:
                for key in keys:
                    active[key] -= 1
        cases = [h.Case("repo view", str(i), work, serial_group="shared" if i < 2 else None) for i in range(8)]
        with contextlib.redirect_stdout(io.StringIO()):
            self.assertEqual(self.sweep.run(cases, workers=8, per_account=2), 0)
        self.assertGreaterEqual(peaks["all"], 2)
        self.assertEqual(peaks["shared"], 1)
        for account in self.sweep.accounts:
            self.assertLessEqual(peaks[account.name], 2)

    def test_local_only_runs_without_server_fixtures(self):
        def local(ctx):
            self.assertTrue(ctx.offline)
            self.assertEqual(ctx.gh("environment").json()["GH_HOST"], "localhost.invalid")
            with self.assertRaises(AssertionError):
                ctx.native("req", "GET", "/")
            with self.assertRaises(AssertionError):
                ctx.repo
        classification = {"api": {"kind": "local_only", "reason": "fake executable"}}
        with patch.object(h, "load_classification", return_value=classification), contextlib.redirect_stdout(io.StringIO()):
            self.assertEqual(self.sweep.run([h.Case("api", "offline", local)]), 0)

    def test_provision_reuses_reserved_accounts_and_stores_ghes_credentials(self):
        account = self.sweep.accounts[0]
        self.assertEqual(account.name, "sw-a01")
        self.sweep.base = "http://127.0.0.1:12345"
        token = "cy_pat_" + "a" * 64
        with patch.object(self.sweep, "native", side_effect=["409", "", token]) as native:
            self.sweep.provision(account)
        self.assertEqual(native.call_args_list[1].args[1:3], ("form", "/login"))
        self.assertEqual(account.token, token)
        hosts = account.directory / "config/hosts.yml"
        self.assertIn("127.0.0.1:", hosts.read_text())
        self.assertEqual(hosts.stat().st_mode & 0o777, 0o600)
        auth = self.sweep.records[-1]["argv"]
        self.assertEqual(auth[:4], ["auth", "login", "--hostname", h.HOST])
        self.assertIn("--insecure-storage", auth)
        self.assertIsNone(self.sweep.records[-1]["case"])
        self.assertNotIn(token, repr(account))
        self.assertEqual(self.sweep.environment(account)["GH_HOST"], h.HOST)
        for base in ("https://github.com", "http://localhost:1234", "http://user@127.0.0.1:1234", "http://127.0.0.1/path"):
            with self.assertRaises(ValueError):
                h.Sweep(base)

    def test_fixture_contract_is_lazy_unique_and_uses_api_not_gh(self):
        definition = h.Case("repo view", "fixture", None)
        ctx = h.Ctx(self.sweep, self.sweep.accounts[0], definition)
        calls = []
        def api(account, method, path, data=None):
            calls.append((method, path, data))
            if path == "user/repos":
                return {"full_name": account.name + "/" + data["name"]}
            if method == "GET":
                return {"object": {"sha": "abc"}} if "/git/ref/" in path else {"default_branch": "main"}
            return {"number": 1, **(data or {})}
        with patch.object(self.sweep, "api", side_effect=api):
            self.assertEqual(ctx.repo, ctx.repo)
            self.assertEqual(ctx.issue["number"], 1)
            self.assertEqual(ctx.pr["number"], 1)
            self.assertIn("tag_name", ctx.release)
        self.assertEqual(sum(path == "user/repos" for _, path, _ in calls), 1)
        self.assertFalse(self.sweep.records)

    def test_example_import_and_cli_check_are_offline(self):
        module = importlib.import_module("gh_sweep.example_cases")
        if not h.CASES:
            importlib.reload(module)
        self.assertEqual(len(h.CASES), 3)
        inventory = json.loads((h.HERE / "inventory.json").read_text())
        self.assertEqual(sum(not row["children"] for row in inventory.values()), 197)
        result = subprocess.run([sys.executable, str(h.HERE / "harness.py"), "--check"],
                                capture_output=True, text=True, timeout=10)
        self.assertIn(result.returncode, (0, 1), result.stderr)
        report = json.loads(result.stdout)
        self.assertEqual(report["ok"], result.returncode == 0)
        self.assertEqual(report["invalid_cases"], [])


if __name__ == "__main__":
    unittest.main()
