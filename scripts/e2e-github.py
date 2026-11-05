#!/usr/bin/env python3
"""Unmodified GitHub CLI and github-script against e2e.sh's actual forge processes.

The CONNECT tunnel supplies a normal, trusted TLS GHES hostname without binding port 443
or changing host DNS. It forwards application bytes unchanged to the round's HTTP server.
No API responses, Git data, workflow results or webhook documents are mocked.
"""
import argparse
import base64
import contextlib
import concurrent.futures
import gzip
import hashlib
import hmac
import http.server
import json
import os
from pathlib import Path
import re
import secrets
import select
import signal
import shutil
import socket
import socketserver
import ssl
import subprocess
import threading
import time
import urllib.parse
import urllib.request
import urllib.error
import uuid

import jsonschema
from referencing import Registry

ROOT = Path(__file__).resolve().parent.parent
HOST = "codeyard-e2e.test"
REPO = "e2e-org/gh-client"
SECRET = "real-gh-client-private-value"
SCRIPT_SHA = "60a0d83039c74a4aee543508d2ffcb1c3799cdea"


class Tunnel(http.server.BaseHTTPRequestHandler):
    def log_message(self, *_):
        pass

    def do_CONNECT(self):
        if self.path != HOST + ":443":
            self.send_error(403)
            return
        self.send_response(200, "Connection established")
        self.end_headers()
        self.wfile.flush()
        self.close_connection = True
        with self.server.tls.wrap_socket(self.connection, server_side=True) as client:
            with socket.create_connection(self.server.upstream, timeout=120) as upstream:
                def upload():
                    try:
                        while data := client.recv(65536):
                            upstream.sendall(data)
                    except (OSError, ssl.SSLError):
                        pass
                    finally:
                        with contextlib.suppress(OSError):
                            upstream.shutdown(socket.SHUT_WR)

                worker = threading.Thread(target=upload)
                worker.start()
                try:
                    while data := upstream.recv(65536):
                        client.sendall(data)
                except (OSError, ssl.SSLError):
                    pass
                finally:
                    with contextlib.suppress(OSError):
                        client.shutdown(socket.SHUT_RDWR)
                    worker.join(timeout=5)
                    assert not worker.is_alive(), "TLS tunnel upload did not stop"


class Receiver(http.server.BaseHTTPRequestHandler):
    def log_message(self, *_):
        pass

    def do_POST(self):
        size = int(self.headers["Content-Length"])
        assert 0 < size <= 25 * 1024 * 1024
        payload = self.rfile.read(size)
        with self.server.lock:
            self.server.deliveries.append((dict(self.headers.items()), payload))
        self.send_response(204)
        self.end_headers()


@contextlib.contextmanager
def serving(server):
    worker = threading.Thread(target=server.serve_forever)
    worker.start()
    try:
        yield server
    finally:
        server.shutdown()
        server.server_close()
        worker.join(timeout=10)
        assert not worker.is_alive(), "test listener did not stop"


