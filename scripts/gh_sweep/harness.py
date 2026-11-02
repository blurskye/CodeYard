#!/usr/bin/env python3
"""Shared real-gh sweep contract (Python standard library only).
Import case, Ctx and expect_* from gh_sweep.harness; import lanes before checking.
@case(command="pr merge", variant="--squash", needs=("repo", "pr"),
      serial_group=None, timeout=120) registers a function taking one Ctx.
Use ctx.gh(*args, input=None, cwd=None, ok=(0,), timeout=60); ok=None captures errors.
ctx.account is the assigned Account; ctx.sweep.accounts exposes other identities.
ctx.name("label") and ctx.directory are unique per invocation, even on dirty servers.
ctx.repo is owner/name; ctx.issue and ctx.pr are REST documents with a number;
ctx.release is a REST release document. Lazy fixtures are cached per case.
needs eagerly builds those same fixtures; fixture API calls do NOT count as gh coverage.
ctx.api(method, path, data=None) uses /api/v3; ctx.native(*e2e_web_args) uses native forms.
ctx.org() and ctx.team(org) create unique organization/team fixtures.
expect_ok(r), expect_json(r, {"number": int}), expect_error(r, status=404,
contains="Not Found"), expect_unsupported(r) assert results; r.json() decodes stdout.
JSON shapes use types, dict subsets, one-element list shapes, or literal values.
Sweep(base, accounts=3) provisions sw-a01.. through native register/login/token forms,
then real gh auth login through the same test-owned TLS CONNECT transport as e2e-github.
Only disposable e2e loopback HTTP origins are accepted. Sweep(None) is offline.
with Sweep(base) as sweep: exit_code = sweep.run(CASES, workers=4, per_account=2).
serial_group is a sweep-wide mutex key; use the same key for a shared repository.
Timeouts kill subprocess groups. Python case bodies must cooperate (no infinite Python
loops): timed-out daemon bodies are abandoned, and further harness calls are rejected.
All gh argv are recorded, including secondary commands and failed invocations.
classification_<lane>.json (one per lane, merged): {"version":1,"commands":{"repo view":{"kind":"covered"}}}.
Other kinds require reason; unsupported_on_ghes requires expected {status, contains};
interactive_only requires expected {rc, contains}; local_only runs offline, not skipped.
Every kind still needs a registered case. --check fails missing classifications/cases.
waivers_<lane>.json (merged): {"version":1,"flags":{"repo view":{"--web":"reason"}}}; only flags
may be waived. coverage_report(records=sweep.records) reports all inventory flags.
"""
import argparse
import base64
import contextlib
from dataclasses import dataclass, field
import http.client
import http.server
import importlib
import json
import os
from pathlib import Path
import re
import signal
import socket
import ssl
import subprocess
import sys
import tempfile
import threading
import time
import uuid
from concurrent.futures import ThreadPoolExecutor
from urllib.parse import urlsplit

HERE = Path(__file__).resolve().parent
ROOT = HERE.parent.parent
HOST = "codeyard-e2e.test"
CASES = []
LOCAL = threading.local()


@dataclass(frozen=True)
class Case:
    command: str
    variant: str
    function: object
    needs: tuple = ()
    serial_group: str = None
    timeout: float = 120


def case(*, command, variant="default", needs=(), serial_group=None, timeout=120):
    def register(function):
        if any((c.command, c.variant) == (command, variant) for c in CASES):
            raise ValueError(f"duplicate case: {command} [{variant}]")
        if timeout <= 0 or set(needs) - {"repo", "issue", "pr", "release"}:
            raise ValueError("invalid case timeout/needs")
        CASES.append(Case(command, variant, function, tuple(needs), serial_group, timeout))
        return function
    return register


@dataclass
class Result:
    rc: int
    stdout: str
    stderr: str
    timed_out: bool = False

    def json(self):
        return json.loads(self.stdout)


def expect_ok(result):
    assert not result.timed_out and result.rc == 0, f"exit {result.rc}: {result.stderr}"
    return result


