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

ROOT = Path(__file__).resolve().parent.parent


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
        values = ports(9)
        self.directory_address = f"127.0.0.1:{values[0]}"
        self.base = f"http://127.0.0.1:{values[1]}"
        self.ssh = values[2]
        self.native = [(f"http://127.0.0.1:{values[3]}", values[4], values[5]),
                       (f"http://127.0.0.1:{values[6]}", values[7], values[8])]
        self.env = {name: value for name, value in os.environ.items()
                    if not name.startswith(("GIT_", "GH_", "GITHUB_", "DATABASE_", "DIRECTORY_", "NODE_", "CLUSTER_"))}
        self.env.update(COOKIE_SECURE="false", PUBLIC_URL=self.base,
                        PUBLIC_HTTP_CLONE_BASE=self.base,
                        PUBLIC_SSH_CLONE_BASE=f"ssh://git@127.0.0.1:{self.ssh}",
                        RUST_LOG="warn", SSH_MODE="builtin", SECRETS_MASTER_KEY=secrets.token_hex(32))
        for name in ("directory", "zero", "one", "router"):
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
        self.settings["directory"]["DIRECTORY_DATABASE_URL"] = os.environ["E2E_CELLS_DIRECTORY_URL"]
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

    def wait(self, name, url):
        deadline = time.monotonic() + 60
        while time.monotonic() < deadline:
            assert self.processes[name].poll() is None, f"{name} stopped; see its server.log"
            try:
                with self.http.open(url + "/health", timeout=2) as response:
                    if response.status == 200:
                        return
            except (OSError, urllib.error.HTTPError):
                pass
            threading.Event().wait(0.1)
        raise AssertionError(f"{name} did not become ready")

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
        self.command("directory", "cell", "directory", "initialize")
        for index, name in enumerate(("zero", "one")):
            self.command(name, "cell", "initialize", "--prefix", index)
            subprocess.run(["ssh-keygen", "-q", "-t", "ed25519", "-N", "", "-f", str(self.root / name / "host_key")],
                           check=True, capture_output=True, timeout=20)
            self.register(index, opened=index == 0)
        self.command("directory", "cell", "directory", "router-key", "--public-key", self.keys["router"]["NODE_PUBLIC_KEY"])
        self.start("directory", "cell", "directory", "serve", "--address", self.directory_address)
        for index, name in enumerate(("zero", "one")):
            web, ssh, rpc = self.native[index]
            self.settings[name].update(DIRECTORY_ADDRESS=self.directory_address,
                DIRECTORY_PUBLIC_KEY=self.keys["directory"]["NODE_PUBLIC_KEY"],
                CELL_RPC_ADDRESS=f"127.0.0.1:{rpc}", GITSERV_NODE_ROLES="server",
                HTTP_ADDR=web.removeprefix("http://"), SSH_ADDR=f"127.0.0.1:{ssh}", HTTP_TRUSTED_PROXY_IPS="127.0.0.1")
            self.start(name, "serve")
            self.wait(name, web)
        self.settings["router"].update(DIRECTORY_ADDRESS=self.directory_address,
            DIRECTORY_PUBLIC_KEY=self.keys["directory"]["NODE_PUBLIC_KEY"], CELL_REGION="a",
            GITSERV_NODE_ROLES="router", HTTP_ADDR=self.base.removeprefix("http://"), SSH_ADDR=f"127.0.0.1:{self.ssh}")
        self.start("router", "serve")
        self.wait("router", self.base)

    def web(self, who, command, *arguments):
        result = subprocess.run([sys.executable, "-I", str(ROOT / "scripts/e2e-web.py"), command,
                                 "--base", self.base, "--jar", str(self.root / (who + ".json")), "--who", who, *map(str, arguments)],
                                text=True, capture_output=True, timeout=80, env=self.env)
        assert result.returncode == 0, f"{who} native HTTP failed: {result.stderr[-3000:]}"
        return result.stdout.strip()

    def account(self, who, setup=False, expected="303"):
        route = "/setup" if setup else "/register"
        arguments = [route, "--action", route, "--has", "username", "--set", "username=" + who,
                     "--set", "email=" + who + "@example.test", "--set", "password=cell-acceptance-password",
                     "--set", "confirm_password=cell-acceptance-password", "--status", expected]
        if setup:
            arguments += ["--set", "node_mode=public"]
        self.web(who, "form", *arguments)

    def token(self, who):
        value = self.web(who, "form", "/settings?tab=tokens", "--action", "/settings/tokens", "--has", "name",
                         "--set", "name=cells acceptance", "--set", "scope=write", "--set", "expiry=never", "--status", "200",
                         "--print", r"re:(?s)Copy this token now.*?(cy_pat_(?:[0-9]{1,5}_)?[0-9a-f]{64})")
        assert re.fullmatch(r"cy_pat_(?:[0-9]{1,5}_)?[0-9a-f]{64}", value)
        return value


