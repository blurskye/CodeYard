#!/usr/bin/env python3
"""Two native PostgreSQL cells behind the real HTTP/SSH router, driven by gh and Git."""
import argparse
import base64
import contextlib
import hashlib
import http.server
import importlib.util
import json
import os
from pathlib import Path
import re
import secrets
import shlex
import signal
import socket
import ssl
import subprocess
import sys
import threading
import time
import urllib.error
import urllib.parse
import urllib.request

ROOT = Path(__file__).resolve().parents[2]


def load(name, path):
    spec = importlib.util.spec_from_file_location(name, path)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def ports(count):
    with contextlib.ExitStack() as owner:
        sockets = [owner.enter_context(socket.socket()) for _ in range(count)]
        for connection in sockets:
            connection.bind(("127.0.0.1", 0))
        return [connection.getsockname()[1] for connection in sockets]


class Deployment:
    def __init__(self, args):
        self.args = args
        self.root = args.run
        self.root.mkdir(parents=True, mode=0o700, exist_ok=True)
        self.processes = {}
        self.logs = []
        self.keys = {}
        self.settings = {}
        self.http = urllib.request.build_opener(urllib.request.ProxyHandler({}))
        values = ports(10)
        self.directory_address = f"127.0.0.1:{values[0]}"
        # A second directory listener on the same database serves search, so it can stop alone.
        self.search_address = f"127.0.0.1:{values[9]}"
        self.base = f"http://127.0.0.1:{values[1]}"
        self.ssh = values[2]
        self.native = [(f"http://127.0.0.1:{values[3]}", values[4], values[5]),
                       (f"http://127.0.0.1:{values[6]}", values[7], values[8])]
        self.env = {name: value for name, value in os.environ.items()
                    if not name.startswith(("GIT_", "GH_", "GITHUB_", "DATABASE_", "DIRECTORY_", "NODE_", "CLUSTER_", "IROH_"))}
        self.env.update(COOKIE_SECURE="false", PUBLIC_URL=self.base,
                        PUBLIC_HTTP_CLONE_BASE=self.base,
                        PUBLIC_SSH_CLONE_BASE=f"ssh://git@127.0.0.1:{self.ssh}",
                        RUST_LOG="warn", SSH_MODE="builtin", SECRETS_MASTER_KEY=secrets.token_hex(32))
        for name in ("directory", "search", "zero", "one", "router"):
            directory = self.root / name
            directory.mkdir(mode=0o700)
            (directory / ".env").write_text("")
            environment = dict(self.env, GITSERV_STATE_DIR=str(directory / "state"),
                               REPOS_PATH=str(directory / "repos"), HOST_KEY_PATH=str(directory / "host_key"))
            result = self.command(name, "cluster", "keygen", env=environment)
            key = dict(line.split("=", 1) for line in result.stdout.splitlines() if "=" in line)
            self.keys[name] = key
            environment["NODE_KEY"] = key["NODE_KEY"]
            self.settings[name] = environment
        for name in ("directory", "search"):
            self.settings[name]["DIRECTORY_DATABASE_URL"] = os.environ["E2E_CELLS_DIRECTORY_URL"]
        for name, variable in (("zero", "E2E_CELLS_ZERO_URL"), ("one", "E2E_CELLS_ONE_URL")):
            self.settings[name]["DATABASE_URL"] = os.environ[variable]

    def command(self, name, *arguments, env=None):
        result = subprocess.run([str(self.args.binary), *map(str, arguments)],
                                env=env or self.settings[name], cwd=self.root / name,
                                stdin=subprocess.DEVNULL, text=True, capture_output=True, timeout=60)
        assert result.returncode == 0, f"{name} operator failed: {result.stderr[-3000:]}"
        return result

    def start(self, name, *arguments):
        assert name not in self.processes
        log = (self.root / name / "server.log").open("a")
        self.logs.append(log)
        self.processes[name] = subprocess.Popen([str(self.args.binary), *map(str, arguments)],
                                              env=self.settings[name], cwd=self.root / name,
                                              stdin=subprocess.DEVNULL, stdout=log, stderr=log,
                                              start_new_session=True)

    def stop(self, name):
        process = self.processes.pop(name)
        process.send_signal(signal.SIGTERM)
        try:
            result = process.wait(timeout=45)
        except subprocess.TimeoutExpired:
            os.killpg(process.pid, signal.SIGKILL)
            process.wait(timeout=10)
            raise AssertionError(f"{name} did not stop within its shutdown deadline") from None
        assert result == 0, f"{name} exited {result}; see {self.root / name / 'server.log'}"

    def crash(self, name):
        process = self.processes.pop(name)
        os.killpg(process.pid, signal.SIGKILL)
        assert process.wait(timeout=10) == -signal.SIGKILL

    def close(self):
        failures = []
        owned = list(self.processes.items())[::-1]
        self.processes.clear()
        for _, process in owned:
            if process.poll() is None:
                process.send_signal(signal.SIGTERM)
        deadline = time.monotonic() + 20
        for name, process in owned:
            try:
                result = process.wait(timeout=max(0.01, deadline - time.monotonic()))
                if result != 0:
                    failures.append(f"{name} exited {result}")
            except subprocess.TimeoutExpired:
                os.killpg(process.pid, signal.SIGKILL)
                process.wait(timeout=5)
                failures.append(f"{name} exceeded the shared shutdown deadline")
        for log in self.logs:
            log.close()
        assert not failures, "; ".join(failures)

    def diagnostic(self, name):
        path = self.root / name / "server.log"
        with path.open("rb") as log:
            log.seek(0, 2)
            log.seek(max(0, log.tell() - 4096))
            return log.read(4096).decode("utf-8", errors="replace")

    def wait(self, name, url):
        deadline = time.monotonic() + 60
        while time.monotonic() < deadline:
            assert self.processes[name].poll() is None, f"{name} stopped: {self.diagnostic(name)}"
            try:
                with self.http.open(url + "/health", timeout=2) as response:
                    if response.status == 200:
                        return
            except (OSError, urllib.error.HTTPError):
                pass
            threading.Event().wait(0.1)
        raise AssertionError(f"{name} did not become ready: {self.diagnostic(name)}")

    def register(self, index, opened=True):
        name = ("zero", "one")[index]
        web, ssh, rpc = self.native[index]
        host_key = self.root / name / "host_key.pub"
        wire = base64.b64decode(host_key.read_text().split()[1], validate=True)
        pins = self.keys[name]["NODE_PUBLIC_KEY"] + "," + hashlib.sha256(wire).hexdigest()
        arguments = ["cell", "directory", "register", "--prefix", str(index), "--name", name,
                     "--region", "a", "--web-url", web, "--ssh-address", f"127.0.0.1:{ssh}",
                     "--rpc-address", f"127.0.0.1:{rpc}", "--node-key", pins]
        if opened:
            arguments.append("--open")
        self.command("directory", *arguments)

    def prepare(self):
        for name in ("zero", "one"):
            subprocess.run(["ssh-keygen", "-q", "-t", "ed25519", "-N", "", "-f", str(self.root / name / "host_key")],
                           check=True, capture_output=True, timeout=20)
        self.seed_existing_zero()
        self.command("directory", "cell", "directory", "initialize")
        for index, name in enumerate(("zero", "one")):
            self.command(name, "cell", "initialize", "--prefix", index)
            self.register(index, opened=index == 0)
        self.command("directory", "cell", "directory", "router-key", "--public-key", self.keys["router"]["NODE_PUBLIC_KEY"])
        self.start("directory", "cell", "directory", "serve", "--address", self.directory_address)
        self.listening("directory", self.directory_address)
        self.start_search()
        for kind in ("user", "organization", "repository"):
            publication = self.command("zero", "cell", "publish", "--directory-address", self.directory_address,
                "--directory-key", self.keys["directory"]["NODE_PUBLIC_KEY"], "--kind", kind, "--limit", "250")
            assert json.loads(publication.stdout)["count"] == (0 if kind == "organization" else 1)
        for index, name in enumerate(("zero", "one")):
            web, ssh, rpc = self.native[index]
            self.settings[name].update(DIRECTORY_ADDRESS=self.directory_address,
                DIRECTORY_PUBLIC_KEY=self.keys["directory"]["NODE_PUBLIC_KEY"],
                SEARCH_ADDRESS=self.search_address, SEARCH_PUBLIC_KEY=self.keys["search"]["NODE_PUBLIC_KEY"],
                CELL_RPC_ADDRESS=f"127.0.0.1:{rpc}", GITSERV_NODE_ROLES="server", IROH_BIND="127.0.0.1:0",
                HTTP_ADDR=web.removeprefix("http://"), SSH_ADDR=f"127.0.0.1:{ssh}", HTTP_TRUSTED_PROXY_IPS="127.0.0.1")
            self.start(name, "serve")
            self.wait(name, web)
        self.settings["router"].update(DIRECTORY_ADDRESS=self.directory_address,
            DIRECTORY_PUBLIC_KEY=self.keys["directory"]["NODE_PUBLIC_KEY"], CELL_REGION="a",
            GITSERV_NODE_ROLES="router", HTTP_ADDR=self.base.removeprefix("http://"), SSH_ADDR=f"127.0.0.1:{self.ssh}")
        self.start("router", "serve")
        self.wait("router", self.base)

    def listening(self, name, address):
        deadline = time.monotonic() + 60
        while True:
            assert self.processes[name].poll() is None, self.diagnostic(name)
            try:
                host, port = address.rsplit(":", 1)
                with socket.create_connection((host, int(port)), timeout=1):
                    return
            except OSError:
                assert time.monotonic() < deadline, self.diagnostic(name)
                threading.Event().wait(0.1)

    def start_search(self):
        self.start("search", "cell", "directory", "serve", "--address", self.search_address)
        self.listening("search", self.search_address)

    def web(self, who, command, *arguments, base=None):
        result = subprocess.run([sys.executable, "-I", str(ROOT / "tests/e2e/e2e-web.py"), command,
                                 "--base", base or self.base, "--jar", str(self.root / (who + ".json")), "--who", who, *map(str, arguments)],
                                text=True, capture_output=True, timeout=80, env=self.env)
        assert result.returncode == 0, f"{who} native HTTP failed: {result.stderr[-3000:]}"
        return result.stdout.strip()

    def account(self, who, setup=False, expected="303", base=None):
        route = "/setup" if setup else "/register"
        arguments = [route, "--action", route, "--has", "username", "--set", "username=" + who,
                     "--set", "email=" + who + "@example.test", "--set", "password=cell-acceptance-password",
                     "--set", "confirm_password=cell-acceptance-password", "--status", expected]
        if setup:
            arguments += ["--set", "node_mode=public"]
        self.web(who, "form", *arguments, base=base)

    def seed_existing_zero(self):
        original = self.settings["zero"].copy()
        web, ssh, _ = self.native[0]
        self.settings["zero"].update(HTTP_ADDR=web.removeprefix("http://"), SSH_ADDR=f"127.0.0.1:{ssh}",
            PUBLIC_URL=web, PUBLIC_HTTP_CLONE_BASE=web, GITSERV_NODE_ROLES="server", IROH_BIND="127.0.0.1:0")
        try:
            self.start("zero", "serve")
            self.wait("zero", web)
            self.account("alice", setup=True, base=web)
            self.existing = json.loads(self.web("alice", "req", "POST", "/api/repos", "--accept", "json",
                "--status", "201", "--json", json.dumps({"name": "existing", "description": "Before cell enrollment"}),
                "--print", "body", base=web))
            self.existing_sha = self.web("alice", "req", "POST", "/api/repos/alice/existing/contents/README.md",
                "--accept", "json", "--status", "201", "--json", json.dumps({
                    "content": base64.b64encode(b"Before cell enrollment\n").decode(), "message": "Before cell enrollment",
                    "branch": "main", "expected_revision": "unborn"}), "--print", "json:sha", base=web)
            self.existing_token = self.token("alice", base=web)
            self.ssh_key("alice", base=web)
            self.existing_session = json.loads((self.root / "alice.json").read_text())["session_id"]
        finally:
            try:
                if "zero" in self.processes:
                    self.stop("zero")
            finally:
                self.settings["zero"] = original

    def token(self, who, base=None):
        value = self.web(who, "form", "/settings?tab=tokens", "--action", "/settings/tokens", "--has", "name",
                         "--set", "name=cells acceptance", "--set", "scope=write", "--set", "expiry=never", "--status", "200",
                         "--print", r"re:(?s)Copy this token now.*?(cy_pat_(?:[0-9]{1,5}_)?[0-9a-f]{64})", base=base)
        assert re.fullmatch(r"cy_pat_(?:[0-9]{1,5}_)?[0-9a-f]{64}", value)
        return value

    def ssh_key(self, who, base=None):
        key = self.root / (who + "-key")
        subprocess.run(["ssh-keygen", "-q", "-t", "ed25519", "-N", "", "-f", str(key)],
                       check=True, capture_output=True, timeout=20)
        self.web(who, "form", "/settings?tab=keys", "--action", "/settings/keys", "--has", "public_key",
                 "--set", "name=cells SSH", "--set", "public_key=" + key.with_suffix(".pub").read_text().strip(),
                 "--status", "303", base=base)

    def ssh_environment(self, who):
        return {"GIT_SSH_COMMAND": shlex.join(["ssh", "-F", "/dev/null", "-o", "BatchMode=yes", "-o", "IdentitiesOnly=yes",
            "-o", "IdentityAgent=none", "-o", "ConnectTimeout=15", "-o", "StrictHostKeyChecking=accept-new",
            "-o", "UserKnownHostsFile=" + str(self.root / "known_hosts"), "-i", str(self.root / (who + "-key"))])}

    def browser_navigation(self, who, user_id, repository_id):
        try:
            from playwright.sync_api import Error as BrowserError, expect, sync_playwright
        except ImportError as error:
            raise AssertionError("The cells browser check requires Playwright") from error

        session = json.loads((self.root / (who + ".json")).read_text())["session_id"]
        try:
            with sync_playwright() as playwright:
                browser = playwright.chromium.launch(executable_path=os.environ["CODEYARD_BROWSER_CHROMIUM"])
                try:
                    for javascript in (False, True):
                        context = browser.new_context(java_script_enabled=javascript)
                        try:
                            context.add_cookies([{"name": "session_id", "value": session, "url": self.base}])
                            page = context.new_page()
                            page.set_default_timeout(15000)
                            page.set_default_navigation_timeout(60000)
                            errors = []
                            page.on("pageerror", lambda error: errors.append(str(error)))

                            def navigate(path, link):
                                if javascript:
                                    with page.expect_response(lambda response:
                                            response.url == self.base + path and
                                            response.request.headers.get("accept") == "application/vnd.codeyard.page+json") as pending:
                                        link.click()
                                    response = pending.value
                                    assert response.status == 200 and response.request.resource_type != "document", path
                                else:
                                    link.click()
                                page.wait_for_url(self.base + path)
                                if javascript:
                                    page.locator('[data-hydrated="true"]').wait_for(state="attached", timeout=60000)

                            response = page.goto(self.base + f"/{who}/other")
                            assert response.status == 200
                            if javascript:
                                page.locator('[data-hydrated="true"]').wait_for(state="attached", timeout=60000)
                                exact = page.locator("#codeyard-bootstrap").evaluate("""script => {
                                    const initial = JSON.parse(script.textContent).initial;
                                    if (initial.page.lane !== "Repositories") return false;
                                    const repo = initial.page.model.Tree.repo;
                                    return [repo.viewer_id, repo.id].map(id => ({
                                        safe: Number.isSafeInteger(id), text: String(id)
                                    }));
                                }""")
                                assert exact == [{"safe": True, "text": str(user_id)},
                                                 {"safe": True, "text": str(repository_id)}], exact
                            response = page.goto(self.base + "/")
                            assert response.status == 200
                            if javascript:
                                page.locator('[data-hydrated="true"]').wait_for(state="attached", timeout=60000)
                            shared = page.locator('.discovery-shared a[href="/alice/shared"]')
                            expect(shared).to_be_visible()
                            navigate("/alice/shared", shared)
                            expect(page.get_by_text("two cells", exact=True)).to_be_visible()
                            navigate("/", page.locator("header .site-brand"))
                            expect(shared).to_be_visible()
                            assert not errors, errors
                        finally:
                            context.close()
                finally:
                    browser.close()
        except BrowserError as error:
            raise AssertionError(str(error)) from error