class Client:
    def __init__(self, base, directory, proxy, certificate, skip_runs=""):
        self.failures = []
        self.passes = []
        self.diagnostics = []
        # Why runs cannot finish on this host; checks that need a finished run are skipped.
        self.skip_runs = skip_runs
        self.skips = []
        self.command_number = 0
        self.log_lock = threading.Lock()
        self.base = base
        self.directory = directory
        self.clone = directory / "clone"
        self.credentials = {
            "alice": (directory.parent / "state-files/write_token").read_text(),
            "bob": (directory.parent / "state-files/gh_bob_token").read_text(),
        }
        self.env = {name: value for name, value in os.environ.items() if not name.startswith("GIT_")}
        for name in ("GH_TOKEN", "GITHUB_TOKEN", "GH_ENTERPRISE_TOKEN", "GITHUB_ENTERPRISE_TOKEN", "ALL_PROXY", "all_proxy"):
            self.env.pop(name, None)
        self.env.update({
            "GH_HOST": HOST, "GH_PROMPT_DISABLED": "1", "GH_NO_UPDATE_NOTIFIER": "1", "GH_DEBUG": "api",
            "GH_NO_EXTENSION_UPDATE_NOTIFIER": "1", "NO_COLOR": "1", "CLICOLOR": "0",
            "HTTPS_PROXY": proxy, "https_proxy": proxy,
            "NO_PROXY": "127.0.0.1,localhost", "no_proxy": "127.0.0.1,localhost",
            "SSL_CERT_FILE": str(certificate), "GIT_SSL_CAINFO": str(certificate),
            "GIT_TERMINAL_PROMPT": "0", "GIT_CONFIG_NOSYSTEM": "1",
            "GIT_CONFIG_GLOBAL": "/dev/null", "HOME": str(directory / "home"),
        })
        (directory / "home").mkdir()

    def execute(self, argv, *, who="alice", input=None, cwd=None, git_auth=False, env=None, ok=(0,)):
        env = dict(self.env, GH_CONFIG_DIR=str(self.directory / ("config-" + who)), **(env or {}))
        if git_auth:
            encoded = base64.b64encode((who + ":" + self.credentials[who]).encode()).decode()
            env.update(GIT_CONFIG_COUNT="1", GIT_CONFIG_KEY_0="http.extraHeader",
                       GIT_CONFIG_VALUE_0="Authorization: Basic " + encoded)
        result = subprocess.run(argv, input=input, text=True, capture_output=True, env=env,
                                cwd=cwd or ROOT, timeout=360)
        diagnostic = result.stdout + result.stderr
        for value in [*self.credentials.values(), SECRET]:
            diagnostic = diagnostic.replace(value, "[redacted]")
        with self.log_lock:
            self.command_number += 1
            log = self.directory / f"command-{self.command_number:03}.log"
        log.write_text(f"{' '.join(map(str, argv))}\nexit: {result.returncode}\n{diagnostic}")
        if result.returncode not in ok:
            # GH_DEBUG=api fills stderr with request traces; the error text is what is left.
            quiet = "\n".join(line for line in diagnostic.splitlines() if line.strip()[:1] not in ("*", ">", "<", "{", "}", "[", "]", '"', ","))
            raise AssertionError(f"{' '.join(map(str, argv))} failed ({result.returncode}; {log}):\n{quiet}")
        return result

    def gh(self, *args, **kwargs):
        return self.execute(["gh", *map(str, args)], **kwargs)

    def data(self, *args, **kwargs):
        result = self.gh(*args, **kwargs)
        return json.loads(result.stdout)

    def api(self, path, *args, **kwargs):
        return self.data("api", path, *args, **kwargs)

    def git(self, *args, **kwargs):
        return self.execute(["git", *map(str, args)], cwd=self.clone, **kwargs).stdout.strip()

    def native(self, method, path, *args):
        result = self.execute(["python3", "-I", ROOT / "scripts/e2e-web.py", "req", "--base", self.base,
                               "--jar", self.directory.parent / "jar-alice.json", "--who", "alice",
                               method, path, "--accept", "json", "--print", "body", *args])
        return json.loads(result.stdout)

    def page(self, path):
        result = self.execute(["python3", "-I", ROOT / "scripts/e2e-web.py", "req", "--base", self.base,
                               "--jar", self.directory.parent / "jar-alice.json", "--who", "alice",
                               "GET", path, "--print", "body"])
        return result.stdout

    @contextlib.contextmanager
    def check(self, name):
        try:
            yield
        except (AssertionError, KeyError, IndexError, StopIteration, AttributeError, ValueError, subprocess.SubprocessError, OSError) as error:
            self.failures.append(name)
            diagnostic = str(error)
            for value in [*self.credentials.values(), SECRET]:
                diagnostic = diagnostic.replace(value, "[redacted]")
            lines = [line.strip() for line in diagnostic.splitlines() if line.strip()]
            self.diagnostics.append(f"{name}: {' | '.join(lines[:4] + ['...'] + lines[-4:] if len(lines) > 8 else lines) if lines else type(error).__name__}")
            print(f"FAIL real-client {name}: {diagnostic}", flush=True)
        else:
            self.checked(name)

    def checked(self, name):
        self.passes.append(name)
        print("PASS real-client " + name, flush=True)

    def skipped(self, name):
        self.skips.append(name)
        print(f"SKIP real-client {name}: {self.skip_runs}", flush=True)

    def url(self, result, path):
        value = result.stdout.strip()
        assert urllib.parse.urlsplit(value).path == path, (value, path)
        assert urllib.parse.urlsplit(value).scheme in ("http", "https"), value

    def login(self):
        for who, token in self.credentials.items():
            result = self.gh("auth", "login", "--hostname", HOST, "--with-token",
                             "--git-protocol", "https", "--insecure-storage", input=token + "\n", who=who)
            assert result.stdout.strip() == "", result.stdout
            status = self.gh("auth", "status", "--hostname", HOST, who=who)
            assert who in status.stdout + status.stderr
            assert "Logged in" in status.stdout + status.stderr
            assert self.api("user", who=who)["login"] == who
            # Absolute upload URLs advertise the round's HTTP origin, not the TLS tunnel.
            # Trust only that second test-owned origin with this same stored credential.
            hosts = self.directory / ("config-" + who) / "hosts.yml"
            with hosts.open("a") as config:
                config.write(f"{urllib.parse.urlsplit(self.base).hostname}:\n    oauth_token: {token.strip()}\n    user: {who}\n")
        self.checked("gh auth login/status with stored GHES tokens")

    def repositories(self):
        with self.check("gh repo create"):
            result = self.gh("repo", "create", REPO, "--public", "--description", "Real GitHub CLI repository")
            self.url(result, "/" + REPO)
        if "gh repo create" in self.failures:
            # Real REST fixtures let independent commands run; the failed gh check stays failed.
            try:
                self.api(f"repos/{REPO}")
            except AssertionError:
                print("FIXTURE real-client repository via REST after gh repo create failed", flush=True)
                self.api("orgs/e2e-org/repos", "--method", "POST", "-f", "name=gh-client",
                         "-f", "description=Real GitHub CLI repository")
        with self.check("gh repo view"):
            repository = self.data("repo", "view", HOST + "/" + REPO, "--json",
                                   "nameWithOwner,description,isPrivate,viewerPermission")
            assert repository == {"nameWithOwner": REPO, "description": "Real GitHub CLI repository",
                                  "isPrivate": False, "viewerPermission": "ADMIN"}, repository
        with self.check("gh repo clone"):
            result = self.gh("repo", "clone", HOST + "/" + REPO, self.clone)
            assert "Cloning into" in result.stdout + result.stderr
            assert self.git("rev-parse", "--is-inside-work-tree") == "true"
            assert urllib.parse.urlsplit(self.git("remote", "get-url", "origin")).path.removesuffix(".git") == "/" + REPO
        if not (self.clone / ".git").exists():
            print("FIXTURE real-client Git clone after gh repo clone failed", flush=True)
            self.execute(["git", "clone", self.api(f"repos/{REPO}")["clone_url"], self.clone])
        self.git("checkout", "-B", "main")
        self.git("config", "user.name", "alice")
        self.git("config", "user.email", "alice@example.test")
        (self.clone / "README.md").write_text("Real gh clone and push\n")
        self.git("add", "README.md")
        self.git("commit", "-m", "Seed real client repository")
        self.git("push", "-u", "origin", "main", git_auth=True)
        assert self.git("show", "origin/main:README.md") == "Real gh clone and push"
        with self.check("gh repo list"):
            listed = self.data("repo", "list", "e2e-org", "--json", "nameWithOwner,isPrivate")
            assert {"nameWithOwner": REPO, "isPrivate": False} in listed, listed
        with self.check("organization reviewer fixture grants team write access"):
            self.api("orgs/e2e-org/memberships/bob", "--method", "PUT", "-f", "role=member")
            self.api("user/memberships/orgs/e2e-org", "--method", "PATCH", "-f", "state=active", who="bob")
            team = self.api("orgs/e2e-org/teams", "--method", "POST", "-f", "name=gh-reviewers", "-f", "privacy=closed")
            team_path = "orgs/e2e-org/teams/" + team["slug"]
            self.api(team_path + "/memberships/bob", "--method", "PUT", "-f", "role=member")
            self.gh("api", team_path + "/repos/" + REPO, "--method", "PUT", "-f", "permission=push")
            assert self.api(f"repos/{REPO}", who="bob")["permissions"]["push"] is True
        with self.check("gh repo view as collaborator"):
            assert self.data("repo", "view", HOST + "/" + REPO, "--json", "viewerPermission", who="bob")["viewerPermission"] == "WRITE"

    def web_loss(self):
        value = os.environ.get("E2E_STOP_WEB_PID", "")
        if not value:
            return
        pid = int(value)
        assert pid > 1
        with self.check("gh and Git survive losing the other web node"):
            acknowledged = threading.Event()
            lost = threading.Event()
            def issues():
                numbers = []
                for index in range(12):
                    result = self.gh("issue", "create", "-R", REPO, "--title",
                                     f"Web loss acknowledged {index}", "--body", "Survives web1")
                    numbers.append(int(result.stdout.strip().rstrip("/").rsplit("/", 1)[1]))
                    acknowledged.set()
                    if index == 0:
                        assert lost.wait(timeout=65), "web-loss action did not finish"
                return numbers
            def clones():
                for index in range(3):
                    path = self.directory / f"web-loss-clone-{index}"
                    self.git("clone", "--quiet", f"{self.base}/{REPO}.git", str(path), git_auth=True)
                    self.git("-C", path, "fsck", "--full")
                    if index == 0:
                        assert lost.wait(timeout=65), "web-loss action did not finish"
            with concurrent.futures.ThreadPoolExecutor(max_workers=2) as workers:
                writes = workers.submit(issues)
                reads = workers.submit(clones)
                try:
                    assert acknowledged.wait(timeout=60), "gh did not acknowledge a write before web loss"
                    os.kill(pid, signal.SIGKILL)
                finally:
                    lost.set()
                numbers = writes.result(timeout=180)
                reads.result(timeout=180)
            assert len(set(numbers)) == 12
            for index, number in enumerate(numbers):
                issue = self.api(f"repos/{REPO}/issues/{number}")
                assert issue["title"] == f"Web loss acknowledged {index}"
                assert issue["body"] == "Survives web1"

    def issues(self):
        with self.check("gh issue create"):
            result = self.gh("issue", "create", "-R", REPO, "--title", "Real gh issue", "--body", "Created with gh")
            self.issue = int(result.stdout.strip().rstrip("/").rsplit("/", 1)[1])
            self.url(result, f"/{REPO}/issues/{self.issue}")
        if not hasattr(self, "issue"):
            print("FIXTURE real-client issue via REST after gh issue create failed", flush=True)
            self.issue = self.api(f"repos/{REPO}/issues", "--method", "POST", "-f", "title=Real gh issue",
                                  "-f", "body=Created with gh")["number"]
        with self.check("gh issue list"):
            listed = self.data("issue", "list", "-R", REPO, "--json", "number,title,state")
            assert {"number": self.issue, "title": "Real gh issue", "state": "OPEN"} in listed, listed
        with self.check("gh issue view"):
            detail = self.data("issue", "view", self.issue, "-R", REPO, "--json", "number,title,body,state")
            assert detail == {"number": self.issue, "title": "Real gh issue", "body": "Created with gh", "state": "OPEN"}, detail
        with self.check("gh issue comment"):
            self.url(self.gh("issue", "comment", self.issue, "-R", REPO, "--body", "Real gh comment"),
                     f"/{REPO}/issues/{self.issue}")
            comments = self.api(f"repos/{REPO}/issues/{self.issue}/comments")
            assert any(row["body"] == "Real gh comment" and row["user"]["login"] == "alice" for row in comments), comments
        with self.check("gh issue close"):
            closed = self.gh("issue", "close", self.issue, "-R", REPO)
            assert closed.stdout.strip() == "" or "Closed" in closed.stdout
            assert self.api(f"repos/{REPO}/issues/{self.issue}")["state"] == "closed"

    def pulls(self):
        self.git("checkout", "-b", "gh-feature")
        (self.clone / "feature.txt").write_text("Real gh pull request\n")
        self.git("add", "feature.txt")
        self.git("commit", "-m", "Real gh feature")
        self.git("push", "-u", "origin", "gh-feature", git_auth=True)
        with self.check("gh pr create"):
            result = self.gh("pr", "create", "-R", REPO, "--head", "gh-feature", "--base", "main",
                             "--title", "Real gh pull", "--body", "Created with the GitHub CLI", cwd=self.clone)
            self.pull = int(result.stdout.strip().rstrip("/").rsplit("/", 1)[1])
            self.url(result, f"/{REPO}/pull/{self.pull}")
        if not hasattr(self, "pull"):
            print("FIXTURE real-client pull request via REST after gh pr create failed", flush=True)
            self.pull = self.api(f"repos/{REPO}/pulls", "--method", "POST", "-f", "head=gh-feature", "-f", "base=main",
                                 "-f", "title=Real gh pull", "-f", "body=Created with the GitHub CLI")["number"]
        with self.check("gh pr list"):
            listed = self.data("pr", "list", "-R", REPO, "--json", "number,title,state,headRefName")
            assert {"number": self.pull, "title": "Real gh pull", "state": "OPEN", "headRefName": "gh-feature"} in listed, listed
        with self.check("gh pr view"):
            detail = self.data("pr", "view", self.pull, "-R", REPO, "--json", "number,title,body,state,baseRefName,headRefName")
            assert detail == {"number": self.pull, "title": "Real gh pull", "body": "Created with the GitHub CLI", "state": "OPEN",
                              "baseRefName": "main", "headRefName": "gh-feature"}, detail
        self.git("checkout", "main")
        with self.check("gh pr checkout"):
            checkout = self.gh("pr", "checkout", self.pull, "-R", REPO, cwd=self.clone)
            assert checkout.stdout.strip() == "" or "gh-feature" in checkout.stdout + checkout.stderr
            assert self.git("branch", "--show-current") == "gh-feature"
            assert (self.clone / "feature.txt").read_text() == "Real gh pull request\n"
        with self.check("gh pr review"):
            reviewed = self.gh("pr", "review", self.pull, "-R", REPO, "--approve", "--body", "Real gh approval", who="bob")
            assert reviewed.stdout.strip() == "" or "Approved" in reviewed.stdout
            reviews = self.api(f"repos/{REPO}/pulls/{self.pull}/reviews")
            assert any(row["state"] == "APPROVED" and row["body"] == "Real gh approval" and row["user"]["login"] == "bob" for row in reviews), reviews
        with self.check("gh pr merge with verified published Git tree"):
            merged = self.gh("pr", "merge", self.pull, "-R", REPO, "--merge", "--subject", "Merge real gh pull", "--body", "Real gh merge")
            assert merged.stdout.strip() == "" or "Merged" in merged.stdout
            assert self.api(f"repos/{REPO}/pulls/{self.pull}")["merged"] is True
            self.git("fetch", "origin", "main", git_auth=True)
            assert self.git("show", "origin/main:feature.txt") == "Real gh pull request"

    def releases(self):
        asset = self.directory / "proof.txt"
        asset.write_bytes(b"real gh release asset\x00\xff\n")
        with self.check("gh release create and asset upload"):
            self.url(self.gh("release", "create", "v1.0.0", asset, "-R", REPO, "--target", "main", "--title", "Real gh release", "--notes", "Real release notes"),
                     f"/{REPO}/releases/tag/v1.0.0")
        with self.check("gh release list"):
            listed = self.data("release", "list", "-R", REPO, "--json", "tagName,name,isDraft")
            assert {"tagName": "v1.0.0", "name": "Real gh release", "isDraft": False} in listed, listed
        with self.check("gh release view"):
            detail = self.data("release", "view", "v1.0.0", "-R", REPO, "--json", "tagName,name,body,assets")
            assert detail["tagName"] == "v1.0.0" and detail["name"] == "Real gh release" and detail["body"] == "Real release notes", detail
            assert any(row["name"] == "proof.txt" and row["size"] == asset.stat().st_size for row in detail["assets"]), detail
        with self.check("gh release download with byte-for-byte asset comparison"):
            downloaded = self.directory / "downloads"
            result = self.gh("release", "download", "v1.0.0", "-R", REPO, "--pattern", "proof.txt", "--dir", downloaded)
            assert result.stdout.strip() == "", result.stdout
            assert (downloaded / "proof.txt").read_bytes() == asset.read_bytes()

    def apis_and_values(self):
        with self.check("gh api REST"):
            assert self.gh("api", f"repos/{REPO}", "--jq", ".full_name").stdout.strip() == REPO
        with self.check("gh api graphql"):
            data = self.api("graphql", "-f", 'query=query { viewer { login } repository(owner:"e2e-org",name:"gh-client") { nameWithOwner } }')
            assert "errors" not in data and data["data"] == {"viewer": {"login": "alice"}, "repository": {"nameWithOwner": REPO}}, data
        with self.check("gh secret set"):
            result = self.gh("secret", "set", "CLIENT_TOKEN", "-R", REPO, input=SECRET)
            assert result.stdout.strip() == "", result.stdout
        with self.check("gh secret list using the server public key"):
            entries = self.data("secret", "list", "-R", REPO, "--json", "name,updatedAt")
            assert any(row["name"] == "CLIENT_TOKEN" and row["updatedAt"] for row in entries), entries
            assert SECRET not in json.dumps(entries)
        with self.check("gh variable set"):
            result = self.gh("variable", "set", "REGION", "-R", REPO, "--body", "e2e-visible")
            assert result.stdout.strip() == "", result.stdout
        with self.check("gh variable list"):
            entries = self.data("variable", "list", "-R", REPO, "--json", "name,value")
            assert {"name": "REGION", "value": "e2e-visible"} in entries, entries

    def workflows(self):
        self.git("checkout", "main")
        self.git("pull", "--ff-only", "origin", "main", git_auth=True)
        workflow = self.clone / ".github/workflows/client.yml"
        workflow.parent.mkdir(parents=True)
        workflow.write_text(f'''name: Real github-script client
on:
  push:
  workflow_dispatch:
    inputs:
      message:
        description: Evidence marker
        required: true
        type: string
permissions:
  contents: read
  issues: write
jobs:
  script:
    runs-on: self-hosted
    steps:
      - name: Check authenticated REST and GraphQL clients
        uses: actions/github-script@{SCRIPT_SHA}
        env:
          CLIENT_SECRET: ${{{{ secrets.CLIENT_TOKEN }}}}
          CLIENT_REGION: ${{{{ vars.REGION }}}}
        with:
          script: |
            const assert = require('node:assert/strict');
            assert.equal(require('node:crypto').createHash('sha256').update(process.env.CLIENT_SECRET).digest('hex'),
              '{hashlib.sha256(SECRET.encode()).hexdigest()}');
            assert.equal(process.env.CLIENT_REGION, 'e2e-visible');
            const repo = await github.rest.repos.get(context.repo);
            assert.equal(repo.data.full_name, '{REPO}');
            core.info('GH_SCRIPT_REST_OK ' + repo.data.full_name);
            const data = await github.graphql(`query($owner:String!, $repo:String!) {{
              viewer {{ login }} repository(owner:$owner, name:$repo) {{ nameWithOwner }}
            }}`, context.repo);
            assert.equal(data.viewer.login, 'github-actions[bot]');
            assert.equal(data.repository.nameWithOwner, '{REPO}');
            core.info('GH_SCRIPT_GRAPHQL_OK ' + data.viewer.login);
            const comment = await github.rest.issues.createComment({{...context.repo,
              issue_number: {self.issue}, body: 'github-script job token: ' + (context.payload.inputs?.message ?? 'push-client-proof')}});
            assert.equal(comment.data.user.login, 'github-actions[bot]');
            core.info('GH_SCRIPT_WRITE_OK ' + comment.data.id);
''')
        self.git("add", ".github/workflows/client.yml")
        self.git("commit", "-m", "Run real github-script against Codeyard")
        self.git("push", "origin", "main", git_auth=True)
        script_check = "actions/github-script on Codeyard with GITHUB_TOKEN"
        with self.check("github-script push queues a run" if self.skip_runs else script_check):
            deadline = time.monotonic() + 330
            while True:
                runs = self.native("GET", "/api/ci/jobs?owner=e2e-org&repo=gh-client")["jobs"]
                run = next((row for row in runs if row["head_sha"] == self.git("rev-parse", "HEAD") and row["event"] == "push"), None)
                if run:
                    (self.directory.parent / "state-files/gh_run_id").write_text(run["id"])
                    if self.skip_runs or run["status"] in ("success", "failed", "cancelled"):
                        break
                assert time.monotonic() < deadline, "github-script push run did not finish"
                time.sleep(1)
            if not self.skip_runs:
                text = self.page(f"/e2e-org/gh-client/actions?job={run['id']}&view=logs&log_limit=5000")
                assert SECRET not in text, "workflow logs leaked the stored secret"
                assert run["status"] == "success", text
                for marker in ("GH_SCRIPT_REST_OK " + REPO, "GH_SCRIPT_GRAPHQL_OK github-actions[bot]", "GH_SCRIPT_WRITE_OK"):
                    assert marker in text, (marker, text)
                comments = self.api(f"repos/{REPO}/issues/{self.issue}/comments")
                assert any(row["body"] == "github-script job token: push-client-proof" and row["user"]["login"] == "github-actions[bot]" for row in comments), comments
        if self.skip_runs:
            self.skipped(script_check)
        with self.check("gh workflow list"):
            workflows = self.data("workflow", "list", "-R", REPO, "--json", "id,name,path,state")
            assert any(row["path"] == ".github/workflows/client.yml" and row["name"] == "Real github-script client" and row["state"] == "active" for row in workflows), workflows
        with self.check("gh workflow run"):
            result = self.gh("workflow", "run", "client.yml", "-R", REPO, "--ref", "main", "-f", "message=real-client-proof")
            assert result.stdout.strip() == "" or "Created" in result.stdout or "/actions/runs/" in result.stdout, result.stdout
        expected_event = "push" if "gh workflow run" in self.failures else "workflow_dispatch"
        with self.check("gh run list"):
            deadline = time.monotonic() + 120
            while True:
                runs = self.data("run", "list", "-R", REPO, "--json", "databaseId,event,headSha,workflowName,status,conclusion")
                run = next((row for row in runs if row["event"] == expected_event), None)
                if run:
                    break
                assert time.monotonic() < deadline, f"{expected_event} never queued a run"
                time.sleep(1)
            assert run["workflowName"] == "Real github-script client", run
            assert run["headSha"] == self.git("rev-parse", "HEAD"), run
            self.run_id = run["databaseId"]
        if not hasattr(self, "run_id"):
            print("BLOCKED real-client gh run view/watch/log: gh run list returned no usable run ID", flush=True)
            return
        if self.skip_runs:
            for name in ("gh run watch", "gh run view", "gh run view --log",
                         "actions/github-script on Codeyard: real job-token REST read/write, GraphQL, secrets and vars"):
                self.skipped(name)
            return
        with self.check("gh run watch"):
            watched = self.gh("run", "watch", self.run_id, "-R", REPO, "--exit-status", "--interval", "1")
            assert "completed" in (watched.stdout + watched.stderr).lower(), watched.stdout + watched.stderr
        with self.check("gh run view"):
            detail = self.data("run", "view", self.run_id, "-R", REPO, "--json", "databaseId,event,status,conclusion,jobs")
            assert detail["databaseId"] == self.run_id and detail["event"] == expected_event, detail
            assert detail["status"] == "completed" and detail["conclusion"] == "success", detail
            assert detail["jobs"] and all(job["conclusion"] == "success" for job in detail["jobs"]), detail
        logs = self.gh("run", "view", self.run_id, "-R", REPO, "--log").stdout
        for marker in ("GH_SCRIPT_REST_OK " + REPO, "GH_SCRIPT_GRAPHQL_OK github-actions[bot]", "GH_SCRIPT_WRITE_OK"):
            assert marker in logs, (marker, logs)
        assert SECRET not in logs, "workflow logs leaked the stored secret"
        comments = self.api(f"repos/{REPO}/issues/{self.issue}/comments")
        marker = "push-client-proof" if expected_event == "push" else "real-client-proof"
        assert any(row["body"] == "github-script job token: " + marker and row["user"]["login"] == "github-actions[bot]" for row in comments), comments
        self.checked("actions/github-script on Codeyard: real job-token REST read/write, GraphQL, secrets and vars")

    # Every other gh command a person would expect to work, run unmodified against the server.
    def step(self, name, *args, expect=None, **kwargs):
        """Run one gh command as its own check; returns the result, or None when it failed."""
        with self.check(name):
            result = self.gh(*args, **kwargs)
            if expect is not None:
                assert expect in result.stdout + result.stderr, (expect, result.stdout, result.stderr)
            return result
        return None

    def postgres_failover(self):
        if os.environ.get("E2E_CHECK_PG_RESTART") != "1":
            return
        data = os.environ.get("GITSERV_TEST_POSTGRES_DATA")
        if not data:
            assert os.environ.get("E2E_REQUIRE_PG_RESTART") != "1", "owned PostgreSQL restart fixture is required"
            return
        data = Path(data).resolve()
        marker = os.environ.get("GITSERV_TEST_POSTGRES_MARKER", "")
        assert marker and (data.parent / ".codeyard-owned-postgres").read_text().strip() == marker, "PostgreSQL is not owned by this test"
        options = os.environ["GITSERV_TEST_POSTGRES_OPTIONS"]
        pg_ctl = shutil.which("pg_ctl")
        assert pg_ctl, "pg_ctl is required for the owned crash test"
        with self.check("PostgreSQL crash during gh issue create and pool recovery"):
            before = self.gh("issue", "create", "-R", REPO, "--title", "Acknowledged before PostgreSQL crash", "--body", "durable").stdout.strip()
            results = []
            worker = threading.Thread(target=lambda: results.append(self.gh("issue", "create", "-R", REPO,
                "--title", "Concurrent PostgreSQL crash", "--body", "in flight", ok=(0, 1))))
            worker.start()
            stopped = False
            try:
                subprocess.run([pg_ctl, "-D", str(data), "-m", "immediate", "-w", "-t", "60", "stop"], check=True, capture_output=True, timeout=65)
                stopped = True
                try:
                    urllib.request.urlopen(self.base + "/ready", timeout=5)
                except urllib.error.HTTPError as error:
                    assert error.code == 503
                else:
                    raise AssertionError("readiness did not fail while PostgreSQL was stopped")
            finally:
                if stopped:
                    subprocess.run([pg_ctl, "-D", str(data), "-l", str(data.parent / "restart.log"), "-o", options, "-w", "-t", "60", "start"], check=True, capture_output=True, timeout=65)
                worker.join(timeout=120)
                assert not worker.is_alive(), "gh did not stop after the database crash"
            assert results, "the concurrent gh request did not return"
            deadline = time.monotonic() + 60
            while True:
                try:
                    with urllib.request.urlopen(self.base + "/ready", timeout=5) as response:
                        assert response.status == 200
                    break
                except (urllib.error.URLError, TimeoutError):
                    assert time.monotonic() < deadline, "readiness did not recover without restarting Codeyard"
            previous = before.rstrip("/").rsplit("/", 1)[1]
            assert self.gh("issue", "view", previous, "-R", REPO, "--json", "title", "--jq", ".title").stdout.strip() == "Acknowledged before PostgreSQL crash"
            if results[0].returncode == 0:
                number = results[0].stdout.strip().rstrip("/").rsplit("/", 1)[1]
                assert self.gh("issue", "view", number, "-R", REPO, "--json", "title", "--jq", ".title").stdout.strip() == "Concurrent PostgreSQL crash"
            self.gh("issue", "create", "-R", REPO, "--title", "Pool recovered without restart", "--body", "after crash")

    def sweep_account(self):
        self.step("gh auth token", "auth", "token", "--hostname", HOST, expect=self.credentials["alice"].strip())
        self.step("gh auth status --show-token", "auth", "status", "--hostname", HOST, "--show-token", expect="Logged in")
        self.step("gh config set", "config", "set", "git_protocol", "https")
        self.step("gh config get", "config", "get", "git_protocol", expect="https")
        self.step("gh config list", "config", "list", expect="git_protocol")
        self.step("gh config clear-cache", "config", "clear-cache")
        self.step("gh org list", "org", "list")
        self.step("gh status", "status")
        key = self.directory / "sweep-key"
        subprocess.run(["ssh-keygen", "-q", "-t", "ed25519", "-N", "", "-f", str(key)], check=True, timeout=30)
        self.step("gh ssh-key add", "ssh-key", "add", str(key) + ".pub", "--title", "sweep-key")
        with self.check("gh ssh-key list and delete"):
            keys = self.gh("ssh-key", "list").stdout
            assert "sweep-key" in keys, keys
            sweep = next(key for key in self.api("user/keys") if key["title"] == "sweep-key")
            self.gh("ssh-key", "delete", sweep["id"], "--yes")
        with self.check("gh gpg-key add, list and delete"):
            armored = ROOT / "scripts/e2e-gpg-public.asc"
            self.gh("gpg-key", "add", armored, "--title", "sweep gpg")
            listed = self.gh("gpg-key", "list").stdout
            assert "67AE4AD72C1192CC" in listed, listed
            self.gh("gpg-key", "delete", self.api("user/gpg_keys")[0]["key_id"], "--yes")
        with self.check("gh gist create, list, view, edit, rename and delete"):
            note = self.directory / "note.txt"
            note.write_text("gist one\n")
            created = self.gh("gist", "create", note, "--public", "--desc", "sweep gist").stdout.strip()
            gist = created.rstrip("/").rsplit("/", 1)[1]
            assert "sweep gist" in self.gh("gist", "list").stdout
            assert "gist one" in self.gh("gist", "view", gist, "--raw").stdout
            extra = self.directory / "more.txt"
            extra.write_text("more\n")
            self.gh("gist", "edit", gist, "--add", extra)
            self.gh("gist", "rename", gist, "more.txt", "renamed.txt")
            assert "renamed.txt" in self.gh("gist", "view", gist, "--files").stdout
            self.step("gh gist clone", "gist", "clone", gist, str(self.directory / "gist-clone"))
            self.gh("gist", "delete", gist, "--yes")

    def sweep_fine_grained(self):
        """A fine-grained token made on the settings page: gh logs in with it and is bounded by it."""
        org = self.api("orgs/e2e-org")["id"]
        # Run directly, not through execute, so the token is never written to a command log.
        created = subprocess.run(
            ["python3", "-I", ROOT / "scripts/e2e-web.py", "form", "--base", self.base,
             "--jar", self.directory.parent / "jar-alice.json", "--who", "alice",
             "--action", "/settings/personal-access-tokens", "--has", "name",
             "--set", "name=gh fine-grained", "--set", f"owner={org}", "--set", "access=all",
             "--set", "permission.issues=read", "--submit", "action=create",
             "--status", "200", "--print", "body", "/settings/personal-access-tokens/new"],
            text=True, capture_output=True, env=self.env, cwd=ROOT, timeout=120)
        found = re.search(r"cy_fgpat_[0-9a-f]{64}", created.stdout)
        assert created.returncode == 0 and found, created.stderr[-2000:]
        self.credentials["fine"] = found.group(0)
        self.gh("auth", "login", "--hostname", HOST, "--with-token", "--insecure-storage",
                input=self.credentials["fine"] + "\n", who="fine")
        status = self.gh("auth", "status", "--hostname", HOST, who="fine")
        assert "Logged in" in status.stdout + status.stderr, status.stdout + status.stderr
        assert self.api("user", who="fine")["login"] == "alice"
        with self.check("gh with a fine-grained token reads what it grants"):
            assert isinstance(self.api(f"repos/{REPO}/issues", who="fine"), list)
            assert isinstance(self.data("issue", "list", "--repo", HOST + "/" + REPO, "--json", "number", who="fine"), list)
        with self.check("gh with a fine-grained token is refused what it does not grant"):
            denied = self.gh("api", f"repos/{REPO}/issues", "--method", "POST", "-f", "title=Denied",
                             "--include", who="fine", ok=(1,))
            output = (denied.stdout + denied.stderr).lower()
            assert "403" in output and "x-accepted-github-permissions: issues=write" in output, output[-2000:]
            refused = self.gh("api", "orgs/e2e-org/personal-access-tokens", who="fine", ok=(1,))
            assert "403" in refused.stdout + refused.stderr
        with self.check("organization owners list fine-grained token grants"):
            grants = self.api("orgs/e2e-org/personal-access-tokens")
            assert any(grant["token_name"] == "gh fine-grained" for grant in grants), grants

    def sweep_repositories(self):
        self.step("gh repo view text", "repo", "view", REPO, expect="Real GitHub CLI repository")
        self.step("gh repo edit", "repo", "edit", REPO, "--description", "Edited by gh", "--homepage",
                  "https://example.test", "--add-topic", "sweep", "--enable-wiki=false")
        with self.check("gh repo edit took effect"):
            detail = self.data("repo", "view", REPO, "--json", "description,homepageUrl,repositoryTopics")
            rest = self.api(f"repos/{REPO}")
            assert detail["description"] == "Edited by gh" and detail["homepageUrl"].rstrip("/") == "https://example.test", (detail, rest["description"], rest["homepage"])
        self.step("gh repo list by owner", "repo", "list", "alice", "--json", "name")
        self.step("gh repo set-default", "repo", "set-default", REPO, cwd=self.clone)
        self.step("gh repo gitignore list", "repo", "gitignore", "list")
        self.step("gh repo license list", "repo", "license", "list")
        self.step("gh repo read-dir", "repo", "read-dir", "-R", REPO, expect="README.md")
        self.step("gh repo read-file", "repo", "read-file", "README.md", "-R", REPO, expect="Real gh clone and push")
        with self.check("gh repo deploy-key add, list and delete"):
            key = self.directory / "deploy-key"
            subprocess.run(["ssh-keygen", "-q", "-t", "ed25519", "-N", "", "-f", str(key)], check=True, timeout=30)
            self.gh("repo", "deploy-key", "add", str(key) + ".pub", "-R", REPO, "--title", "sweep deploy")
            assert "sweep deploy" in self.gh("repo", "deploy-key", "list", "-R", REPO).stdout
            mine = next(row for row in self.api(f"repos/{REPO}/keys") if row["title"] == "sweep deploy")
            self.gh("repo", "deploy-key", "delete", mine["id"], "-R", REPO)
        with self.check("gh repo autolink create, list, view and delete"):
            self.gh("repo", "autolink", "create", "-R", REPO, "SWEEP-", "https://example.test/<num>")
            assert "SWEEP-" in self.gh("repo", "autolink", "list", "-R", REPO).stdout
            identifier = self.api(f"repos/{REPO}/autolinks")[0]["id"]
            self.gh("repo", "autolink", "view", identifier, "-R", REPO)
            self.gh("repo", "autolink", "delete", identifier, "-R", REPO, "--yes")
        with self.check("gh label create, list, edit, clone and delete"):
            self.gh("label", "create", "sweep", "-R", REPO, "--color", "ff0000", "--description", "first")
            assert "sweep" in self.gh("label", "list", "-R", REPO).stdout
            self.gh("label", "edit", "sweep", "-R", REPO, "--name", "sweep2", "--description", "second")
            self.gh("repo", "create", "e2e-org/gh-labels", "--public")
            self.gh("label", "clone", REPO, "-R", "e2e-org/gh-labels")
            assert "sweep2" in self.gh("label", "list", "-R", "e2e-org/gh-labels").stdout
            self.gh("label", "delete", "sweep2", "-R", REPO, "--yes")
        with self.check("gh repo fork and sync as bob"):
            self.gh("repo", "fork", REPO, "--clone=false", "--fork-name", "gh-client-fork", who="bob")
            assert self.api("repos/bob/gh-client-fork", who="bob")["fork"] is True
            self.gh("repo", "sync", "bob/gh-client-fork", who="bob")
        with self.check("gh repo rename, archive, unarchive and delete"):
            self.gh("repo", "create", "e2e-org/gh-temp", "--public")
            self.gh("repo", "rename", "gh-temp2", "-R", "e2e-org/gh-temp", "--yes")
            self.gh("repo", "archive", "e2e-org/gh-temp2", "--yes")
            assert self.api("repos/e2e-org/gh-temp2")["archived"] is True
            self.gh("repo", "unarchive", "e2e-org/gh-temp2", "--yes")
            self.gh("repo", "delete", "e2e-org/gh-temp2", "--yes")
        self.step("gh ruleset list", "ruleset", "list", "-R", REPO)
        with self.check("gh ruleset view"):
            made = self.api(f"repos/{REPO}/rulesets", "--method", "POST", "--input", "-", input=json.dumps({
                "name": "sweep", "target": "branch", "enforcement": "evaluate",
                "conditions": {"ref_name": {"include": ["~DEFAULT_BRANCH"], "exclude": []}},
                "rules": [{"type": "deletion"}]}))
            assert "sweep" in self.gh("ruleset", "view", made["id"], "-R", REPO).stdout
        self.step("gh ruleset check", "ruleset", "check", "main", "-R", REPO)
        self.step("gh browse", "browse", "--no-browser", "-R", REPO, expect=f"/{REPO}")
        self.step("gh api paginate", "api", f"repos/{REPO}/commits", "--paginate", "--jq", ".[].sha")
        self.step("gh api include", "api", f"repos/{REPO}", "--include", expect="200")
        for kind, query, field, expected in [
            ("repos", "gh-client", "fullName", REPO),
            ("issues", "Real gh issue", "number", self.issue),
            ("prs", "Real gh pull", "number", self.pull),
            ("commits", "feature", "sha", self.git("rev-parse", "gh-feature")),
            ("code", "Real", "path", "README.md"),
        ]:
            with self.check(f"gh search {kind}"):
                scope = [] if kind == "repos" else ["--repo", REPO]
                rows = self.data("search", kind, query, *scope, "--json", field)
                assert any(row[field] == expected for row in rows), rows

    def sweep_issues(self):
        issue = self.issue
        self.step("gh issue reopen", "issue", "reopen", issue, "-R", REPO)
        self.step("gh issue edit", "issue", "edit", issue, "-R", REPO, "--title", "Edited issue", "--body", "Edited body",
                  "--add-assignee", "@me")
        with self.check("gh issue edit took effect"):
            detail = self.data("issue", "view", issue, "-R", REPO, "--json", "title,body,assignees,state")
            assert detail["title"] == "Edited issue" and detail["state"] == "OPEN", detail
            assert [row["login"] for row in detail["assignees"]] == ["alice"], detail
        self.step("gh issue comment --edit-last", "issue", "comment", issue, "-R", REPO, "--body", "first")
        self.step("gh issue comment --edit-last again", "issue", "comment", issue, "-R", REPO, "--edit-last", "--body", "second")
        self.step("gh issue view --comments", "issue", "view", issue, "-R", REPO, "--comments", expect="second")
        self.step("gh issue list filters", "issue", "list", "-R", REPO, "--assignee", "@me", "--search", "Edited", "--json", "number")
        self.step("gh issue status", "issue", "status", "-R", REPO)
        self.step("gh issue lock", "issue", "lock", issue, "-R", REPO)
        self.step("gh issue unlock", "issue", "unlock", issue, "-R", REPO)
        self.step("gh issue pin", "issue", "pin", issue, "-R", REPO)
        self.step("gh issue unpin", "issue", "unpin", issue, "-R", REPO)
        self.step("gh issue develop", "issue", "develop", issue, "-R", REPO, "--name", "sweep-branch")
        with self.check("gh issue transfer and delete"):
            made = self.gh("issue", "create", "-R", REPO, "--title", "To transfer", "--body", "x").stdout.strip()
            number = made.rstrip("/").rsplit("/", 1)[1]
            self.gh("repo", "create", "e2e-org/gh-target", "--public")
            moved = self.gh("issue", "transfer", number, "e2e-org/gh-target", "-R", REPO).stdout.strip()
            assert "/e2e-org/gh-target/issues/" in moved, moved
            self.gh("issue", "delete", moved.rstrip("/").rsplit("/", 1)[1], "-R", "e2e-org/gh-target", "--confirm")

    def sweep_pulls(self):
        self.git("fetch", "origin", "main", git_auth=True)
        self.git("checkout", "-B", "gh-two", "origin/main")
        (self.clone / "two.txt").write_text("second pull\n")
        self.git("add", "two.txt")
        self.git("commit", "-m", "Second pull")
        self.git("push", "-u", "origin", "gh-two", git_auth=True)
        created = self.step("gh pr create --draft", "pr", "create", "-R", REPO, "--head", "gh-two", "--base", "main",
                            "--title", "Second pull", "--body", "draft", "--draft", cwd=self.clone)
        if created is None:
            return
        number = int(created.stdout.strip().rstrip("/").rsplit("/", 1)[1])
        with self.check("gh pr diff"):
            diff = self.gh("pr", "diff", number, "-R", REPO).stdout
            assert "diff --git a/two.txt b/two.txt" in diff and "+second pull" in diff, diff
        with self.check("gh pr diff --patch"):
            patch = self.gh("pr", "diff", number, "-R", REPO, "--patch").stdout
            assert "Subject: [PATCH]" in patch and "+second pull" in patch, patch
        self.step("gh pr diff --name-only", "pr", "diff", number, "-R", REPO, "--name-only", expect="two.txt")
        self.step("gh pr status", "pr", "status", "-R", REPO)
        with self.check("gh pr checks"):
            head = self.git("rev-parse", "HEAD")
            self.gh("api", f"repos/{REPO}/statuses/{head}", "-f", "state=success", "-f", "context=ci/sweep")
            # gh exits 8 while a check is still pending, which is expected right after the push,
            # and 1 for a failed check, which a workflow run is where runs cannot execute.
            allowed = (0, 1, 8) if self.skip_runs else (0, 8)
            assert "ci/sweep" in self.gh("pr", "checks", number, "-R", REPO, ok=allowed).stdout
        self.step("gh pr view --comments", "pr", "view", number, "-R", REPO, "--comments")
        self.step("gh pr comment", "pr", "comment", number, "-R", REPO, "--body", "pull comment")
        self.step("gh label create bug", "label", "create", "bug", "-R", REPO, "--color", "d73a4a")
        self.step("gh pr edit", "pr", "edit", number, "-R", REPO, "--title", "Second pull edited", "--add-label", "bug")
        self.step("gh pr ready", "pr", "ready", number, "-R", REPO)
        self.step("gh pr ready --undo", "pr", "ready", number, "-R", REPO, "--undo")
        self.step("gh pr lock", "pr", "lock", number, "-R", REPO)
        self.step("gh pr unlock", "pr", "unlock", number, "-R", REPO)
        self.step("gh pr close", "pr", "close", number, "-R", REPO)
        self.step("gh pr reopen", "pr", "reopen", number, "-R", REPO)
        self.step("gh pr update-branch", "pr", "update-branch", number, "-R", REPO)
        self.step("gh pr review --comment", "pr", "review", number, "-R", REPO, "--comment", "--body", "looks fine", who="bob")
        self.step("gh pr review --request-changes", "pr", "review", number, "-R", REPO, "--request-changes", "--body", "no", who="bob")
        self.step("gh pr list --state merged", "pr", "list", "-R", REPO, "--state", "merged", "--json", "number", expect=str(self.pull))
        self.step("gh pr revert", "pr", "revert", self.pull, "-R", REPO)
        self.step("gh pr ready before merge", "pr", "ready", number, "-R", REPO)
        self.step("gh pr merge --squash --admin", "pr", "merge", number, "-R", REPO, "--squash", "--admin")

    def sweep_releases(self):
        extra = self.directory / "extra.txt"
        extra.write_text("extra asset\n")
        self.step("gh release edit", "release", "edit", "v1.0.0", "-R", REPO, "--notes", "Edited notes")
        self.step("gh release upload", "release", "upload", "v1.0.0", extra, "-R", REPO)
        self.step("gh release download --archive", "release", "download", "v1.0.0", "-R", REPO, "--archive", "zip",
                  "--dir", self.directory / "archive")
        self.step("gh release delete-asset", "release", "delete-asset", "v1.0.0", "extra.txt", "-R", REPO, "--yes")
        self.step("gh release create --draft --generate-notes", "release", "create", "v1.1.0", "-R", REPO, "--draft",
                  "--generate-notes", "--target", "main")
        # As on GitHub, a draft's new tag is created only when the release is published, so the
        # draft has no tag to clean up. The published release's tag goes with --cleanup-tag.
        with self.check("gh release create --draft leaves the tag to publishing"):
            refs = self.api(f"repos/{REPO}/git/matching-refs/tags/v1.1.0")
            assert refs == [], refs
        self.step("gh release delete draft", "release", "delete", "v1.1.0", "-R", REPO, "--yes")
        self.step("gh release delete --cleanup-tag", "release", "delete", "v1.0.0", "-R", REPO, "--yes", "--cleanup-tag")
        with self.check("gh release delete --cleanup-tag removes the tag"):
            refs = self.api(f"repos/{REPO}/git/matching-refs/tags/v1.0.0")
            assert refs == [], refs

    def sweep_actions(self):
        self.step("gh workflow list", "workflow", "list", "-R", REPO, expect="client")
        self.step("gh workflow view", "workflow", "view", "client.yml", "-R", REPO)
        self.step("gh workflow disable", "workflow", "disable", "client.yml", "-R", REPO)
        self.step("gh workflow enable", "workflow", "enable", "client.yml", "-R", REPO)
        self.step("gh run list", "run", "list", "-R", REPO, "--json", "databaseId", expect=str(self.run_id))
        self.step("gh run view --json", "run", "view", self.run_id, "-R", REPO, "--json", "jobs")
        if self.skip_runs:
            self.skipped("gh run view --log-failed")
            self.skipped("gh run rerun")
        else:
            self.step("gh run view --log-failed", "run", "view", self.run_id, "-R", REPO, "--log-failed")
            with self.check("gh run rerun"):
                self.gh("run", "rerun", self.run_id, "-R", REPO)
                self.gh("run", "watch", self.run_id, "-R", REPO, "--exit-status", "--interval", "1")
                result = self.data("run", "view", self.run_id, "-R", REPO, "--json", "status,conclusion")
                assert result == {"status": "completed", "conclusion": "success"}, result
        self.step("gh cache list", "cache", "list", "-R", REPO)
        self.step("gh disposable secret set", "secret", "set", "DELETE_ME", "-R", REPO, "--body", "disposable")
        self.step("gh secret delete", "secret", "delete", "DELETE_ME", "-R", REPO)
        self.step("gh variable get", "variable", "get", "REGION", "-R", REPO, expect="e2e-visible")
        self.step("gh disposable variable set", "variable", "set", "DELETE_ME", "-R", REPO, "--body", "disposable")
        self.step("gh variable delete", "variable", "delete", "DELETE_ME", "-R", REPO)
        self.step("gh org secret set", "secret", "set", "ORG_TOKEN", "--org", "e2e-org", "--visibility", "all", "--body", "x")
        self.step("gh org secret list", "secret", "list", "--org", "e2e-org", expect="ORG_TOKEN")
        self.step("gh org secret delete", "secret", "delete", "ORG_TOKEN", "--org", "e2e-org")
        self.step("gh org variable set", "variable", "set", "ORG_VAR", "--org", "e2e-org", "--visibility", "all", "--body", "x")
        self.step("gh org variable delete", "variable", "delete", "ORG_VAR", "--org", "e2e-org")
        self.step("gh api create environment", "api", f"repos/{REPO}/environments/staging", "--method", "PUT")
        self.step("gh env secret set", "secret", "set", "ENV_TOKEN", "-R", REPO, "--env", "staging", "--body", "x")

    def sweep_projects(self):
        owner = ["--owner", "@me"]
        created = self.step("gh project create", "project", "create", *owner, "--title", "Sweep board", "--format", "json")
        if created is None:
            return
        number = json.loads(created.stdout)["number"]
        self.step("gh project list", "project", "list", *owner, expect="Sweep board")
        self.step("gh project view", "project", "view", number, *owner, "--format", "json")
        self.step("gh project edit", "project", "edit", number, *owner, "--title", "Sweep board 2", "--description", "d")
        self.step("gh project field-create", "project", "field-create", number, *owner, "--name", "Estimate", "--data-type", "NUMBER")
        fields = self.step("gh project field-list", "project", "field-list", number, *owner, "--format", "json", expect="Estimate")
        item = self.step("gh project item-create", "project", "item-create", number, *owner, "--title", "Draft card", "--format", "json")
        self.step("gh project item-add", "project", "item-add", number, *owner, "--url", f"{self.base}/{REPO}/issues/{self.issue}")
        listing = self.step("gh project item-list", "project", "item-list", number, *owner, "--format", "json", expect="Draft card")
        view = self.step("gh project view for id", "project", "view", number, *owner, "--format", "json")
        if item and view:
            project_id = json.loads(view.stdout)["id"]
            item_id = json.loads(item.stdout)["id"]
            draft = next((row for row in json.loads(listing.stdout)["items"] if row["title"] == "Draft card"), {})
            draft_id = draft.get("content", {}).get("id") or item_id
            self.step("gh project item-edit", "project", "item-edit", "--id", draft_id, "--title", "Renamed card")
            self.step("gh project item-archive", "project", "item-archive", number, *owner, "--id", item_id)
            self.step("gh project item-delete", "project", "item-delete", number, *owner, "--id", item_id)
        if fields:
            estimate = next(row["id"] for row in json.loads(fields.stdout)["fields"] if row["name"] == "Estimate")
            self.step("gh project field-delete", "project", "field-delete", "--id", estimate)
        self.step("gh repo create own", "repo", "create", "gh-own", "--public")
        self.step("gh project link", "project", "link", number, "--owner", "alice", "--repo", "alice/gh-own")
        self.step("gh project unlink", "project", "unlink", number, "--owner", "alice", "--repo", "alice/gh-own")
        self.step("gh project copy", "project", "copy", number, "--source-owner", "@me", "--target-owner", "@me", "--title", "Sweep copy")
        self.step("gh project mark-template", "project", "mark-template", number, *owner)
        self.step("gh project close", "project", "close", number, *owner)
        self.step("gh project delete", "project", "delete", number, *owner)

    def device_flow(self, *args):
        """Run a gh command that starts the OAuth device flow, and approve its code in the browser session."""
        env = dict(self.env, GH_CONFIG_DIR=str(self.directory / "config-device"), BROWSER="true")
        command = subprocess.Popen(["gh", "auth", *args], env=env, stdin=subprocess.DEVNULL,
                                   stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True)
        try:
            output = ""
            code = None
            while code is None:
                line = command.stdout.readline()
                assert line, "gh exited before printing a one-time code: " + output
                output += line
                found = re.search(r"code: ([A-Z0-9]{4}-[A-Z0-9]{4})", line)
                code = found.group(1) if found else None
            token = re.search(r'name="csrf_token" value="([0-9a-f]{64})"', self.page("/logout")).group(1)
            body = self.directory / "device-approval"
            body.write_text(urllib.parse.urlencode({"csrf_token": token, "user_code": code, "decision": "approve"}))
            self.execute(["python3", "-I", ROOT / "scripts/e2e-web.py", "req", "--base", self.base,
                          "--jar", self.directory.parent / "jar-alice.json", "--who", "alice", "POST", "/login/device",
                          "--body-file", body, "--content-type", "application/x-www-form-urlencoded", "--status", "200"])
            assert command.wait(timeout=120) == 0, command.stdout.read()
        finally:
            if command.poll() is None:
                command.kill()

    def sweep_login(self):
        self.device_flow("login", "--hostname", HOST, "--web", "--git-protocol", "https")
        status = self.gh("auth", "status", "--hostname", HOST, who="device")
        assert "Logged in" in status.stdout + status.stderr, status.stdout + status.stderr
        assert self.api("user", who="device")["login"] == "alice"
        with self.check("gh auth refresh"):
            self.device_flow("refresh", "--hostname", HOST, "--scopes", "read:org")
            assert self.api("user", who="device")["login"] == "alice"
        with self.check("gh auth switch between two stored accounts"):
            for who in ("alice", "bob"):
                self.gh("auth", "login", "--hostname", HOST, "--with-token", "--insecure-storage",
                        input=self.credentials[who] + "\n", who="switch")
            self.gh("auth", "switch", "--hostname", HOST, "--user", "alice", who="switch")
            assert self.api("user", who="switch")["login"] == "alice"
            self.gh("auth", "switch", "--hostname", HOST, "--user", "bob", who="switch")
            assert self.api("user", who="switch")["login"] == "bob"

    def sweep_git_setup(self):
        self.step("gh auth setup-git", "auth", "setup-git", "--hostname", HOST,
                  env={"GIT_CONFIG_GLOBAL": str(self.directory / "gitconfig")})
        self.step("gh auth logout of the device login", "auth", "logout", "--hostname", HOST, who="device")

    def sweep_slow_run(self):
        """A run that is still going, for gh run cancel and delete."""
        if self.skip_runs:
            self.skipped("gh run cancel and delete")
            return
        self.git("checkout", "main")
        self.git("pull", "--ff-only", "origin", "main", git_auth=True)
        (self.clone / ".github/workflows/slow.yml").write_text('''name: Slow
on: workflow_dispatch
jobs:
  wait:
    runs-on: self-hosted
    steps:
      - run: sleep 120
''')
        self.git("add", ".github/workflows/slow.yml")
        self.git("commit", "-m", "Add a slow workflow")
        self.git("push", "origin", "main", git_auth=True)
        with self.check("gh run cancel and delete"):
            known = {row["databaseId"] for row in self.data("run", "list", "-R", REPO, "--json", "databaseId")}
            self.gh("workflow", "run", "slow.yml", "-R", REPO, "--ref", "main")
            deadline = time.monotonic() + 60
            slow = None
            while slow is None:
                assert time.monotonic() < deadline, "slow run never started"
                runs = self.data("run", "list", "-R", REPO, "--workflow", "slow.yml", "--json", "databaseId")
                slow = next((row["databaseId"] for row in runs if row["databaseId"] not in known), None)
                time.sleep(1)
            self.gh("run", "cancel", slow, "-R", REPO)
            deadline = time.monotonic() + 120
            while self.data("run", "view", slow, "-R", REPO, "--json", "status")["status"] != "completed":
                assert time.monotonic() < deadline, "cancelled run never completed"
                time.sleep(2)
            self.gh("run", "delete", slow, "-R", REPO)
    def sweep_discussions(self):
        category = "General"
        self.step("gh repo edit --enable-discussions", "repo", "edit", REPO, "--enable-discussions")
        with self.check("gh discussion create, view, comment, edit and list"):
            made = self.gh("discussion", "create", "-R", REPO, "--title", "Sweep talk", "--body", "hello", "--category", category).stdout.strip()
            number = made.rstrip("/").rsplit("/", 1)[1]
            assert "Sweep talk" in self.gh("discussion", "list", "-R", REPO).stdout
            assert "hello" in self.gh("discussion", "view", number, "-R", REPO).stdout
            self.gh("discussion", "comment", number, "-R", REPO, "--body", "first reply")
            self.gh("discussion", "edit", number, "-R", REPO, "--title", "Sweep talk 2")
            assert "first reply" in self.gh("discussion", "view", number, "-R", REPO, "--comments").stdout