def expect_json(result, shape):
    value = expect_ok(result).json()

    def check(actual, wanted):
        if isinstance(wanted, type):
            assert isinstance(actual, wanted), (actual, wanted)
        elif isinstance(wanted, dict):
            assert isinstance(actual, dict), actual
            for key, child in wanted.items():
                assert key in actual, key
                check(actual[key], child)
        elif isinstance(wanted, list):
            assert isinstance(actual, list), actual
            if wanted:
                assert len(wanted) == 1, "list shapes must have one element"
                for child in actual:
                    check(child, wanted[0])
            else:
                assert actual == [], actual
        else:
            assert actual == wanted, (actual, wanted)
    check(value, shape)
    return value


def expect_error(result, status=404, contains=None):
    assert not result.timed_out and result.rc > 0, f"not a clean error: {result}"
    assert re.search(rf"\bHTTP {status}\b", result.stderr), result.stderr
    assert not re.search(r"panic:|panic at|segmentation fault|fatal error:", result.stderr, re.I), result.stderr
    if contains is not None:
        assert contains in result.stderr, result.stderr
    return result


def expect_unsupported(result):
    return expect_error(result, status=404, contains="Not Found")


def merged_documents(pattern, key):
    """Merge every lane's file (classification_<lane>.json, waivers_<lane>.json); a repeated entry is an error."""
    merged = {}
    for path in sorted(HERE.glob(pattern)):
        document = json.loads(path.read_text())
        if document.get("version") != 1 or not isinstance(document.get(key), dict):
            raise ValueError(f"{path.name} requires version=1 and a {key} object")
        for name, row in document[key].items():
            if key == "flags":
                merged.setdefault(name, {}).update(row)
            elif name in merged:
                raise ValueError(f"{name} appears in more than one {pattern} file")
            else:
                merged[name] = row
    return {"version": 1, key: merged}


def load_classification(path=None, inventory=None):
    inventory = inventory if inventory is not None else json.loads((HERE / "inventory.json").read_text())
    document = json.loads(Path(path).read_text()) if path else merged_documents("classification*.json", "commands")
    if document.get("version") != 1 or not isinstance(document.get("commands"), dict):
        raise ValueError("classification requires version=1 and commands object")
    for command, row in document["commands"].items():
        if command not in inventory or inventory[command]["children"]:
            raise ValueError(f"not an inventory leaf: {command}")
        kind = row.get("kind")
        if kind not in {"covered", "local_only", "unsupported_on_ghes", "interactive_only"}:
            raise ValueError(f"invalid classification: {command}")
        if kind != "covered" and (not isinstance(row.get("reason"), str) or not row["reason"].strip()):
            raise ValueError(f"reason required: {command}")
        expected = row.get("expected", {})
        if kind in {"unsupported_on_ghes", "interactive_only"}:
            key = "status" if kind == "unsupported_on_ghes" else "rc"
            if type(expected.get(key)) is not int or not isinstance(expected.get("contains"), str) or not expected["contains"].strip():
                raise ValueError(f"expected {key}/contains required: {command}")
            if (key == "status" and not 400 <= expected[key] < 500) or (key == "rc" and expected[key] <= 0):
                raise ValueError(f"expected clean failure required: {command}")
    return document["commands"]