def cross_cell_fork(deployment, client, directory):
    """The fork belongs to Bob's cell; the pull and its checked-out head belong to Alice's."""
    client.api("repos/alice/shared", "--method", "PATCH", "-F", "allow_forking=true")
    client.gh("repo", "fork", "alice/shared", "--fork-name", "forked",
              "--clone=false", who="bob")
    fork = client.api("repos/bob/forked", who="bob")
    assert 2**37 <= fork["id"] < 2**53 and fork["fork"], fork
    assert fork["parent"]["full_name"] == "alice/shared", fork
    path = directory / "cross-cell-fork"
    client.execute(["git", "clone", deployment.base + "/bob/forked.git", str(path)],
                   who="bob", git_auth=True)

    def git(*args, who="bob"):
        return client.execute(["git", "-C", str(path), *args], who=who, git_auth=True).stdout.strip()

    git("config", "user.name", "Cell fork acceptance")
    git("config", "user.email", "fork@example.test")

    def pull(branch, title):
        git("checkout", "-B", branch, "origin/main")
        (path / f"{branch}.txt").write_text(f"first {branch} push\n")
        git("add", f"{branch}.txt")
        git("commit", "-m", f"Create {branch}")
        git("push", "origin", branch)
        created = client.gh("pr", "create", "-R", "alice/shared", "--base", "main",
                            "--head", f"bob:{branch}", "--title", title,
                            "--body", "The base must follow each head-cell push", who="bob", cwd=path)
        number = created.stdout.strip().rstrip("/").rsplit("/", 1)[1]
        assert number.isdecimal(), created.stdout
        return number

    def follows(number, tip):
        deadline = time.monotonic() + 60
        while True:
            # The fork's owner reads its head repository; private-fork access does not yet
            # pass to the upstream owner across cells.
            pull = client.api(f"repos/alice/shared/pulls/{number}", who="bob")
            assert pull["head"]["repo"]["id"] == fork["id"], pull
            if pull["head"]["sha"] == tip:
                break
            assert time.monotonic() < deadline, "the base cell did not receive its fork head push"
            threading.Event().wait(0.1)
        advertised = client.execute(["git", "ls-remote", deployment.base + "/alice/shared.git",
                                      f"refs/pull/{number}/head"], git_auth=True).stdout.split()
        assert advertised and advertised[0] == tip, advertised

    def checkout(number, name):
        checked = directory / name
        # gh pr checkout needs an origin on its own host.
        client.execute(["git", "clone", f"https://{client.env['GH_HOST']}/alice/shared.git", str(checked)], git_auth=True)
        client.gh("pr", "checkout", number, "-R", "alice/shared", "--branch", name, cwd=checked, git_auth=True)
        return checked

    def merged(number, name, text):
        client.gh("pr", "merge", number, "-R", "alice/shared", "--merge")
        assert client.data("pr", "view", number, "-R", "alice/shared", "--json", "state")["state"] == "MERGED"
        git("fetch", deployment.base + "/alice/shared.git", "main", who="alice")
        assert git("show", f"FETCH_HEAD:{name}") == text

    with client.check("cross-cell forks follow pushes, gh pr checkout and gh pr merge"):
        number = pull("cross-cell-head", "From a fork in cell one")
        follows(number, git("rev-parse", "HEAD"))
        (path / "cross-cell-head.txt").write_text("second foreign fork push\n")
        git("add", "cross-cell-head.txt")
        git("commit", "-m", "Advance the cross-cell fork head")
        git("push", "origin", "cross-cell-head")
        follows(number, git("rev-parse", "HEAD"))
        checked = checkout(number, "cross-cell-pull-checkout")
        assert (checked / "cross-cell-head.txt").read_text() == "second foreign fork push\n"
        merged(number, "cross-cell-head.txt", "second foreign fork push")
    with client.check("upstream maintainers push to a cross-cell fork pull head"):
        number = pull("maintainer-head", "Maintainer edits across cells")
        follows(number, git("rev-parse", "HEAD"))
        checked = checkout(number, "cross-cell-maintainer-checkout")
        for key, value in (("user.name", "Upstream maintainer"), ("user.email", "maintainer@example.test")):
            client.execute(["git", "-C", str(checked), "config", key, value])
        (checked / "maintainer.txt").write_text("upstream maintainer writes the remote fork\n")
        client.execute(["git", "-C", str(checked), "add", "maintainer.txt"])
        client.execute(["git", "-C", str(checked), "commit", "-m", "Adjust the remote fork as its upstream maintainer"])
        client.execute(["git", "-C", str(checked), "push",
                        f"ssh://git@127.0.0.1:{deployment.ssh}/bob/forked.git", "HEAD:maintainer-head"],
                       env=deployment.ssh_environment("alice"))
        follows(number, client.execute(["git", "-C", str(checked), "rev-parse", "HEAD"]).stdout.strip())
        merged(number, "maintainer.txt", "upstream maintainer writes the remote fork")