def openapi_nullable(value):
    """Translate OpenAPI 3.0 nullability without relaxing required fields or oneOf."""
    if isinstance(value, list):
        return [openapi_nullable(item) for item in value]
    if not isinstance(value, dict):
        return value
    value = {key: openapi_nullable(item) for key, item in value.items()}
    if value.pop("nullable", False):
        if "enum" in value and None not in value["enum"]:
            value["enum"].append(None)
        if isinstance(value.get("type"), str):
            value["type"] = [value["type"], "null"]
        else:
            value = {"anyOf": [value, {"type": "null"}]}
    return value


def webhook_schemas():
    with gzip.open(ROOT / "spec/github/api.github.com.json.gz") as source:
        components = openapi_nullable(json.load(source)["components"])
    def no_network(uri):
        raise AssertionError("unvendored webhook schema reference: " + uri)
    return components, Registry(retrieve=no_network)


def webhook_validator(components, registry, event, action):
    name = "webhook-" + event.replace("_", "-")
    if action:
        name += "-" + action.replace("_", "-")
    assert name in components["schemas"], "no vendored webhook schema for " + name
    schema = {"components": components, "allOf": [{"$ref": "#/components/schemas/" + name}]}
    return jsonschema.Draft4Validator(schema, registry=registry, format_checker=jsonschema.FormatChecker())


