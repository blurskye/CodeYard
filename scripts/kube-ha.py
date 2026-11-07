#!/usr/bin/env python3
"""Run Codeyard on a Kubernetes cluster with CloudNativePG and test it through its real boundaries.

  kube-ha.py image IMAGE    copy a local Podman image to every k3s node: one upload to a
                            temporary pod, then each node imports it with k3s ctr over ssh
  kube-ha.py up IMAGE       the operator, a test S3 store, then the chart with a three-instance
                            PostgreSQL 18 cluster, PgBouncer and an ingress on <name>.<ip>.nip.io
  kube-ha.py test [STEP..]  steps (default all): basic (setup, Git over HTTP and SSH, issues),
                            kill (crash the primary pod), drain (cordon and drain its node),
                            restart (rolling restart of web), scale (web 2 -> 4 under load)
  kube-ha.py down           delete the namespace; the operator and its CRDs stay

Settings: KUBE_CONTEXT (default: the current context), NAMESPACE (codeyard-test),
NODE_SSH_KEY and NODE_SSH_USER (root) for `image`, INGRESS_IP (default: the first worker's
external IP). Generated passwords and keys live only in cluster Secrets. Needs kubectl, helm,
git, ssh and ssh-keygen; `image` also needs podman and k3s nodes reachable over SSH.
"""
import base64
import collections
import contextlib
import json
import os
import re
import secrets
import socket
import subprocess
import sys
import tempfile
import threading
import time
import urllib.error
import urllib.request
from dataclasses import dataclass
from datetime import date, timedelta
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
CHART = ROOT / "deploy/helm/codeyard"
FORMS = ROOT / "scripts/e2e-web.py"
CNPG_VERSION = "1.30.1"
CNPG_MANIFEST = (
    "https://github.com/cloudnative-pg/cloudnative-pg/releases/download/"
    f"v{CNPG_VERSION}/cnpg-{CNPG_VERSION}.yaml"
)
# The same S3 server compose.yaml pins; a single test pod, not part of the chart.
RUSTFS_IMAGE = (
    "docker.io/rustfs/rustfs:1.0.1"
    "@sha256:1803faef57627e2d9c2e7d89d655d712ddded5389040054987163043fecb6a3c"
)
# Serves the image archive to the nodes during `image`.
BUSYBOX_IMAGE = "docker.io/library/busybox:1.37.0@sha256:a7dbc319b1438115a207e11930dc4bb3e643dcb3ec0bdd694bb36cd8fad289ec"
NS = os.environ.get("NAMESPACE", "codeyard-test")
CONTEXT = os.environ.get("KUBE_CONTEXT", "")
RELEASE = "codeyard"
PG = f"{RELEASE}-pg"
BUCKET = "codeyard-files"
CONTROL_PLANE = "node-role.kubernetes.io/control-plane"


def log(message):
    print(time.strftime("%H:%M:%S"), message, file=sys.stderr, flush=True)


def expect(condition, message):
    if not condition:
        raise SystemExit(f"FAILED: {message}")


def run(command, *, input=None, timeout=280, check=True, env=None, cwd=None):
    result = subprocess.run([str(part) for part in command], input=input, capture_output=True,
                            text=True, timeout=timeout, env=env, cwd=cwd)
    if check and result.returncode:
        shown = " ".join(str(part) for part in command[:6])
        raise SystemExit(f"{shown} ... failed: {(result.stderr or result.stdout).strip()[-2000:]}")
    return result


def kubectl_command(namespaced=True):
    return ["kubectl"] + (["--context", CONTEXT] if CONTEXT else []) + (["-n", NS] if namespaced else [])


def kubectl(*args, namespaced=True, **options):
    return run(kubectl_command(namespaced) + list(args), **options).stdout


def kube_json(*args, namespaced=True):
    return json.loads(kubectl(*args, "-o", "json", namespaced=namespaced))


def exists(*args, namespaced=True):
    return run(kubectl_command(namespaced) + ["get", *args], check=False).returncode == 0


def wait_for(what, condition, timeout, interval=2):
    deadline = time.time() + timeout
    while time.time() < deadline:
        if condition():
            return
        time.sleep(interval)
    raise SystemExit(f"FAILED: {what} within {timeout} seconds")


def nodes():
    found = []
    for node in kube_json("get", "nodes", namespaced=False)["items"]:
        addresses = {address["type"]: address["address"] for address in node["status"]["addresses"]}
        found.append((node["metadata"]["name"], addresses.get("ExternalIP") or addresses["InternalIP"],
                      CONTROL_PLANE in node["metadata"]["labels"]))
    return sorted(found)