def eventually(check, limit=60):
    deadline = time.monotonic() + limit
    while True:
        try:
            return check()
        except AssertionError:
            if time.monotonic() >= deadline:
                raise
            threading.Event().wait(2)


def discovery_across_cells(deployment, client):
    """Explore, Trending and "used by" list public repositories of both cells, never private ones."""
    def page(who, path, *checks):
        return lambda: deployment.web(who, "req", "GET", path, "--no-scripts", *checks)
    eventually(page("visitor", "/explore?q=beacon&sort=stars", "--contains", "/alice/beacon-zero",
                    "--contains", "/bob/beacon-one", "--lacks", "beacon-secret"))
    # Trending leaves out the viewer's own repositories and covers the other cell.
    eventually(page("alice", "/?q=beacon", "--contains", "/bob/beacon-one"))
    eventually(page("bob", "/?q=beacon", "--contains", "/alice/beacon-zero", "--lacks", "beacon-secret"))
    for name, body in (("package.json", {"name": "beacon-lib", "version": "1.0.0"}),
                       ("package-lock.json", {"name": "beacon-lib", "version": "1.0.0", "lockfileVersion": 3,
                                              "requires": True, "packages": {"": {"name": "beacon-lib", "version": "1.0.0"}}})):
        client.api(f"repos/alice/beacon-zero/contents/{name}", "--method", "PUT", "-f", "message=Provide beacon-lib",
                   "-f", "content=" + base64.b64encode(json.dumps(body).encode()).decode())

    def depend(repository, who):
        head = client.api(f"repos/{repository}/commits/main", who=who)["sha"]
        snapshot = {"version": 0, "sha": head, "ref": "refs/heads/main",
                    "job": {"id": "1", "correlator": "cells-acceptance"},
                    "detector": {"name": "cells-acceptance", "version": "1.0.0", "url": "https://example.test/detector"},
                    "scanned": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()),
                    "manifests": {"package-lock.json": {"name": "package-lock.json",
                                  "file": {"source_location": "package-lock.json"},
                                  "resolved": {"beacon-lib": {"package_url": "pkg:npm/beacon-lib@1.0.0",
                                                              "relationship": "direct", "scope": "runtime"}}}}}
        client.api(f"repos/{repository}/dependency-graph/snapshots", "--method", "POST", "--input", "-",
                   input=json.dumps(snapshot), who=who)
    depend("bob/beacon-one", "bob")
    # A private repository's graph is off by default. Enable it through the public API
    # before testing that a private dependent is never listed, not even to its owner.
    enabled = client.gh("api", "repos/alice/beacon-secret/vulnerability-alerts",
                        "--method", "PUT", "--include").stdout
    assert "204" in enabled.splitlines()[0], enabled
    depend("alice/beacon-secret", "alice")
    for who in ("visitor", "alice"):
        eventually(page(who, "/alice/beacon-zero/dependency-graph", "--contains", "Used by 1",
                        "--contains", "/bob/beacon-one", "--lacks", "beacon-secret"))
    client.checked("Explore, Trending and used by list public repositories of both cells and never private ones")