def schema_report(failures):
    return "".join(f"{event}/{action}: " + "\n".join(errors) + "\n" for (event, action), errors in sorted(failures.items(), key=str))


def check_deliveries(receiver, secret, directory, finished_runs=True):
    required = {("repository", "created"), ("push", None), ("create", None),
                ("issues", "opened"), ("issues", "closed"), ("issue_comment", "created"),
                ("pull_request", "opened"), ("pull_request", "closed"),
                ("pull_request_review", "submitted"), ("release", "published")}
    if finished_runs:
        required |= {("workflow_run", "completed"), ("workflow_job", "completed")}
    schemas, registry = webhook_schemas()
    seen = set()
    checked = 0
    deadline = time.monotonic() + 120
    records = []
    # Every event's schema errors are reported together, so one run shows all of them.
    failures = {}
    while True:
        with receiver.lock:
            pending = list(receiver.deliveries[checked:])
        for headers, raw in pending:
            headers = {key.lower(): value for key, value in headers.items()}
            event = headers["x-github-event"]
            uuid.UUID(headers["x-github-delivery"])
            signature = "sha256=" + hmac.new(secret.encode(), raw, hashlib.sha256).hexdigest()
            assert hmac.compare_digest(headers["x-hub-signature-256"], signature)
            body = json.loads(raw)
            action = body.get("action")
            errors = sorted(webhook_validator(schemas, registry, event, action).iter_errors(body), key=lambda error: str(error.path))
            if errors and (event, action) not in failures:
                failures[(event, action)] = [f"{list(error.absolute_path)}: {error.message}" for error in errors[:20]]
            seen.add((event, action))
            records.append({"headers": headers, "body": body})
            checked += 1
        if required <= seen:
            break
        assert time.monotonic() < deadline, schema_report(failures) + f"missing real webhook deliveries: {sorted(required - seen, key=str)}; observed {sorted(seen, key=str)}"
        time.sleep(1)
    (directory / "webhooks.json").write_text(json.dumps(records, indent=2) + "\n")
    assert not failures, schema_report(failures)
    print(f"PASS real-client {checked} signed webhook deliveries match pinned action-specific schemas", flush=True)