# ------------------------------------------------------------------ image --

def copy_pod():
    labels = {"app.kubernetes.io/name": "codeyard-image-copy"}
    return {"apiVersion": "v1", "kind": "Pod", "metadata": {"name": "codeyard-image-copy", "labels": labels}, "spec": {
        "securityContext": {"runAsNonRoot": True, "runAsUser": 65534, "runAsGroup": 65534,
                            "seccompProfile": {"type": "RuntimeDefault"}},
        "containers": [{"name": "http", "image": BUSYBOX_IMAGE, "command": ["httpd", "-f", "-p", "8080", "-h", "/srv"],
                        "readinessProbe": {"tcpSocket": {"port": 8080}, "periodSeconds": 2},
                        "securityContext": {"allowPrivilegeEscalation": False, "readOnlyRootFilesystem": True,
                                            "capabilities": {"drop": ["ALL"]}},
                        "volumeMounts": [{"name": "srv", "mountPath": "/srv"}]}],
        "volumes": [{"name": "srv", "emptyDir": {}}]}}


def cmd_image(image):
    """Upload one saved copy of the image to a temporary pod; every node imports it from there.

    Nothing goes to a registry. The upload crosses the operator's link once; the nodes fetch
    the archive over the cluster network and import it with `k3s ctr`.
    """
    key = os.environ.get("NODE_SSH_KEY", "")
    user = os.environ.get("NODE_SSH_USER", "root")
    ssh = ["ssh", "-o", "BatchMode=yes", "-o", "ConnectTimeout=10", "-o", "StrictHostKeyChecking=accept-new"]
    if key:
        ssh += ["-i", key, "-o", "IdentitiesOnly=yes"]
    if not exists("namespace", NS, namespaced=False):
        kubectl("create", "namespace", NS, namespaced=False)
    with tempfile.TemporaryDirectory() as work:
        archive = Path(work) / "image.tar.gz"
        log(f"saving {image}")
        with open(archive, "wb") as out:
            save = subprocess.Popen(["podman", "save", "--format", "docker-archive", image], stdout=subprocess.PIPE)
            subprocess.run(["gzip", "-1"], stdin=save.stdout, stdout=out, check=True)
            expect(save.wait() == 0, "podman save")
        kubectl("delete", "pod", "codeyard-image-copy", "--ignore-not-found")
        kubectl("create", "-f", "-", input=json.dumps(copy_pod()))
        try:
            kubectl("wait", "--for=condition=Ready", "pod/codeyard-image-copy", "--timeout=240s")
            log(f"uploading {archive.stat().st_size >> 20} MiB")
            with open(archive, "rb") as stream:
                upload = subprocess.run(kubectl_command() + [
                    "exec", "-i", "codeyard-image-copy", "--", "sh", "-c",
                    "cat > /srv/part && mv /srv/part /srv/image.tar.gz"], stdin=stream, timeout=3600)
            expect(upload.returncode == 0, "upload to the copy pod")
            source = "http://{}:8080/image.tar.gz".format(
                kubectl("get", "pod", "codeyard-image-copy", "-o", "jsonpath={.status.podIP}"))
            jobs = [(name, subprocess.Popen(
                ssh + [f"{user}@{address}",
                       f"set -o pipefail; curl -fsS {source} | gzip -dc | k3s ctr -n k8s.io images import -"],
                stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True)) for name, address, _ in nodes()]
            failed = []
            for name, job in jobs:
                output = job.communicate(timeout=1800)[0]
                log(f"{name}: {output.strip().splitlines()[-1:] or ['done']}")
                if job.returncode:
                    failed.append(name)
            expect(not failed, f"image import on {', '.join(failed)}")
        finally:
            kubectl("delete", "pod", "codeyard-image-copy", "--wait=false")


# --------------------------------------------------------------------- up --

def create_secret(name, data):
    # `create`, not `apply`: apply would copy the values into an annotation.
    if not exists("secret", name):
        kubectl("create", "-f", "-", input=json.dumps({
            "apiVersion": "v1", "kind": "Secret", "metadata": {"name": name},
            "type": "Opaque", "stringData": data}))