def global_search(deployment, client, tokens, http_get):
    def put_file(repository, who):
        client.api(f"repos/{repository}/contents/beacon.txt", "--method", "PUT", "-f", "message=Add the beacon",
                   "-f", "content=" + base64.b64encode(b"first line\nglowmarker shines here\n").decode(), who=who)
    client.gh("repo", "create", "alice/beacon-zero", "--public")
    client.gh("repo", "create", "alice/beacon-secret", "--private")
    client.gh("repo", "create", "bob/beacon-one", "--public", who="bob")
    for repository, who in (("alice/beacon-zero", "alice"), ("alice/beacon-secret", "alice"), ("bob/beacon-one", "bob")):
        put_file(repository, who)
    client.gh("issue", "create", "-R", "alice/beacon-zero", "--title", "Beacon issue zero", "--body", "cell zero")
    client.gh("issue", "create", "-R", "bob/beacon-one", "--title", "Beacon issue one", "--body", "cell one", who="bob")
    client.gh("issue", "create", "-R", "alice/beacon-secret", "--title", "Beacon issue secret", "--body", "private")
    changed = time.monotonic()

    def budget(who, needed):
        # Search allows 30 requests a minute per account, as on GitHub; wait for the window.
        window = client.api("rate_limit", who=who)["resources"]["search"]
        if window["remaining"] < needed:
            threading.Event().wait(max(1, window["reset"] - time.time() + 1))

    def searched(who):
        budget(who, 5)
        repos = {item["fullName"] for item in client.data("search", "repos", "beacon", "--json", "fullName", who=who)}
        issues = {item["title"] for item in client.data("search", "issues", "beacon", "--json", "title", who=who)}
        pulls = {item["title"] for item in client.data("search", "prs", "collaborator", "--json", "title", who=who)}
        shared = {item["fullName"] for item in client.data("search", "repos", "shared", "--json", "fullName", who=who)}
        code = {item["repository"]["nameWithOwner"]
                for item in client.data("search", "code", "glowmarker", "--json", "path,repository", who=who)}
        return repos, issues, pulls, shared, code
    public = {"alice/beacon-zero", "bob/beacon-one"}
    expected = {
        "alice": (public | {"alice/beacon-secret"}, {"Beacon issue zero", "Beacon issue one", "Beacon issue secret"},
                  {"Across cells"}, {"alice/shared"}, public | {"alice/beacon-secret"}),
        # Bob reads alice/shared as a collaborator, but not alice/beacon-secret.
        "bob": (public, {"Beacon issue zero", "Beacon issue one"}, {"Across cells"}, {"alice/shared"}, public),
    }

    def converge(limit):
        deadline = time.monotonic() + limit
        while True:
            seen = {who: searched(who) for who in expected}
            if seen == expected:
                return
            assert time.monotonic() < deadline, f"search did not converge: {seen}"
            threading.Event().wait(3)
    converge(60)
    assert time.monotonic() - changed <= 60
    status, body = http_get("/api/v3/search/repositories?q=beacon")
    assert status == 200 and {item["full_name"] for item in json.loads(body)["items"]} == public, body
    status, body = http_get("/api/v3/search/repositories?q=shared")
    assert status == 200 and json.loads(body)["items"] == [], body
    # Code items carry GitHub's shape, with text matches from the indexed file.
    budget("alice", 1)
    code = client.api("search/code?q=glowmarker+repo:bob/beacon-one", "-H", "Accept: application/vnd.github.text-match+json")
    assert code["total_count"] == 1 and code["items"][0]["path"] == "beacon.txt", code
    assert "glowmarker" in code["items"][0]["text_matches"][0]["fragment"], code

    discovery_across_cells(deployment, client)

    # An index rebuilt from nothing returns the same results.
    deployment.command("directory", "cell", "directory", "search-reset")
    assert searched("alice")[0] == set()
    for name in ("zero", "one"):
        scratch = deployment.root / name / "reindex-state"
        scratch.mkdir(mode=0o700, exist_ok=True)
        queued = deployment.command(name, "cell", "search-reindex", "--limit", "1000",
                                    env=dict(deployment.settings[name], GITSERV_STATE_DIR=str(scratch)))
        assert json.loads(queued.stdout)["count"] >= 1, queued.stdout
    converge(60)

    # Without the index, pages work and search says it is unavailable; changes made meanwhile
    # arrive once it is back.
    deployment.stop("search")
    budget("alice", 2)
    status, body = http_get("/api/v3/search/repositories?q=beacon", tokens["alice"])
    assert status == 503 and "Search is unavailable" in json.loads(body)["message"], body
    assert client.gh("search", "repos", "beacon", ok=(1,)).returncode == 1
    assert client.api("repos/bob/beacon-one")["name"] == "beacon-one"
    deployment.web("alice", "req", "GET", "/bob/beacon-one", "--no-scripts", "--contains", "beacon-one")
    client.gh("issue", "create", "-R", "bob/beacon-one", "--title", "Beacon issue late", "--body", "while search was down", who="bob")
    deployment.start_search()
    for who in expected:
        expected[who][1].add("Beacon issue late")
    converge(90)