def coverage_report(*, cases=None, records=(), inventory=None, classification=None, waivers=None):
    inventory = inventory if inventory is not None else json.loads((HERE / "inventory.json").read_text())
    classification = classification if classification is not None else load_classification(inventory=inventory)
    cases = CASES if cases is None else cases
    waivers = waivers if waivers is not None else merged_documents("waivers*.json", "flags")
    if waivers.get("version") != 1 or not isinstance(waivers.get("flags"), dict):
        raise ValueError("waivers requires version=1 and flags object")
    waived = set()
    for command, flags in waivers["flags"].items():
        for flag, reason in flags.items():
            if command not in inventory or flag not in inventory[command]["flags"] or not isinstance(reason, str) or not reason.strip():
                raise ValueError(f"invalid flag waiver: {command} {flag}")
            waived.add((command, flag))
    exercised = set()
    for record in records:
        if not record.get("case"):
            continue  # Authentication/fixture setup is not command coverage.
        argv = record["argv"]
        # Match the actual invocation, never the decorator's command. Longest wins.
        matches = [c for c in inventory if c and argv[:len(c.split())] == c.split()]
        command = max(matches, key=len) if matches else ""
        for arg in argv[len(command.split()):]:
            if arg == "--":
                break
            flag = arg.split("=", 1)[0]
            if flag == "-R":
                flag = "--repo"
            # Help/global options have distinct behavior at each command path.
            if flag in inventory.get(command, {}).get("flags", []):
                exercised.add((command, flag))
    leaves = {c for c, row in inventory.items() if not row["children"]}
    missing_cases = sorted(leaves - {c.command for c in cases})
    missing_classification = sorted(leaves - classification.keys())
    invalid_cases = sorted({c.command for c in cases} - leaves)
    missing_flags = sorted((c, f) for c, row in inventory.items() for f in row["flags"]
                           if (c, f) not in exercised | waived)
    return {"missing_classification": missing_classification, "missing_cases": missing_cases,
            "invalid_cases": invalid_cases, "missing_flags": missing_flags, "waived": len(waived),
            "ok": not (missing_classification or missing_cases or invalid_cases)}


class Tunnel(http.server.BaseHTTPRequestHandler):
    def log_message(self, *_):
        pass

    def do_CONNECT(self):
        if self.path != HOST + ":443":
            self.send_error(403)
            return
        self.connection.settimeout(10)
        self.send_response(200, "Connection established")
        self.end_headers()
        self.wfile.flush()
        self.close_connection = True
        try:
            with self.server.tls.wrap_socket(self.connection, server_side=True) as client:
                client.settimeout(60)
                with socket.create_connection(self.server.upstream, timeout=60) as upstream:
                    def upload():
                        try:
                            while data := client.recv(65536):
                                upstream.sendall(data)
                        except OSError:
                            pass
                        finally:
                            with contextlib.suppress(OSError):
                                upstream.shutdown(socket.SHUT_WR)
                    worker = threading.Thread(target=upload, daemon=True)
                    worker.start()
                    try:
                        while data := upstream.recv(65536):
                            client.sendall(data)
                    finally:
                        with contextlib.suppress(OSError):
                            client.shutdown(socket.SHUT_RDWR)
                        worker.join(timeout=2)
        except OSError:
            pass


@dataclass
class Account:
    name: str
    directory: Path
    token: str = field(default="", repr=False)