def s3_store(node):
    labels = {"app.kubernetes.io/name": "codeyard-s3"}
    keys = [{"name": f"RUSTFS_{kind}_KEY", "valueFrom": {"secretKeyRef": {
        "name": "codeyard-config", "key": f"S3_{kind}_KEY"}}} for kind in ("ACCESS", "SECRET")]
    return [
        {"apiVersion": "v1", "kind": "PersistentVolumeClaim", "metadata": {"name": "codeyard-s3"},
         "spec": {"accessModes": ["ReadWriteOnce"], "resources": {"requests": {"storage": "5Gi"}}}},
        {"apiVersion": "apps/v1", "kind": "Deployment", "metadata": {"name": "codeyard-s3", "labels": labels},
         "spec": {"replicas": 1, "strategy": {"type": "Recreate"}, "selector": {"matchLabels": labels},
                  "template": {"metadata": {"labels": labels}, "spec": {
                      # A control-plane node: the drain run takes a worker away.
                      "nodeSelector": {"kubernetes.io/hostname": node},
                      "securityContext": {"runAsNonRoot": True, "runAsUser": 10001, "runAsGroup": 10001,
                                          "fsGroup": 10001, "seccompProfile": {"type": "RuntimeDefault"}},
                      "containers": [{
                          "name": "rustfs", "image": RUSTFS_IMAGE, "env": keys,
                          "ports": [{"name": "s3", "containerPort": 9000}],
                          "securityContext": {"allowPrivilegeEscalation": False, "capabilities": {"drop": ["ALL"]}},
                          "readinessProbe": {"tcpSocket": {"port": "s3"}, "periodSeconds": 5},
                          "volumeMounts": [{"name": "data", "mountPath": "/data"}]}],
                      "volumes": [{"name": "data", "persistentVolumeClaim": {"claimName": "codeyard-s3"}}]}}}},
        {"apiVersion": "v1", "kind": "Service", "metadata": {"name": "codeyard-s3"},
         "spec": {"selector": labels, "ports": [{"name": "s3", "port": 9000, "targetPort": "s3"}]}},
    ]


def bucket_job(image):
    keys = [{"name": name, "valueFrom": {"secretKeyRef": {"name": "codeyard-config", "key": name}}}
            for name in ("S3_ACCESS_KEY", "S3_SECRET_KEY")]
    script = ('code=$(curl -sS -o /dev/null -w "%{http_code}" --aws-sigv4 aws:amz:us-east-1:s3 '
              f'--user "$S3_ACCESS_KEY:$S3_SECRET_KEY" -X PUT http://codeyard-s3:9000/{BUCKET}); '
              'echo "$code"; [ "$code" = 200 ] || [ "$code" = 409 ]')
    return {"apiVersion": "batch/v1", "kind": "Job", "metadata": {"name": "codeyard-s3-bucket"},
            "spec": {"backoffLimit": 20, "template": {"spec": {
                "restartPolicy": "OnFailure",
                "securityContext": {"runAsNonRoot": True, "runAsUser": 1000, "seccompProfile": {"type": "RuntimeDefault"}},
                "containers": [{"name": "bucket", "image": image, "imagePullPolicy": "Never", "env": keys,
                                "command": ["/bin/sh", "-ec", script],
                                "securityContext": {"allowPrivilegeEscalation": False, "readOnlyRootFilesystem": True,
                                                    "capabilities": {"drop": ["ALL"]}}}]}}}}


def spread(component):
    return {"podAntiAffinity": {"preferredDuringSchedulingIgnoredDuringExecution": [{
        "weight": 100, "podAffinityTerm": {"topologyKey": "kubernetes.io/hostname", "labelSelector": {
            "matchLabels": {"app.kubernetes.io/instance": RELEASE, "app.kubernetes.io/component": component}}}}]}}


def chart_values(image, host, s3_host):
    repository, tag = image.rsplit(":", 1)
    return {
        "image": {"repository": repository, "tag": tag, "pullPolicy": "Never"},
        "secrets": {"existingSecret": "codeyard-config"},
        "config": {
            "publicUrl": f"http://{host}", "publicHttpCloneBase": f"http://{host}",
            "publicSshCloneBase": "ssh://git@127.0.0.1:2222", "cookieSecure": False,
            "s3": {"endpoint": "http://codeyard-s3:9000", "publicEndpoint": f"http://{s3_host}", "bucket": BUCKET},
        },
        "web": {"replicas": 2, "pdb": {"enabled": True}, "affinity": spread("web")},
        "ssh": {"enabled": True, "service": {"type": "ClusterIP"}},
        "storage": {"replicas": 3, "persistence": {"size": "5Gi"}, "affinity": spread("storage")},
        "postgres": {"cnpg": {
            "enabled": True, "storage": {"size": "5Gi"}, "parameters": {"max_connections": "300"},
            # One instance per worker, so draining a worker never takes the operator or S3 store.
            "affinity": {
                "enablePodAntiAffinity": True, "podAntiAffinityType": "required",
                "topologyKey": "kubernetes.io/hostname",
                "nodeAffinity": {"requiredDuringSchedulingIgnoredDuringExecution": {"nodeSelectorTerms": [
                    {"matchExpressions": [{"key": CONTROL_PLANE, "operator": "DoesNotExist"}]}]}}},
        }},
        "ingress": {"enabled": True, "className": "traefik", "host": host},
    }