def presence_and_rollout(deployment, client):
    """Operators add and remove org presence by name, and roll a change out to cells in waves."""
    def operator(*arguments, env=None):
        return subprocess.run([str(deployment.args.binary), *map(str, arguments)],
                              env=env or deployment.settings["directory"], cwd=deployment.root / "directory",
                              stdin=subprocess.DEVNULL, text=True, capture_output=True, timeout=300)

    def presence(*arguments):
        return json.loads(deployment.command("directory", "cell", "directory", "presence",
                                             "--organization", "cell-org", "--region", "b", *arguments).stdout)
    added = presence()
    assert added["changed"] and added["regions"] == ["b"], added
    again = presence()
    assert not again["changed"] and again["regions"] == ["b"], again
    removed = presence("--remove")
    assert removed["changed"] and removed["regions"] == [], removed
    for hostile in (("--organization", "alice", "--region", "b"), ("--organization", "cell-org", "--region", "B!")):
        refused = operator("cell", "directory", "presence", *hostile)
        assert refused.returncode != 0 and not refused.stdout, refused.stdout

    log = deployment.root / "rollout.log"
    script = deployment.root / "rollout-apply.sh"
    script.write_text('#!/bin/sh\nprintf "%s %s\\n" "$1" "$2" >> "$ROLLOUT_LOG"\n[ "$1" != "$ROLLOUT_FAIL" ]\n')
    script.chmod(0o700)
    env = dict(deployment.settings["directory"], ROLLOUT_LOG=str(log), ROLLOUT_FAIL="0")
    stopped = operator("cell", "directory", "rollout", "--apply", script, "--health-seconds", "30", env=env)
    assert stopped.returncode != 0, stopped.stdout
    reports = [json.loads(line) for line in stopped.stdout.splitlines()]
    assert [(r["wave"], r["cell"], r["result"]) for r in reports] == [(1, 0, "failed")], reports
    assert log.read_text() == "0 zero\n", "a failing canary must leave later cells unchanged"
    log.unlink()
    env["ROLLOUT_FAIL"] = "none"
    finished = operator("cell", "directory", "rollout", "--apply", script, "--canary", "1", "--health-seconds", "30", env=env)
    assert finished.returncode == 0, finished.stderr[-3000:]
    reports = [json.loads(line) for line in finished.stdout.splitlines()]
    assert [(r["wave"], r["cell"], r["result"]) for r in reports] == [(1, 1, "updated"), (2, 0, "updated")], reports
    assert log.read_text() == "1 one\n0 zero\n"
    client.checked("org presence is set by name and refused for users and bad regions; a rollout stops at "
                   "a failing canary and otherwise covers every cell in waves after health checks")