class Sweep:
    def __init__(self, base, *, accounts=3, gh="gh", directory=None):
        if accounts < 1:
            raise ValueError("accounts must be positive")
        origin = urlsplit(base or "")
        if base and (origin.scheme != "http" or origin.hostname != "127.0.0.1" or origin.path not in ("", "/") or origin.username or origin.query or origin.fragment):
            raise ValueError("base must be the disposable e2e HTTP loopback origin")
        self.base, self.binary = base, str(gh)
        self.temporary = tempfile.TemporaryDirectory(prefix="gh-sweep-", dir=directory)
        self.directory = Path(self.temporary.name)
        self.accounts = [Account(f"sw-a{i:02}", self.directory / f"sw-a{i:02}") for i in range(1, accounts + 1)]
        self.records, self.processes = [], {}
        self.lock = threading.Lock()
        self.closed = False
        self.proxy, self.certificate = "http://127.0.0.1:9", None
        self.listener = None
        for account in self.accounts:
            for folder in ("home", "config", "cache", "data", "state", "runtime"):
                (account.directory / folder).mkdir(parents=True, mode=0o700)

    def __enter__(self):
        try:
            if self.base:
                cert, key = self.directory / "cert.pem", self.directory / "key.pem"
                subprocess.run(["openssl", "req", "-x509", "-newkey", "rsa:2048", "-nodes", "-days", "1",
                                "-subj", "/CN=" + HOST, "-addext", "subjectAltName=DNS:" + HOST,
                                "-addext", "basicConstraints=critical,CA:TRUE", "-keyout", str(key), "-out", str(cert)],
                               check=True, capture_output=True, timeout=30)
                self.certificate = cert
                self.listener = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Tunnel)
                self.listener.daemon_threads = True
                self.listener.upstream = ("127.0.0.1", urlsplit(self.base).port or 80)
                self.listener.tls = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
                self.listener.tls.load_cert_chain(cert, key)
                self.listener_thread = threading.Thread(target=self.listener.serve_forever, daemon=True)
                self.listener_thread.start()
                self.proxy = f"http://127.0.0.1:{self.listener.server_port}"
                for account in self.accounts:
                    self.provision(account)
            return self
        except BaseException:
            self.__exit__(None, None, None)
            raise

    def __exit__(self, *_):
        with self.lock:
            self.closed = True
            for process, _ in self.processes.values():
                self.kill(process)
        if self.listener:
            self.listener.shutdown()
            self.listener.server_close()
            self.listener_thread.join(timeout=3)
        self.temporary.cleanup()

    def environment(self, account, *, offline=False):
        env = {k: v for k, v in os.environ.items() if not k.startswith(("GH_", "GITHUB_", "GIT_", "XDG_", "SSH_"))
               and k.lower() not in {"http_proxy", "https_proxy", "all_proxy", "no_proxy"}}
        for key, folder in {"HOME": "home", "GH_CONFIG_DIR": "config", "XDG_CONFIG_HOME": "config",
                            "XDG_CACHE_HOME": "cache", "XDG_DATA_HOME": "data", "XDG_STATE_HOME": "state",
                            "XDG_RUNTIME_DIR": "runtime"}.items():
            env[key] = str(account.directory / folder)
        env.update(GH_HOST=HOST if self.base and not offline else "localhost.invalid", GH_PROMPT_DISABLED="1",
                   GH_NO_UPDATE_NOTIFIER="1", GH_NO_EXTENSION_UPDATE_NOTIFIER="1", NO_COLOR="1", CLICOLOR="0",
                   GIT_TERMINAL_PROMPT="0", GIT_CONFIG_NOSYSTEM="1", GIT_CONFIG_GLOBAL=str(account.directory / "gitconfig"),
                   GH_BROWSER="false", BROWSER="false", GH_EDITOR="false", EDITOR="false", VISUAL="false",
                   GH_PAGER="cat", PAGER="cat")
        proxy = "http://127.0.0.1:9" if offline else self.proxy
        env.update(HTTPS_PROXY=proxy, https_proxy=proxy, HTTP_PROXY=proxy, http_proxy=proxy,
                   NO_PROXY="" if offline or not self.base else "127.0.0.1,localhost",
                   no_proxy="" if offline or not self.base else "127.0.0.1,localhost")
        if self.certificate:
            env.update(SSL_CERT_FILE=str(self.certificate), GIT_SSL_CAINFO=str(self.certificate))
        return env

    def redact(self, text):
        for account in self.accounts:
            if account.token:
                text = text.replace(account.token, "[redacted]")
        return text

    @staticmethod
    def kill(process):
        with contextlib.suppress(ProcessLookupError):
            os.killpg(process.pid, signal.SIGKILL)

    def execute(self, account, argv, *, input=None, cwd=None, ok=(0,), timeout=60):
        ctx = getattr(LOCAL, "ctx", None)
        if ctx:
            ctx.check_deadline()
            timeout = min(timeout, ctx.deadline - time.monotonic())
        if timeout <= 0:
            raise TimeoutError("command deadline expired")
        with self.lock:
            if self.closed or (ctx and ctx.cancelled.is_set()):
                raise TimeoutError("sweep/case closed")
            process = subprocess.Popen(list(map(str, argv)), stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                                       stderr=subprocess.PIPE, text=True, start_new_session=True,
                                       cwd=cwd or (ctx.directory if ctx else account.directory),
                                       env=self.environment(account, offline=bool(ctx and ctx.offline)))
            self.processes[process.pid] = (process, ctx)
        expired = False
        try:
            try:
                stdout, stderr = process.communicate(input, timeout=timeout)
            except subprocess.TimeoutExpired:
                expired = True
                self.kill(process)
                stdout, stderr = process.communicate(timeout=5)
        finally:
            with self.lock:
                self.processes.pop(process.pid, None)
        result = Result(process.returncode, stdout, stderr, expired)
        if expired:
            raise TimeoutError(f"command timed out after {timeout:.2f}s")
        if ok is not None and result.rc not in ok:
            raise AssertionError(self.redact(f"exit {result.rc}: {result.stderr}"))
        return result

    def gh(self, account, *args, input=None, cwd=None, ok=(0,), timeout=60):
        ctx = getattr(LOCAL, "ctx", None)
        with self.lock:
            self.records.append({"case": ctx.id if ctx else None, "account": account.name,
                                 "argv": [self.redact(str(a)) for a in args]})
        # Search allows 30 requests a minute per user, as on GitHub, and several cases share
        # each account: wait for the window to reset and run the command again, twice at most.
        for attempt in range(3):
            result = self.execute(account, [self.binary, *args], input=input, cwd=cwd, ok=None, timeout=timeout)
            if attempt == 2 or "HTTP 403: API rate limit exceeded" not in result.stderr:
                break
            self.wait_for_search(account)
        if ok is not None and result.rc not in ok:
            raise AssertionError(self.redact(f"exit {result.rc}: {result.stderr}"))
        return result

    def wait_for_search(self, account):
        reset = self.api(account, "GET", "rate_limit")["resources"]["search"]["reset"]
        ctx = getattr(LOCAL, "ctx", None)
        delay = min(max(reset - time.time(), 0) + 1, 61)
        if ctx:
            delay = min(delay, ctx.deadline - time.monotonic() - 5)
        if delay > 0:
            time.sleep(delay)

    def native(self, account, *args):
        ctx = getattr(LOCAL, "ctx", None)
        assert not (ctx and ctx.offline), "local_only cases cannot use native fixtures"
        assert self.base, "native fixtures need a server"
        return self.execute(account, [sys.executable, "-I", ROOT / "scripts/e2e-web.py", *args,
                                    "--base", self.base, "--jar", account.directory / "jar.json",
                                    "--who", account.name]).stdout.strip()

    def provision(self, account):
        # Stable, test-only password permits reusing the reserved accounts on dirty e2e servers.
        password = "Sweep-test-only-" + account.name + "-37!"
        status = self.native(account, "form", "/register", "--action", "/register", "--has", "username",
                             "--set", "username=" + account.name, "--set", "email=" + account.name + "@example.test",
                             "--set", "password=" + password, "--set", "confirm_password=" + password,
                             "--status", "303,409", "--print", "status")
        if status == "409":
            self.native(account, "form", "/login", "--action", "/login", "--has", "username",
                        "--set", "username=" + account.name, "--set", "password=" + password, "--location", "/")
        account.token = self.native(account, "form", "/settings?tab=tokens", "--action", "/settings/tokens",
                                    "--has", "name", "--set", "name=sweep-" + uuid.uuid4().hex,
                                    "--set", "scope=write", "--set", "expiry=never", "--status", "200",
                                    "--print", r"re:(?s)Copy this token now.*?(cy_pat_[0-9a-f]{64})")
        self.gh(account, "auth", "login", "--hostname", HOST, "--with-token", "--git-protocol", "https",
                "--insecure-storage", input=account.token + "\n")
        hosts = account.directory / "config/hosts.yml"
        with hosts.open("a") as handle:
            handle.write(f"127.0.0.1:\n    oauth_token: {account.token}\n    user: {account.name}\n")
        hosts.chmod(0o600)

    def api(self, account, method, path, data=None):
        ctx = getattr(LOCAL, "ctx", None)
        if ctx:
            ctx.check_deadline()
            assert not ctx.offline, "local_only cases cannot use server fixtures"
        origin = urlsplit(self.base or "")
        connection = http.client.HTTPConnection(origin.hostname, origin.port or 80,
                                               timeout=min(30, ctx.deadline - time.monotonic()) if ctx else 30)
        try:
            connection.request(method, "/api/v3/" + path.lstrip("/"),
                               body=json.dumps(data).encode() if data is not None else None,
                               headers={"Authorization": "Bearer " + account.token, "Content-Type": "application/json"})
            response = connection.getresponse()
            raw = response.read()
            assert 200 <= response.status < 300, self.redact(f"fixture HTTP {response.status}: {raw.decode(errors='replace')}")
            return json.loads(raw) if raw else None
        finally:
            connection.close()

    def create_repo(self, account, name=None, *, org=None, **data):
        return self.api(account, "POST", f"orgs/{org}/repos" if org else "user/repos",
                        {"name": name or "sw-" + uuid.uuid4().hex, "auto_init": True, **data})

    def create_org(self, account, name=None):
        slug = name or "sw-" + uuid.uuid4().hex
        self.native(account, "form", "/settings/organizations", "--action", "/settings/organizations",
                    "--has", "action=create", "--set", "slug=" + slug, "--set", "display_name=" + slug,
                    "--location", "/settings/organizations")
        return slug

    def create_team(self, account, org, name=None):
        return self.api(account, "POST", f"orgs/{org}/teams",
                        {"name": name or "sw-" + uuid.uuid4().hex, "privacy": "closed"})

    def run(self, cases=None, *, workers=4, per_account=1):
        cases = list(CASES if cases is None else cases)
        if workers < 1 or per_account < 1:
            raise ValueError("workers/per_account must be positive")
        classification = load_classification()
        limits = [threading.Semaphore(per_account) for _ in self.accounts]
        groups = {c.serial_group: threading.Lock() for c in cases if c.serial_group}

        def run_one(item):
            index, definition = item
            slot = index % len(self.accounts)
            with limits[slot], groups.get(definition.serial_group, contextlib.nullcontext()):
                started = time.monotonic()
                row = classification.get(definition.command, {})
                ctx = Ctx(self, self.accounts[slot], definition, offline=row.get("kind") == "local_only")
                errors = []

                def invoke():
                    LOCAL.ctx = ctx
                    try:
                        for fixture in definition.needs:
                            getattr(ctx, fixture)
                        definition.function(ctx)
                        ctx.check_deadline()
                    except BaseException as error:
                        # Bounded, so the last-gh note below always fits on the case's line.
                        errors.append(self.redact(str(error) or type(error).__name__)[:1200])
                        # Name the gh command the case ran last, so a failure points at its step.
                        with self.lock:
                            last = next((r["argv"] for r in reversed(self.records) if r["case"] == ctx.id), None)
                        if last:
                            errors.append("last gh: " + " ".join(last)[:300])
                    finally:
                        LOCAL.ctx = None
                thread = threading.Thread(target=invoke, daemon=True)
                thread.start()
                thread.join(max(0, ctx.deadline - time.monotonic()))
                if thread.is_alive():
                    ctx.cancelled.set()
                    with self.lock:
                        for process, owner in self.processes.values():
                            if owner is ctx:
                                self.kill(process)
                    errors = ["case timeout"]
                return {"case": definition.command + " [" + definition.variant + "]", "passed": not errors,
                        "duration": time.monotonic() - started, "error": " | ".join(errors)}

        with ThreadPoolExecutor(max_workers=workers) as pool:
            self.results = list(pool.map(run_one, enumerate(cases)))
        print("RESULT  SECONDS  CASE")
        for row in self.results:
            # One line per case: a multi-line error keeps its line breaks as \n.
            print(f"{'PASS' if row['passed'] else 'FAIL':6}  {row['duration']:7.2f}  {row['case']}" +
                  (f"  {row['error']}".replace("\n", "\\n") if row["error"] else ""), flush=True)
        report = coverage_report(cases=cases, records=self.records)
        (self.directory / "argv.json").write_text(json.dumps(self.records, indent=2) + "\n")
        passed = sum(row["passed"] for row in self.results)
        print(f"SWEEP passed={passed} failed={len(cases) - passed} waived={report['waived']}", flush=True)
        return int(passed != len(cases))