def ingress_ip(found):
    return os.environ.get("INGRESS_IP") or next(address for _, address, control in found if not control)


def cmd_up(image):
    found = nodes()
    ip = ingress_ip(found)
    host, s3_host = f"codeyard.{ip}.nip.io", f"codeyard-s3.{ip}.nip.io"
    log(f"CloudNativePG {CNPG_VERSION}")
    kubectl("apply", "--server-side", "-f", CNPG_MANIFEST, namespaced=False)
    kubectl("-n", "cnpg-system", "rollout", "status", "deployment/cnpg-controller-manager",
            "--timeout=240s", namespaced=False)
    if not exists("namespace", NS, namespaced=False):
        kubectl("create", "namespace", NS, namespaced=False)
    create_secret("codeyard-config", {
        "SECRETS_MASTER_KEY": secrets.token_hex(32),
        "ADMIN_SECRET": secrets.token_urlsafe(32),
        "S3_ACCESS_KEY": "codeyard" + secrets.token_hex(8),
        "S3_SECRET_KEY": secrets.token_urlsafe(32),
    })
    create_secret("codeyard-test-admin", {"username": "kubeadmin", "password": secrets.token_urlsafe(24)})
    log("test S3 store")
    s3_node = [name for name, _, control in found if control][-1]
    kubectl("apply", "-f", "-", input=json.dumps({"apiVersion": "v1", "kind": "List", "items": s3_store(s3_node)}))
    kubectl("rollout", "status", "deployment/codeyard-s3", "--timeout=240s")
    kubectl("delete", "job", "codeyard-s3-bucket", "--ignore-not-found")
    kubectl("create", "-f", "-", input=json.dumps(bucket_job(image)))
    kubectl("wait", "--for=condition=complete", "job/codeyard-s3-bucket", "--timeout=240s")
    log(f"chart on http://{host}")
    with tempfile.NamedTemporaryFile("w", suffix=".json") as values:
        json.dump(chart_values(image, host, s3_host), values)
        values.flush()
        helm = ["helm", "upgrade", "--install", RELEASE, CHART, "--namespace", NS, "-f", values.name]
        if CONTEXT:
            helm += ["--kube-context", CONTEXT]
        for attempt in range(3):
            # The operator's webhook may still be starting on a fresh install.
            if run(helm, check=attempt == 2).returncode == 0:
                break
            time.sleep(15)
    wait_healthy(f"http://{host}")
    log(f"ready: http://{host}")


def pg_cluster():
    return kube_json("get", "cluster", PG)


def wait_healthy(base):
    instances = pg_cluster()["spec"]["instances"]
    wait_for("PostgreSQL instances ready", lambda: pg_cluster().get("status", {}).get("readyInstances") == instances, 900, 5)
    for workload in ("deployment/codeyard-pgbouncer", "deployment/codeyard-web", "deployment/codeyard-ssh"):
        kubectl("rollout", "status", workload, "--timeout=280s")
    kubectl("wait", "--for=condition=Ready", "pod", "-l", "app.kubernetes.io/component=storage", "--timeout=280s")
    wait_for("/ready through the ingress", lambda: http("GET", f"{base}/ready")[0] == 200, 120)


# ------------------------------------------------------------------- HTTP --

class _NoRedirect(urllib.request.HTTPRedirectHandler):
    def redirect_request(self, *args, **kwargs):
        return None


_OPENER = urllib.request.build_opener(_NoRedirect)


def http(method, url, body=None, headers=None, timeout=10):
    request = urllib.request.Request(url, data=body, method=method, headers=headers or {})
    try:
        with _OPENER.open(request, timeout=timeout) as response:
            return response.status, response.read(8 << 20)
    except urllib.error.HTTPError as error:
        return error.code, error.read(1 << 20)