def acceptance(deployment):
    github = load("codeyard_cells_github", ROOT / "tests/e2e/e2e-github.py")
    assert json.loads((deployment.root / "alice.json").read_text())["session_id"] == deployment.existing_session
    current = json.loads(deployment.web("alice", "req", "GET", "/api/repos/alice/existing",
        "--accept", "json", "--print", "body"))
    assert current["id"] == deployment.existing["id"] and current["description"] == "Before cell enrollment", current
    deployment.web("alice", "req", "GET", "/alice/existing/raw/main/README.md", "--contains", "Before cell enrollment")
    print("PASS existing standalone account, session, repository ID and commit survive cell zero enrollment", flush=True)
    deployment.register(0, opened=False)
    deployment.register(1)
    deployment.account("bob")
    assert json.loads((deployment.root / "bob.json").read_text())["session_id"].startswith("1.")
    # A fresh browser is placed in cell one, which must refuse cell zero's name.
    deployment.web("duplicate", "form", "/register", "--action", "/register", "--has", "username",
                   "--set", "username=alice", "--set", "email=duplicate@example.test",
                   "--set", "password=cell-acceptance-password", "--set", "confirm_password=cell-acceptance-password", "--status", "409")
    print("PASS real-client native setup, cell one signup and cross-cell duplicate name refusal", flush=True)
    deployment.register(0)
    tokens = {"alice": deployment.existing_token, "bob": deployment.token("bob")}
    assert tokens["bob"].startswith("cy_pat_1_")
    state = deployment.root / "state-files"
    state.mkdir(mode=0o700)
    (state / "write_token").write_text(tokens["alice"])
    (state / "gh_bob_token").write_text(tokens["bob"])
    deployment.ssh_key("bob")

    directory = deployment.root / "real-clients"
    directory.mkdir(mode=0o700)
    cert, key = github.certificate(directory)
    proxy = http.server.ThreadingHTTPServer(("127.0.0.1", 0), github.Tunnel)
    proxy.tls = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
    proxy.tls.load_cert_chain(cert, key)
    upstream = urllib.parse.urlsplit(deployment.base)
    proxy.upstream = (upstream.hostname, upstream.port)
    with github.serving(proxy):
        client = github.Client(deployment.base, directory, f"http://127.0.0.1:{proxy.server_port}", cert)
        client.login()
        alice = client.api("user")
        bob = client.api("user", who="bob")
        assert alice["id"] == deployment.existing["owner_id"], alice
        assert alice["id"] != bob["id"] and bob["id"] > alice["id"]
        assert 2**37 <= bob["id"] < 2**53, bob["id"]
        existing = client.api("repos/alice/existing")
        assert existing["id"] == deployment.existing["id"], existing
        refs = client.execute(["git", "ls-remote", deployment.base + "/alice/existing.git", "refs/heads/main"])
        assert refs.stdout.split()[0] == deployment.existing_sha, refs.stdout
        original_clone = directory / "existing-cell-zero"
        client.execute(["git", "clone", deployment.base + "/alice/existing.git", str(original_clone)])
        assert (original_clone / "README.md").read_text() == "Before cell enrollment\n"
        original_ssh_clone = directory / "existing-cell-zero-ssh"
        client.execute(["git", "clone", f"ssh://git@127.0.0.1:{deployment.ssh}/alice/existing.git", str(original_ssh_clone)],
                       env=deployment.ssh_environment("alice"))
        assert (original_ssh_clone / "README.md").read_text() == "Before cell enrollment\n"
        client.checked("existing standalone IDs, session, token, SSH key and Git data survive cell zero enrollment")
        client.gh("repo", "create", "alice/shared", "--private")
        client.gh("repo", "create", "bob/other", "--public", who="bob")
        other = client.api("repos/bob/other", who="bob")
        assert 2**37 <= other["id"] < 2**53
        client.clone.mkdir()
        client.git("init", "--initial-branch=main")
        client.git("config", "user.name", "Cell acceptance")
        client.git("config", "user.email", "cells@example.test")
        (client.clone / "README.md").write_text("two cells\n")
        client.git("add", "README.md")
        client.git("commit", "-m", "Create the shared repository")
        client.git("remote", "add", "origin", deployment.base + "/alice/shared.git")
        client.git("push", "origin", "main", git_auth=True)
        invitation = client.api("repos/alice/shared/collaborators/bob", "--method", "PUT", "-f", "permission=push")
        accepted = client.gh("api", f"user/repository_invitations/{invitation['id']}",
                             "--method", "PATCH", "--include", who="bob")
        assert re.match(r"HTTP/\S+ 204(?: |$)", accepted.stdout), accepted.stdout
        client.checked("cell one collaborator accepts the cell zero repository invitation")
        deadline = time.monotonic() + 30
        while not any(repo["full_name"] == "alice/shared" for repo in client.api("user/repos", who="bob")):
            assert time.monotonic() < deadline, "the home repository list did not receive the remote membership"
            threading.Event().wait(0.1)
        assert client.api("user/repos?per_page=1&page=1", who="bob")[0]["full_name"] == "alice/shared"
        assert client.api("user/repos?per_page=1&page=2", who="bob")[0]["full_name"] == "bob/other"
        client.checked("gh api user/repos discovers the remote collaborator repository through its home pointer")
        deployment.web("bob", "req", "GET", "/", "--no-scripts", "--contains", "/alice/shared")
        dashboard = json.loads(deployment.web("bob", "req", "GET", "/",
            "--accept", "application/vnd.codeyard.page+json", "--print", "body"))["page"]["model"]["Home"]
        assert dashboard["shared_count"] == 1, dashboard
        assert dashboard["shared"][0]["repository"]["name"] == "shared", dashboard
        client.checked("the home dashboard shows the foreign private repository in raw HTML and hydration data")
        with client.check("cross-cell stars survive a resource-cell crash and never disclose private repositories"):
            for method in ("PUT", "PUT"):
                response = client.gh("api", "user/starred/alice/shared", "--method", method, "--include", who="bob")
                assert re.match(r"HTTP/\S+ 204(?: |$)", response.stdout), response.stdout
            deadline = time.monotonic() + 30
            while not any(repo["full_name"] == "alice/shared" for repo in client.api("user/starred", who="bob")):
                assert time.monotonic() < deadline, "the home star list did not receive its resource pointer"
                threading.Event().wait(0.1)
            assert client.api("repos/alice/shared")["stargazers_count"] == 1
            stamped = client.api("user/starred", "-H", "Accept: application/vnd.github.star+json", who="bob")
            assert len(stamped) == 1 and stamped[0]["repo"]["full_name"] == "alice/shared" and stamped[0]["starred_at"], stamped
            anonymous = urllib.request.Request(deployment.base + "/api/v3/users/bob/starred")
            with deployment.http.open(anonymous, timeout=15) as response:
                assert json.load(response) == []
            deployment.web("bob", "req", "GET", "/stars", "--no-scripts", "--contains", "/alice/shared")
            stars = json.loads(deployment.web("bob", "req", "GET", "/stars", "--accept", "application/vnd.codeyard.page+json", "--print", "body"))["page"]["model"]["Stars"]
            assert len(stars["repositories"]) == 1 and stars["repositories"][0]["name"] == "shared", stars
            client.gh("api", "user/starred/alice/shared", "--method", "DELETE", who="bob")
            deadline = time.monotonic() + 30
            while client.api("user/starred", who="bob"):
                assert time.monotonic() < deadline, "the home star pointer did not receive removal"
                threading.Event().wait(0.1)
            # A foreign credential needs its live home: while it is down the star is refused.
            deployment.stop("one")
            refused = client.gh("api", "user/starred/alice/shared", "--method", "PUT", who="bob", ok=(1,))
            assert "HTTP 503" in refused.stderr, refused.stderr[-2000:]
            assert client.api("repos/alice/shared")["stargazers_count"] == 0
            deployment.start("one", "serve")
            deployment.wait("one", deployment.native[1][0])
            # Kill the source right after the committed star, before its delivery is certain.
            client.gh("api", "user/starred/alice/shared", "--method", "PUT", who="bob")
            deployment.crash("zero")
            deployment.start("zero", "serve")
            deployment.wait("zero", deployment.native[0][0])
            deadline = time.monotonic() + 60
            while not client.api("user/starred", who="bob"):
                assert time.monotonic() < deadline, "the committed star outbox did not recover after SIGKILL"
                threading.Event().wait(0.1)
            rows = client.api("user/starred", who="bob")
            assert len(rows) == 1 and rows[0]["full_name"] == "alice/shared", rows
            assert client.api("repos/alice/shared")["stargazers_count"] == 1
            client.gh("api", "user/starred/alice/shared", "--method", "DELETE", who="bob")
            deadline = time.monotonic() + 30
            while client.api("user/starred", who="bob"):
                assert time.monotonic() < deadline, "the recovered star could not be removed"
                threading.Event().wait(0.1)
            assert client.api("repos/alice/shared")["stargazers_count"] == 0
        with client.check("foreign org and team grants work while member lists and counts exclude metadata stubs"):
            deployment.web("alice", "form", "/settings/organizations", "--action", "/settings/organizations",
                "--has", "action=create", "--set", "slug=cell-org", "--set", "display_name=Cell organization",
                "--location", "/settings/organizations")
            client.api("orgs/cell-org/memberships/bob", "--method", "PUT", "-f", "role=member")
            membership = client.api("user/memberships/orgs/cell-org", "--method", "PATCH", "-f", "state=active", who="bob")
            assert membership["state"] == "active", membership
            deadline = time.monotonic() + 30
            while not any(org["login"] == "cell-org" for org in client.api("user/orgs", who="bob")):
                assert time.monotonic() < deadline, "the home org list did not receive the remote membership"
                threading.Event().wait(0.1)
            members = client.api("orgs/cell-org/members")
            assert [member["login"] for member in members] == ["alice"], members
            team = client.api("orgs/cell-org/teams", "--method", "POST", "-f", "name=Contributors", "-f", "privacy=closed")
            team_path = "orgs/cell-org/teams/" + team["slug"]
            for who, role in (("alice", "maintainer"), ("bob", "member")):
                member = client.api(team_path + "/memberships/" + who, "--method", "PUT", "-f", "role=" + role)
                assert member["state"] == "active", member
            members = client.api(team_path + "/members")
            assert [member["login"] for member in members] == ["alice"], members
            assert client.api(team_path)["members_count"] == 1
        with client.check("foreign private repository navigation works without JavaScript and through hydrated page models"):
            deployment.browser_navigation("bob", bob["id"], other["id"])

        client.git("push", deployment.base + "/bob/other.git", "main", who="bob", git_auth=True)
        client.git("push", f"ssh://git@127.0.0.1:{deployment.ssh}/bob/other.git", "main:ssh-proof",
                   who="bob", env=deployment.ssh_environment("bob"))
        for protocol, url in (("http", deployment.base + "/bob/other.git"),
                              ("ssh", f"ssh://git@127.0.0.1:{deployment.ssh}/bob/other.git")):
            own_clone = directory / ("cell-one-" + protocol)
            client.execute(["git", "clone", url, str(own_clone)], who="bob",
                           git_auth=protocol == "http", env=deployment.ssh_environment("bob") if protocol == "ssh" else None)
            assert (own_clone / "README.md").read_text() == "two cells\n"
            assert client.execute(["git", "-C", str(own_clone), "rev-parse", "HEAD"]).stdout.strip() == client.git("rev-parse", "main")
        client.checked("cell one scoped IDs survive gh and native repository pushes and clones over HTTP and SSH")
        client.git("checkout", "-b", "proposed")
        (client.clone / "README.md").write_text("two cells\nforeign SSH push\n")
        client.git("add", "README.md")
        client.git("commit", "-m", "Push from the other cell")
        client.git("push", f"ssh://git@127.0.0.1:{deployment.ssh}/alice/shared.git", "proposed", who="bob", env=deployment.ssh_environment("bob"))
        client.git("push", "origin", "proposed:http-proof", who="bob", git_auth=True)
        pull = client.gh("pr", "create", "-R", "alice/shared", "--base", "main", "--head", "proposed",
                         "--title", "Across cells", "--body", "Created by the collaborator in cell one", who="bob", cwd=client.clone)
        number = pull.stdout.strip().rstrip("/").rsplit("/", 1)[1]
        assert number.isdecimal(), pull.stdout
        client.gh("pr", "merge", number, "-R", "alice/shared", "--merge", who="bob")
        assert client.data("pr", "view", number, "-R", "alice/shared", "--json", "state", who="bob")["state"] == "MERGED"
        clone = directory / "ssh-clone"
        client.execute(["git", "clone", f"ssh://git@127.0.0.1:{deployment.ssh}/alice/shared.git", str(clone)],
                       who="bob", env=deployment.ssh_environment("bob"))
        assert "foreign SSH push" in (clone / "README.md").read_text()
        client.checked("foreign HTTP and SSH pushes, gh pr create and gh pr merge use live resource grants")
        with client.check("cross-cell fork creation"):
            cross_cell_fork(deployment, client, directory)
        with client.check("a cross-cell mention reaches gh api notifications within a minute"):
            client.gh("repo", "create", "alice/open", "--public")
            client.gh("issue", "create", "-R", "alice/open", "--title", "Cross-cell mention",
                      "--body", "Please look, @bob")
            deadline = time.monotonic() + 60
            while True:
                mentions = [thread for thread in client.api("notifications", who="bob")
                            if thread["reason"] == "mention" and thread["repository"]["full_name"] == "alice/open"]
                if mentions:
                    break
                assert time.monotonic() < deadline, "the mention did not reach the home inbox within 60 seconds"
                threading.Event().wait(0.5)
            assert len(mentions) == 1 and mentions[0]["subject"]["title"] == "Cross-cell mention", mentions
            client.api("repos/alice/open", "--method", "PATCH", "-F", "private=true")
            assert not any(thread["repository"]["full_name"] == "alice/open"
                           for thread in client.api("notifications", who="bob")), "a private repository leaked its delivered notice"
        client.gh("issue", "create", "-R", "alice/shared", "--title", "Router issue", "--body", "cross cell", who="bob")
        assert any(issue["title"] == "Router issue" for issue in client.data("issue", "list", "-R", "alice/shared", "--json", "title"))
        query = '{ zero:repository(owner:"alice",name:"shared"){ id name } one:repository(owner:"bob",name:"other"){ id name } }'
        answer = client.api("graphql", "--method", "POST", "--input", "-", input=json.dumps({"query": query}))
        assert not answer.get("errors"), answer
        assert answer["data"]["zero"]["name"] == "shared" and answer["data"]["one"]["name"] == "other", answer
        identities = {name: repository["id"] for name, repository in answer["data"].items()}
        nodes = client.api("graphql", "--method", "POST", "--input", "-", input=json.dumps({
            "query": 'query($zero:ID!,$one:ID!){zero:node(id:$zero){id ... on Repository{name}} one:node(id:$one){id ... on Repository{name}}}',
            "variables": identities}))
        assert not nodes.get("errors"), nodes
        assert nodes["data"]["zero"] == {"id": identities["zero"], "name": "shared"}, nodes
        assert nodes["data"]["one"] == {"id": identities["one"], "name": "other"}, nodes
        people = client.api("search/users?q=bob")
        assert people["total_count"] == 0 and people["items"] == [], "a local search must not expose metadata stubs"
        client.checked("node lookup and one native GraphQL query span both cells; stubs stay out of account search")

        def http_get(path, token=None, timeout=15):
            request = urllib.request.Request(deployment.base + path, headers={"Accept": "application/json", **({"Authorization": "Bearer " + token} if token else {})})
            try:
                response = deployment.http.open(request, timeout=timeout)
            except urllib.error.HTTPError as error:
                response = error
            with response:
                body = response.read(2 * 1024 * 1024 + 1)
                assert len(body) <= 2 * 1024 * 1024
                return response.status, body
        with client.check("gh search covers both cells within a minute, shows private items only to readers, "
                          "rebuilds from nothing and fails alone when the index stops"):
            global_search(deployment, client, tokens, http_get)
        deployment.stop("one")
        assert http_get("/api/v3/repos/bob/other", tokens["alice"])[0] == 503
        unavailable, body = http_get("/bob/other")
        assert unavailable == 503 and "cell is unavailable" in json.loads(body)["message"], body
        assert client.api("repos/alice/shared")["name"] == "shared"
        deployment.start("one", "serve")
        deployment.wait("one", deployment.native[1][0])
        assert client.api("repos/bob/other")["name"] == "other"
        client.checked("a stopped cell returns a clear unavailable error while the other cell keeps working")
        assert http_get("/api/v3/repos/alice/shared", tokens["bob"])[0] == 200
        token_id = deployment.web("bob", "req", "GET", "/settings?tab=tokens", "--print", r"re:/settings/tokens/([0-9]+)/delete")
        assert token_id.isdecimal()
        deployment.web("bob", "form", "/settings?tab=tokens", "--action", f"/settings/tokens/{token_id}/delete", "--set", "confirm=yes", "--status", "303")
        deadline = time.monotonic() + 60
        while True:
            remaining = deadline - time.monotonic()
            assert remaining > 0, "home token revocation did not reach the resource cell within 60 seconds"
            status, body = http_get("/api/v3/repos/alice/shared", tokens["bob"], timeout=min(15, remaining))
            assert status in (200, 401), f"revocation must deny authentication, got {status}: {body!r}"
            if status == 401:
                assert time.monotonic() <= deadline, "home token revocation exceeded 60 seconds"
                break
            threading.Event().wait(min(0.5, max(0, deadline - time.monotonic())))
        assert client.gh("issue", "create", "-R", "alice/shared", "--title", "Must not be written", "--body", "revoked", who="bob", ok=(1,)).returncode == 1
        assert all(issue["title"] != "Must not be written" for issue in client.data("issue", "list", "-R", "alice/shared", "--json", "title"))
        client.checked("revoked home token stops foreign reads and writes within the cache lifetime")
        with client.check("org presence and rollout waves through the operator commands"):
            presence_and_rollout(deployment, client)
        assert not client.failures, client.diagnostics


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--run", type=Path, required=True)
    parser.add_argument("--binary", type=Path, required=True)
    args = parser.parse_args()
    def interrupted(signum, _frame):
        raise SystemExit(128 + signum)
    signal.signal(signal.SIGTERM, interrupted)
    deployment = Deployment(args)
    try:
        deployment.prepare()
        acceptance(deployment)
    finally:
        deployment.close()


if __name__ == "__main__":
    main()