class Ctx:
    def __init__(self, sweep, account, definition, *, offline=False):
        self.sweep, self.account, self.definition, self.offline = sweep, account, definition, offline
        self.id = "sw-" + uuid.uuid4().hex[:16]
        self.directory = sweep.directory / self.id
        self.directory.mkdir(mode=0o700)
        self.deadline = time.monotonic() + definition.timeout
        self.cancelled = threading.Event()
        self.fixtures = {}

    def check_deadline(self):
        if self.cancelled.is_set() or time.monotonic() >= self.deadline:
            raise TimeoutError("case timeout")

    def name(self, label="fixture"):
        return self.id + "-" + re.sub(r"[^a-z0-9-]", "-", label.lower())[:25] + "-" + uuid.uuid4().hex[:6]

    def gh(self, *args, **kwargs):
        return self.sweep.gh(self.account, *args, **kwargs)

    def api(self, method, path, data=None):
        return self.sweep.api(self.account, method, path, data)

    def native(self, *args):
        self.check_deadline()
        assert not self.offline, "local_only cases cannot use native fixtures"
        return self.sweep.native(self.account, *args)

    def org(self):
        self.check_deadline()
        assert not self.offline, "local_only cases cannot use organization fixtures"
        return self.sweep.create_org(self.account, self.name("org"))

    def team(self, org):
        return self.sweep.create_team(self.account, org, self.name("team"))

    @property
    def repo(self):
        if "repo" not in self.fixtures:
            row = self.sweep.create_repo(self.account, self.name("repo"))
            self.fixtures["repo"] = row["full_name"]
        return self.fixtures["repo"]

    @property
    def issue(self):
        if "issue" not in self.fixtures:
            self.fixtures["issue"] = self.api("POST", f"repos/{self.repo}/issues", {"title": self.name("issue"), "body": "sweep fixture"})
        return self.fixtures["issue"]

    @property
    def pr(self):
        if "pr" not in self.fixtures:
            branch = self.name("head")
            base = self.api("GET", f"repos/{self.repo}")["default_branch"]
            sha = self.api("GET", f"repos/{self.repo}/git/ref/heads/{base}")["object"]["sha"]
            self.api("POST", f"repos/{self.repo}/git/refs", {"ref": "refs/heads/" + branch, "sha": sha})
            self.api("PUT", f"repos/{self.repo}/contents/{self.name('file')}.txt",
                     {"message": "sweep fixture", "content": base64.b64encode(b"sweep\n").decode(), "branch": branch})
            self.fixtures["pr"] = self.api("POST", f"repos/{self.repo}/pulls",
                                           {"title": self.name("pr"), "head": branch, "base": base, "body": "sweep fixture"})
        return self.fixtures["pr"]

    @property
    def release(self):
        if "release" not in self.fixtures:
            self.fixtures["release"] = self.api("POST", f"repos/{self.repo}/releases", {"tag_name": self.name("v1"), "name": self.name("release")})
        return self.fixtures["release"]