class Forge:
    def __init__(self, base, work):
        self.base, self.work = base, Path(work)
        self.token = self.owner = self.repo = self.cookie = ""

    def forms(self, *args):
        return run([sys.executable, "-I", FORMS, args[0], "--base", self.base,
                    "--jar", self.work / "jar.json", "--who", "admin", *args[1:]]).stdout.strip()

    def api(self, method, path, body=None, timeout=10):
        data = None if body is None else json.dumps(body).encode()
        status, raw = http(method, self.base + path, data, {
            "Authorization": f"token {self.token}", "Accept": "application/vnd.github+json",
            "Content-Type": "application/json"}, timeout)
        return status, (json.loads(raw) if raw[:1] in (b"{", b"[") else raw)

    def sign_in(self):
        account = kube_json("get", "secret", "codeyard-test-admin")["data"]
        decode = lambda value: base64.b64decode(value).decode()
        self.owner, password = decode(account["username"]), decode(account["password"])
        if http("GET", f"{self.base}/setup")[0] == 200:
            admin_secret = decode(kube_json("get", "secret", "codeyard-config")["data"]["ADMIN_SECRET"])
            log("first-run setup through the /setup form")
            self.forms("form", "/setup", "--action", "/setup", "--has", "node_mode",
                       "--set", f"username={self.owner}", "--set", f"email={self.owner}@example.test",
                       "--set", f"password={password}", "--set", f"confirm_password={password}",
                       "--set", "node_mode=private", "--set", f"admin_secret={admin_secret}",
                       "--status", "303", "--location", "/")
        else:
            self.forms("form", "/login", "--action", "/login", "--has", "username",
                       "--set", f"username={self.owner}", "--set", f"password={password}", "--location", "/")
        self.token = self.forms(
            "form", "/settings?tab=tokens", "--action", "/settings/tokens", "--has", "name",
            "--set", f"name=kube-ha {time.strftime('%Y%m%d-%H%M%S')}", "--set", "scope=write",
            "--set", "expiry=date", "--set", f"expires_at={date.today() + timedelta(days=2)}",
            "--status", "200",
            "--print", r"re:(?s)Copy this token now.*?(cy_pat_(?:[0-9]{1,5}_)?[0-9a-f]{64})")
        expect(self.token.startswith("cy_pat_"), "personal access token from the settings form")
        jar = json.loads((self.work / "jar.json").read_text())
        self.cookie = "; ".join(f"{name}={value}" for name, value in jar.items())

    def revoke_tokens(self):
        """Revoke every token of the test account, including ones left by earlier runs."""
        for _ in range(20):
            status, html = http("GET", f"{self.base}/settings?tab=tokens",
                                headers={"Accept": "text/html", "Cookie": self.cookie})
            expect(status == 200, f"tokens page answered {status}")
            actions = sorted(set(re.findall(r"/settings/tokens/[0-9]+/delete", html.decode())))
            if not actions:
                return
            for action in actions:
                self.forms("form", "/settings?tab=tokens", "--action", action, "--set", "confirm=yes")
        expect(False, "tokens are still listed after 20 rounds of revoking")

    def page(self, path, timeout=10):
        """An HTML page as the signed-in owner: the node is private, so pages need a session."""
        return http("GET", self.base + path, headers={"Accept": "text/html", "Cookie": self.cookie}, timeout=timeout)[0]

    def issue_titles(self):
        titles, page = {}, 1
        while True:
            status, items = self.api("GET", f"/api/v3/repos/{self.repo}/issues?state=all&per_page=100&page={page}", timeout=30)
            expect(status == 200, f"issue list page {page}: {status}")
            titles.update((item["number"], item["title"]) for item in items)
            if len(items) < 100:
                return titles
            page += 1


# ------------------------------------------------------------------ basic --

def free_port():
    with socket.socket() as probe:
        probe.bind(("127.0.0.1", 0))
        return probe.getsockname()[1]