def certificate(directory):
    cert = directory / "certificate.pem"
    key = directory / "key.pem"
    subprocess.run(["openssl", "req", "-x509", "-newkey", "rsa:2048", "-nodes", "-days", "1",
                    "-subj", "/CN=" + HOST, "-addext", "subjectAltName=DNS:" + HOST,
                    "-addext", "basicConstraints=critical,CA:TRUE", "-keyout", str(key), "-out", str(cert)],
                   check=True, capture_output=True, timeout=30)
    return cert, key


def storage_loss(args):
    """Pause one real smart-HTTP stream while its storage process is killed."""
    assert args.primary_pid and args.primary_pid > 1 and args.repository and args.source
    upstream = urllib.parse.urlsplit(args.base)
    assert upstream.scheme == "http" and upstream.hostname == "127.0.0.1"
    fired, resume = threading.Event(), threading.Event()
    lock = threading.Lock()
    class Proxy(socketserver.BaseRequestHandler):
        def handle(self):
            try:
                self.request.settimeout(10)
                with socket.create_connection((upstream.hostname, upstream.port), timeout=10) as remote:
                    header = b""
                    while b"\r\n\r\n" not in header:
                        assert len(header) < 16384, "oversized test HTTP header"
                        part = self.request.recv(8192)
                        if not part:
                            return
                        header += part
                    header, body = header.split(b"\r\n\r\n", 1)
                    first = header.split(b"\r\n", 1)[0].split(b" ")
                    operation = b"git-upload-pack" if args.storage_loss == "clone" else b"git-receive-pack"
                    target = len(first) == 3 and first[0] == b"POST" and first[1].endswith(operation)
                    # One request per connection makes the gate independent of curl reuse.
                    header = b"\r\n".join(line for line in header.split(b"\r\n")
                                             if not line.lower().startswith(b"connection:"))
                    remote.sendall(header + b"\r\nConnection: close\r\n\r\n" + body)
                    def gate():
                        with lock:
                            if fired.is_set():
                                return
                            fired.set()
                        assert resume.wait(timeout=45), "storage loss was not released"
                    tail = body[-3:] if args.storage_loss == "push" else b""
                    if target and args.storage_loss == "push" and b"PACK" in body:
                        gate()
                    deadline = time.monotonic() + 150
                    while time.monotonic() < deadline:
                        ready, _, _ = select.select([self.request, remote], [], [], 1)
                        for sender in ready:
                            data = sender.recv(8192)
                            if not data:
                                return
                            receiver = remote if sender is self.request else self.request
                            receiver.sendall(data)
                            if target and ((args.storage_loss == "push" and sender is self.request)
                                           or (args.storage_loss == "clone" and sender is remote)):
                                if b"PACK" in tail + data:
                                    gate()
                                tail = data[-3:]
                    raise AssertionError("test proxy connection exceeded its deadline")
            except (ConnectionError, TimeoutError, OSError):
                # A truncated stream is the intended client-visible failure after SIGKILL.
                return
    proxy = socketserver.ThreadingTCPServer(("127.0.0.1", 0), Proxy)
    directory = args.run / ("storage-loss-" + args.storage_loss)
    directory.mkdir(mode=0o700)
    token = (args.run / "state-files/write_token").read_text().strip()
    encoded = base64.b64encode(("alice:" + token).encode()).decode()
    env = {name: value for name, value in os.environ.items() if not name.startswith("GIT_")}
    env.update(GIT_CONFIG_COUNT="1", GIT_CONFIG_KEY_0="http.extraHeader",
               GIT_CONFIG_VALUE_0="Authorization: Basic " + encoded,
               GIT_CONFIG_NOSYSTEM="1", GIT_CONFIG_GLOBAL="/dev/null", GIT_TERMINAL_PROMPT="0")
    url = f"http://127.0.0.1:{proxy.server_address[1]}/{args.repository}.git"
    command = (["git", "clone", "--quiet", url, str(directory / "clone")] if args.storage_loss == "clone"
               else ["git", "-C", str(args.source), "push", "--quiet", url, "main"])
    with serving(proxy), (directory / "client.log").open("w+b") as log:
        child = subprocess.Popen(command, env=env, stdout=log, stderr=log, start_new_session=True)
        try:
            assert fired.wait(timeout=40), "Git did not start its smart-HTTP transfer"
            os.kill(args.primary_pid, signal.SIGKILL)
            resume.set()
            assert child.wait(timeout=120) != 0, "Git accepted a truncated transfer"
            log.seek(0)
            assert log.read(4096), "Git returned no failure reason"
        finally:
            resume.set()
            if child.poll() is None:
                os.killpg(child.pid, signal.SIGKILL)
            child.wait(timeout=10)
    print(f"PASS cluster {args.storage_loss}: storage SIGKILL returns a client error", flush=True)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--base", required=True)
    parser.add_argument("--run", type=Path, required=True)
    parser.add_argument("--skip-runs", metavar="REASON", default="",
                        help="skip the checks that need a finished workflow run, for this reason")
    parser.add_argument("--storage-loss", choices=("clone", "push"))
    parser.add_argument("--primary-pid", type=int)
    parser.add_argument("--repository")
    parser.add_argument("--source", type=Path)
    args = parser.parse_args()
    if args.storage_loss:
        storage_loss(args)
        return
    assert shutil.which("gh") and shutil.which("openssl"), "gh and OpenSSL are required"
    directory = args.run / "github-client"
    directory.mkdir(mode=0o700)
    cert, key = certificate(directory)
    proxy = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Tunnel)
    proxy.tls = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
    proxy.tls.load_cert_chain(cert, key)
    upstream = urllib.parse.urlsplit(args.base)
    assert upstream.scheme == "http" and upstream.hostname == "127.0.0.1"
    proxy.upstream = (upstream.hostname, upstream.port)
    receiver = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Receiver)
    receiver.lock = threading.Lock()
    receiver.deliveries = []
    secret = secrets.token_hex(32)
    with serving(proxy), serving(receiver):
        client = Client(args.base, directory, f"http://127.0.0.1:{proxy.server_port}", cert, args.skip_runs)
        client.login()
        hook = client.api("orgs/e2e-org/hooks", "--method", "POST", "--input", "-", input=json.dumps({
            "name": "web", "active": True, "events": ["*"],
            "config": {"url": f"http://127.0.0.1:{receiver.server_port}/deliveries", "content_type": "json", "secret": secret}}))
        assert hook["active"] is True and hook["id"] > 0, hook
        try:
            for operation in (client.repositories, client.web_loss, client.issues, client.pulls, client.releases,
                              client.apis_and_values, client.workflows):
                with client.check(operation.__name__ + " setup and remaining checks"):
                    operation()
            client.postgres_failover()
            with client.check("signed webhook schemas"):
                check_deliveries(receiver, secret, directory, finished_runs=not args.skip_runs)
            if args.skip_runs:
                client.skipped("workflow_run and workflow_job completed webhooks")
            for operation in (client.sweep_account, client.sweep_fine_grained, client.sweep_repositories, client.sweep_issues, client.sweep_pulls,
                              client.sweep_releases, client.sweep_actions, client.sweep_projects, client.sweep_discussions, client.sweep_login, client.sweep_slow_run, client.sweep_git_setup):
                with client.check(operation.__name__):
                    operation()
            for name in client.passes:
                print("PASS real-client summary " + name, flush=True)
            for name in client.skips:
                print(f"SKIP real-client summary {name}: {args.skip_runs}", flush=True)
            for diagnostic in client.diagnostics:
                print("FAIL real-client summary " + diagnostic, flush=True)
            assert not client.failures, "real client checks failed: " + ", ".join(client.failures)
        finally:
            removed = client.gh("api", f"orgs/e2e-org/hooks/{hook['id']}", "--method", "DELETE")
            assert removed.stdout == "", removed.stdout


if __name__ == "__main__":
    main()