def acceptance(deployment):
    github = load("codeyard_cells_github", ROOT / "scripts/e2e-github.py")
    deployment.account("alice", setup=True)
    deployment.register(0, opened=False)
    deployment.register(1)
    deployment.account("bob")
    assert json.loads((deployment.root / "bob.json").read_text())["session_id"].startswith("1.")
    # A fresh browser is placed in cell one, which must refuse cell zero's name.
    deployment.web("duplicate", "form", "/register", "--action", "/register", "--has", "username",
                   "--set", "username=alice", "--set", "email=duplicate@example.test",
                   "--set", "password=cell-acceptance-password", "--set", "confirm_password=cell-acceptance-password", "--status", "409")
    deployment.register(0)
    tokens = {who: deployment.token(who) for who in ("alice", "bob")}
    assert tokens["bob"].startswith("cy_pat_1_")
    state = deployment.root / "state-files"
    state.mkdir(mode=0o700)
    (state / "write_token").write_text(tokens["alice"])
    (state / "gh_bob_token").write_text(tokens["bob"])
    for who in ("alice", "bob"):
        key = deployment.root / (who + "-key")
        subprocess.run(["ssh-keygen", "-q", "-t", "ed25519", "-N", "", "-f", str(key)],
                       check=True, capture_output=True, timeout=20)
        deployment.web(who, "form", "/settings?tab=keys", "--action", "/settings/keys", "--has", "public_key",
                       "--set", "name=cells SSH", "--set", "public_key=" + key.with_suffix(".pub").read_text().strip(), "--status", "303")

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
        assert alice["id"] != bob["id"] and bob["id"] > alice["id"]
        client.gh("repo", "create", "alice/shared", "--private")
        client.gh("repo", "create", "bob/other", "--public", who="bob")
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
        client.api(f"user/repository_invitations/{invitation['id']}", "--method", "PATCH", who="bob")
        client.checked("cell one collaborator accepts the cell zero repository invitation")
        deadline = time.monotonic() + 30
        while not any(repo["full_name"] == "alice/shared" for repo in client.api("user/repos", who="bob")):
            assert time.monotonic() < deadline, "the home repository list did not receive the remote membership"
            threading.Event().wait(0.1)
        assert client.api("user/repos?per_page=1&page=1", who="bob")[0]["full_name"] == "alice/shared"
        assert client.api("user/repos?per_page=1&page=2", who="bob")[0]["full_name"] == "bob/other"
        client.checked("gh api user/repos discovers the remote collaborator repository through its home pointer")

        def ssh_environment(who):
            return {"GIT_SSH_COMMAND": shlex.join(["ssh", "-F", "/dev/null", "-o", "BatchMode=yes", "-o", "IdentitiesOnly=yes",
                "-o", "IdentityAgent=none", "-o", "ConnectTimeout=15", "-o", "StrictHostKeyChecking=accept-new",
                "-o", "UserKnownHostsFile=" + str(deployment.root / "known_hosts"), "-i", str(deployment.root / (who + "-key"))])}
        client.git("checkout", "-b", "proposed")
        (client.clone / "README.md").write_text("two cells\nforeign SSH push\n")
        client.git("add", "README.md")
        client.git("commit", "-m", "Push from the other cell")
        client.git("push", f"ssh://git@127.0.0.1:{deployment.ssh}/alice/shared.git", "proposed", who="bob", env=ssh_environment("bob"))
        client.git("push", "origin", "proposed:http-proof", who="bob", git_auth=True)
        pull = client.gh("pr", "create", "-R", "alice/shared", "--base", "main", "--head", "proposed",
                         "--title", "Across cells", "--body", "Created by the collaborator in cell one", who="bob", cwd=client.clone)
        number = pull.stdout.strip().rstrip("/").rsplit("/", 1)[1]
        assert number.isdecimal(), pull.stdout
        client.gh("pr", "merge", number, "-R", "alice/shared", "--merge", who="bob")
        assert client.data("pr", "view", number, "-R", "alice/shared", "--json", "state", who="bob")["state"] == "MERGED"
        clone = directory / "ssh-clone"
        client.execute(["git", "clone", f"ssh://git@127.0.0.1:{deployment.ssh}/alice/shared.git", str(clone)],
                       who="bob", env=ssh_environment("bob"))
        assert "foreign SSH push" in (clone / "README.md").read_text()
        client.checked("foreign HTTP and SSH pushes, gh pr create and gh pr merge use live resource grants")
        client.gh("issue", "create", "-R", "alice/shared", "--title", "Router issue", "--body", "cross cell", who="bob")
        assert any(issue["title"] == "Router issue" for issue in client.data("issue", "list", "-R", "alice/shared", "--json", "title"))
        query = '{ zero:repository(owner:"alice",name:"shared"){ id name } one:repository(owner:"bob",name:"other"){ id name } }'
        answer = client.api("graphql", "--method", "POST", "--input", "-", input=json.dumps({"query": query}))
        assert not answer.get("errors"), answer
        assert answer["data"]["zero"]["name"] == "shared" and answer["data"]["one"]["name"] == "other", answer
        identity = answer["data"]["one"]["id"]
        node = client.api("graphql", "--method", "POST", "--input", "-", input=json.dumps({"query": 'query($id:ID!){node(id:$id){id ... on Repository{name}}}', "variables": {"id": identity}}))
        assert node["data"]["node"] == {"id": identity, "name": "other"}, node
        people = client.api("search/users?q=bob")
        assert all(item["id"] != bob["id"] for item in people["items"]), "a local search must not expose metadata stubs"
        client.checked("node lookup and one native GraphQL query span both cells; stubs stay out of account search")

        def http(path, token=None):
            request = urllib.request.Request(deployment.base + path, headers={"Accept": "application/json", **({"Authorization": "Bearer " + token} if token else {})})
            try:
                response = deployment.http.open(request, timeout=15)
            except urllib.error.HTTPError as error:
                response = error
            with response:
                body = response.read(2 * 1024 * 1024 + 1)
                assert len(body) <= 2 * 1024 * 1024
                return response.status, body
        deployment.stop("one")
        assert http("/api/v3/repos/bob/other", tokens["alice"])[0] == 503
        assert client.api("repos/alice/shared")["name"] == "shared"
        deployment.start("one", "serve")
        deployment.wait("one", deployment.native[1][0])
        assert client.api("repos/bob/other")["name"] == "other"
        client.checked("a stopped cell returns a clear unavailable error while the other cell keeps working")
        assert http("/api/v3/repos/alice/shared", tokens["bob"])[0] == 200
        token_id = deployment.web("bob", "req", "GET", "/settings?tab=tokens", "--print", r"re:/settings/tokens/([0-9]+)/delete")
        assert token_id.isdecimal()
        deployment.web("bob", "form", "/settings?tab=tokens", "--action", f"/settings/tokens/{token_id}/delete", "--set", "confirm=yes", "--status", "303")
        deadline = time.monotonic() + 61
        while http("/api/v3/repos/alice/shared", tokens["bob"])[0] == 200:
            assert time.monotonic() < deadline, "home token revocation did not reach the resource cell within 60 seconds"
            threading.Event().wait(0.1)
        assert client.gh("issue", "create", "-R", "alice/shared", "--title", "Must not be written", "--body", "revoked", who="bob", ok=(1,)).returncode == 1
        assert all(issue["title"] != "Must not be written" for issue in client.data("issue", "list", "-R", "alice/shared", "--json", "title"))
        client.checked("revoked home token stops foreign reads and writes within the cache lifetime")
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