@contextlib.contextmanager
def port_forward(target, remote):
    port = free_port()
    forward = subprocess.Popen(kubectl_command() + ["port-forward", target, f"{port}:{remote}"],
                               stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    try:
        def listening():
            with contextlib.suppress(OSError), socket.create_connection(("127.0.0.1", port), 1):
                return True
            return False
        wait_for(f"port-forward to {target}", listening, 30, 1)
        yield port
    finally:
        forward.terminate()
        forward.wait(10)


def git(forge, *args, cwd=None, ssh=None):
    env = {"PATH": os.environ["PATH"], "HOME": str(forge.work), "GIT_CONFIG_GLOBAL": "/dev/null",
           "GIT_CONFIG_NOSYSTEM": "1", "GIT_TERMINAL_PROMPT": "0",
           "GIT_AUTHOR_NAME": forge.owner, "GIT_AUTHOR_EMAIL": f"{forge.owner}@example.test",
           "GIT_COMMITTER_NAME": forge.owner, "GIT_COMMITTER_EMAIL": f"{forge.owner}@example.test"}
    if ssh:
        env["GIT_SSH_COMMAND"] = ssh
    # Through the environment, so the token stays out of argv and error messages.
    auth = base64.b64encode(f"token:{forge.token}".encode()).decode()
    env.update(GIT_CONFIG_COUNT="1", GIT_CONFIG_KEY_0="http.extraHeader",
               GIT_CONFIG_VALUE_0=f"Authorization: Basic {auth}")
    return run(["git", *args], cwd=cwd, env=env, timeout=120).stdout.strip()


def step_basic(forge):
    forge.repo = f"{forge.owner}/ha-{time.strftime('%Y%m%d-%H%M%S')}"
    name = forge.repo.split("/")[1]
    status, body = forge.api("POST", "/api/v3/user/repos", {"name": name, "description": "Kubernetes HA check"})
    expect(status == 201, f"create repository: {status} {body}")
    http_work = forge.work / "http"
    http_work.mkdir()
    git(forge, "init", "-q", "-b", "main", cwd=http_work)
    (http_work / "README.md").write_text("# Kubernetes check\n")
    git(forge, "add", ".", cwd=http_work)
    git(forge, "commit", "-q", "-m", "First commit over HTTP", cwd=http_work)
    git(forge, "push", "-q", f"{forge.base}/{forge.repo}.git", "main", cwd=http_work)
    log("pushed over HTTP through the ingress")

    key = forge.work / "id_ed25519"
    run(["ssh-keygen", "-q", "-t", "ed25519", "-N", "", "-f", key])
    status, body = forge.api("POST", "/api/v3/user/keys", {"title": "kube-ha", "key": key.with_suffix(".pub").read_text().strip()})
    expect(status == 201, f"add SSH key: {status} {body}")
    with port_forward("svc/codeyard-ssh", 22) as port:
        ssh = (f"ssh -F /dev/null -i {key} -o IdentitiesOnly=yes -o BatchMode=yes -o IdentityAgent=none "
               f"-o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o LogLevel=ERROR -p {port}")
        ssh_work = forge.work / "ssh"
        git(forge, "clone", "-q", f"ssh://git@127.0.0.1:{port}/{forge.repo}.git", ssh_work, ssh=ssh)
        (ssh_work / "ssh.txt").write_text("pushed over SSH\n")
        git(forge, "add", ".", cwd=ssh_work)
        git(forge, "commit", "-q", "-m", "Second commit over SSH", cwd=ssh_work)
        git(forge, "push", "-q", "origin", "main", cwd=ssh_work, ssh=ssh)
        head = git(forge, "rev-parse", "HEAD", cwd=ssh_work)
    log("cloned and pushed over SSH")
    remote = git(forge, "ls-remote", f"{forge.base}/{forge.repo}.git", "refs/heads/main", cwd=http_work).split()[0]
    expect(remote == head, "the SSH push is visible over HTTP")

    status, issue = forge.api("POST", f"/api/v3/repos/{forge.repo}/issues", {"title": "Kubernetes check", "body": "Opened through the API"})
    expect(status == 201, f"open issue: {status} {issue}")
    status, again = forge.api("GET", f"/api/v3/repos/{forge.repo}/issues/{issue['number']}")
    expect(status == 200 and again["title"] == "Kubernetes check", "read the issue back")
    status = forge.page(f"/{forge.repo}/issues")
    expect(status == 200, f"issue page: {status}")
    print(f"basic: repository {forge.repo}, HTTP and SSH pushes, issue #{issue['number']}: ok", flush=True)


# --------------------------------------------------------------- failures --

@dataclass
class Result:
    start: float
    end: float
    ok: bool
    status: object
    number: object = None
    title: str = ""


class Traffic:
    """One issue write a second and steady issue-page reads, each result recorded."""

    def __init__(self, forge, label, readers=1, read_interval=1.0):
        self.forge, self.label = forge, label
        self.stop = threading.Event()
        self.writes, self.reads = [], []
        self.threads = [threading.Thread(target=self.write_loop)]
        self.threads += [threading.Thread(target=self.read_loop, args=(read_interval,)) for _ in range(readers)]

    def __enter__(self):
        for thread in self.threads:
            thread.start()
        return self

    def __exit__(self, *_):
        self.stop.set()
        for thread in self.threads:
            thread.join()

    def write_loop(self):
        sequence = 0
        while not self.stop.is_set():
            sequence += 1
            title, start, number = f"{self.label} {sequence}", time.time(), None
            try:
                status, body = self.forge.api("POST", f"/api/v3/repos/{self.forge.repo}/issues", {"title": title}, timeout=5)
                if status == 201:
                    number = body["number"]
            except Exception as error:  # a timeout or reset is an unacknowledged write
                status = type(error).__name__
            self.writes.append(Result(start, time.time(), number is not None, status, number, title))
            self.stop.wait(max(0.0, start + 1 - time.time()))

    def read_loop(self, interval):
        while not self.stop.is_set():
            start = time.time()
            try:
                status = self.forge.page(f"/{self.forge.repo}/issues", timeout=5)
            except Exception as error:
                status = type(error).__name__
            self.reads.append(Result(start, time.time(), status == 200, status))
            self.stop.wait(max(0.0, start + interval - time.time()))

    def recovered(self, since, streak=10):
        later = [write.ok for write in self.writes if write.start > since]
        return len(later) >= streak and all(later[-streak:])


class PrimaryWatch(threading.Thread):
    def __init__(self):
        super().__init__()
        self.stop, self.changes = threading.Event(), []

    def run(self):
        last = None
        while not self.stop.wait(1):
            result = run(kubectl_command() + ["get", "cluster", PG, "-o", "jsonpath={.status.currentPrimary}"], check=False, timeout=20)
            if result.returncode == 0 and result.stdout and result.stdout != last:
                last = result.stdout
                self.changes.append((time.time(), last))

    def __enter__(self):
        self.start()
        return self

    def __exit__(self, *_):
        self.stop.set()
        self.join()


def web_restarts():
    pods = kube_json("get", "pods", "-l", "app.kubernetes.io/component=web")["items"]
    return {pod["metadata"]["name"]: sum(status["restartCount"] for status in pod["status"].get("containerStatuses", []))
            for pod in pods}


def gap(results, since):
    """Seconds from `since` until the first success after the last failure, and the failure count."""
    failed = [result for result in results if not result.ok and result.end >= since]
    if not failed:
        return 0.0, 0
    last = max(result.start for result in failed)
    back = min((result.start for result in results if result.ok and result.start > last), default=None)
    return (None if back is None else round(back - since, 1)), len(failed)


def report(forge, name, traffic, since, before, watch=None, note=""):
    titles = forge.issue_titles()
    acked = [write for write in traffic.writes if write.ok]
    missing = [write.number for write in acked if titles.get(write.number) != write.title]
    write_back, write_failed = gap(traffic.writes, since)
    read_back, read_failed = gap(traffic.reads, since)
    after = web_restarts()
    restarts = sum(after[pod] - before[pod] for pod in after if pod in before)
    slowest = max((write.end - write.start for write in acked), default=0)
    promoted = ""
    if watch and len(watch.changes) > 1:
        moment, primary = watch.changes[-1]
        promoted = f", operator named {primary} primary after {moment - since:.1f} s"
    duration = max(read.end for read in traffic.reads) - min(read.start for read in traffic.reads)
    errors = collections.Counter(str(write.status) for write in traffic.writes if not write.ok).most_common(3)
    print(f"{name}: writes back after {write_back} s ({write_failed} failed of {len(traffic.writes)} {errors}, slowest ack "
          f"{slowest:.1f} s), {len(acked)} acknowledged, {len(missing)} missing {missing[:10]}; reads back after "
          f"{read_back} s ({read_failed} failed of {len(traffic.reads)}, {len(traffic.reads) / duration:.0f}/s); "
          f"web container restarts {restarts}{promoted}{note}", flush=True)
    expect(not missing, f"{name}: acknowledged issues are missing")


def wait_cluster_whole():
    instances = pg_cluster()["spec"]["instances"]
    wait_for("PostgreSQL back to full strength", lambda: pg_cluster()["status"].get("readyInstances") == instances, 600, 5)


def step_kill(forge):
    primary = pg_cluster()["status"]["currentPrimary"]
    before = web_restarts()
    with Traffic(forge, "kill") as traffic, PrimaryWatch() as watch:
        time.sleep(10)
        since = time.time()
        # A one-second grace period stops PostgreSQL like a crash. A plain delete waits for
        # open sessions (PgBouncer keeps them) up to the operator's three-minute smart
        # shutdown before failing over; the drain step covers that graceful path.
        log(f"deleting the primary pod {primary} with a 1 s grace period")
        kubectl("delete", "pod", primary, "--grace-period=1", "--wait=false")

        def promoted_and_writing():
            if not watch.changes or watch.changes[-1][1] == primary:
                return False
            return traffic.recovered(watch.changes[-1][0])
        wait_for("a new primary and writes after it", promoted_and_writing, 400, 1)
    report(forge, "kill-primary", traffic, since, before, watch)
    wait_cluster_whole()


def step_drain(forge):
    primary = pg_cluster()["status"]["currentPrimary"]
    node = kube_json("get", "pod", primary)["spec"]["nodeName"]
    # Draining evicts every pod on the node; only go ahead when they are all ours.
    others = [f"{pod['metadata']['namespace']}/{pod['metadata']['name']}"
              for pod in kube_json("get", "pods", "--all-namespaces", "--field-selector",
                                   f"spec.nodeName={node}", namespaced=False)["items"]
              if pod["metadata"]["namespace"] != NS
              and not any(owner["kind"] == "DaemonSet" for owner in pod["metadata"].get("ownerReferences", []))]
    expect(not others, f"{node} also runs {', '.join(others)}; not draining it")
    before = web_restarts()
    drained = None
    try:
        with Traffic(forge, "drain") as traffic, PrimaryWatch() as watch:
            time.sleep(10)
            since = time.time()
            log(f"cordoning and draining {node}, which runs the primary {primary}")
            kubectl("cordon", node, namespaced=False)
            drain = subprocess.Popen(kubectl_command(False) + ["drain", node, "--ignore-daemonsets",
                                                               "--delete-emptydir-data", "--timeout=270s"],
                                     stdout=subprocess.DEVNULL, stderr=subprocess.PIPE, text=True)
            wait_for("drain and writes after it", lambda: drain.poll() is not None and time.time() > since + 30
                     and traffic.recovered(since), 290, 1)
            drained = time.time() - since
            expect(drain.returncode == 0, f"drain {node}: {drain.stderr.read()[-500:]}")
    finally:
        kubectl("uncordon", node, namespaced=False)
    report(forge, "drain-node", traffic, since, before, watch, f", drain finished after {drained:.1f} s")
    wait_cluster_whole()
    kubectl("wait", "--for=condition=Ready", "pod", "-l", "app.kubernetes.io/component=storage", "--timeout=280s")
    kubectl("rollout", "status", "deployment/codeyard-web", "--timeout=280s")


def step_restart(forge):
    before = web_restarts()
    with Traffic(forge, "restart", readers=4, read_interval=0.25) as traffic:
        time.sleep(5)
        since = time.time()
        kubectl("rollout", "restart", "deployment/codeyard-web")
        kubectl("rollout", "status", "deployment/codeyard-web", "--timeout=280s")
        took = time.time() - since
        time.sleep(5)
    report(forge, "rolling-restart", traffic, since, before, note=f", rollout took {took:.1f} s")


def step_scale(forge):
    before = web_restarts()
    with Traffic(forge, "scale", readers=8, read_interval=0.05) as traffic:
        time.sleep(15)
        since = time.time()
        kubectl("scale", "deployment/codeyard-web", "--replicas=4")
        kubectl("rollout", "status", "deployment/codeyard-web", "--timeout=280s")
        took = time.time() - since
        time.sleep(20)
    def latency(results, low, high):
        values = sorted(result.end - result.start for result in results if low <= result.start < high and result.ok)
        return f"p50 {values[len(values) // 2] * 1000:.0f} ms p95 {values[int(len(values) * 0.95)] * 1000:.0f} ms" if values else "none"
    note = (f", 4 replicas ready after {took:.1f} s; reads at 2 replicas {latency(traffic.reads, 0, since)}, "
            f"at 4 {latency(traffic.reads, since + took, float('inf'))}")
    report(forge, "scale-2-to-4", traffic, since, before, note=note)
    kubectl("scale", "deployment/codeyard-web", "--replicas=2")


STEPS = {"basic": step_basic, "kill": step_kill, "drain": step_drain, "restart": step_restart, "scale": step_scale}


def cmd_test(names):
    names = names or list(STEPS)
    expect(all(name in STEPS for name in names), f"steps are {', '.join(STEPS)}")
    host = kubectl("get", "ingress", RELEASE, "-o", "jsonpath={.spec.rules[0].host}")
    with tempfile.TemporaryDirectory() as work:
        forge = Forge(f"http://{host}", work)
        forge.sign_in()
        if "basic" not in names:
            names.insert(0, "basic")
        try:
            for name in names:
                log(f"step {name}")
                STEPS[name](forge)
        finally:
            forge.revoke_tokens()


def cmd_down():
    kubectl("delete", "namespace", NS, "--wait=false", namespaced=False)
    log(f"deleting {NS}; remove the operator with: kubectl delete -f {CNPG_MANIFEST}")


def main(argv):
    if len(argv) >= 2 and argv[0] == "image":
        cmd_image(argv[1])
    elif len(argv) >= 2 and argv[0] == "up":
        cmd_up(argv[1])
    elif argv[:1] == ["test"]:
        cmd_test(argv[1:])
    elif argv == ["down"]:
        cmd_down()
    else:
        print(__doc__, file=sys.stderr)
        return 2
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