def main(argv=None):
    parser = argparse.ArgumentParser(description="Real-gh sweep; --check needs no server or gh")
    parser.add_argument("--check", action="store_true")
    parser.add_argument("--module", action="append", default=[], help="import a lane module (repeatable)")
    parser.add_argument("--records", type=Path, help="argv.json from a sweep")
    parser.add_argument("--base")
    parser.add_argument("--record-output", type=Path, help="persist recorded argv after temporary state is removed")
    parser.add_argument("--accounts", type=int, default=3)
    parser.add_argument("--workers", type=int, default=4)
    parser.add_argument("--per-account", type=int, default=1)
    args = parser.parse_args(argv)
    for module in args.module or [f"gh_sweep.{path.stem}" for path in sorted(HERE.glob("cases_*.py"))]:
        importlib.import_module(module)
    if args.check:
        report = coverage_report(records=json.loads(args.records.read_text()) if args.records else ())
        print(json.dumps(report, indent=2))
        return int(not report["ok"])
    if not args.base:
        parser.error("--base is required to run real server cases")
    with Sweep(args.base, accounts=args.accounts) as sweep:
        result = sweep.run(workers=args.workers, per_account=args.per_account)
        if args.record_output:
            args.record_output.write_text(json.dumps(sweep.records, indent=2) + "\n")
        return result


if __name__ == "__main__":
    # Direct script and -m entrypoints must share the registry imported by lanes.
    sys.path.insert(0, str(HERE.parent))
    sys.exit(importlib.import_module("gh_sweep.harness").main())
