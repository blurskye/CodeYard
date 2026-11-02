"""Actions/settings sweep: disposable repositories, no marketplace actions or shared names.

CI cases require the live server's normal self-hosted runner and object storage.
Fixtures use REST; only literal gh invocations in registered cases count as coverage.
"""
import base64
import json
import shlex
import time

from gh_sweep.harness import case, expect_error, expect_json, expect_ok, expect_unsupported


MISSING_ID = "9223372036854775807"


def _missing(ctx):
    return ctx.account.name + "/" + ctx.name("missing")


def _name(ctx):
    return ctx.name("value").replace("-", "_").upper()


def _other(ctx):
    return next(account for account in ctx.sweep.accounts if account != ctx.account)


def _foreign(ctx, private=False):
    return ctx.sweep.create_repo(_other(ctx), ctx.name("foreign"), private=private)["full_name"]


def _client_error(result, text):
    assert not result.timed_out and result.rc > 0, result
    assert text.lower() in result.stderr.lower(), result.stderr
    assert "panic:" not in result.stderr.lower(), result.stderr


def _no_workflow(result):
    """gh rewrites a workflow 404 as "workflow X not found on the default branch"."""
    return expect_error(result, status=404, contains="not found on the default branch")


def _env(ctx):
    name = ctx.name("environment")
    ctx.api("PUT", f"repos/{ctx.repo}/environments/{name}", {})
    return name


def _variable(ctx, name=None, value="sweep value", *, env=None, org=None):
    name = name or _name(ctx)
    path = (f"orgs/{org}/actions" if org else
            f"repos/{ctx.repo}/environments/{env}" if env else f"repos/{ctx.repo}/actions")
    data = {"name": name, "value": value}
    if org:
        data["visibility"] = "private"
    ctx.api("POST", path + "/variables", data)
    return name


def _values(ctx, plural, *, env=None, org=None):
    path = (f"orgs/{org}/actions" if org else
            f"repos/{ctx.repo}/environments/{env}" if env else f"repos/{ctx.repo}/actions")
    return ctx.api("GET", path + "/" + plural)[plural]


def _secret_present(ctx, name, *, env=None, org=None):
    rows = _values(ctx, "secrets", env=env, org=org)
    assert any(row["name"] == name and row["updated_at"] for row in rows), rows
    assert all("value" not in row and "encrypted_value" not in row for row in rows), rows


# A workflow step uploads real objects with the runner's own credential. No PAT is
# embedded, and no external marketplace action/network service is needed.
_OBJECT_STEP = r'''
import base64, io, json, os, urllib.request, zipfile
url = os.environ['ACTIONS_RESULTS_URL'].rstrip('/')
token = os.environ['ACTIONS_RUNTIME_TOKEN']
claims = json.loads(base64.urlsafe_b64decode(token.split('.')[1] + '==='))
backend = claims['scp'].split(':')[1]
def rpc(service, method, data):
    request = urllib.request.Request(url + '/twirp/github.actions.results.api.v1.' + service + '/' + method,
        data=json.dumps(data).encode(), headers={'Authorization': 'Bearer ' + token, 'Content-Type': 'application/json'})
    with urllib.request.urlopen(request, timeout=20) as response:
        return json.load(response)
def upload(service, create, finish, data, payload):
    result = rpc(service, create, data)
    assert result['ok'], result
    request = urllib.request.Request(result['signedUploadUrl'], data=payload, method='PUT',
        headers={'x-ms-blob-type': 'BlockBlob'})
    with urllib.request.urlopen(request, timeout=20) as response:
        assert response.status in (200, 201)
    data.update(size=str(len(payload)), sizeBytes=str(len(payload)))
    assert rpc(service, finish, data)['ok']
for index in range(3):
    upload('CacheService', 'CreateCacheEntry', 'FinalizeCacheEntryUpload',
        {'key': 'sweep-cache-' + str(index), 'version': 'sweep-v1'}, b'x' * (index + 1))
for name in ('sweep-artifact-a', 'sweep-artifact-b'):
    stream = io.BytesIO()
    with zipfile.ZipFile(stream, 'w') as archive:
        archive.writestr('proof.txt', name + '\n')
    upload('ArtifactService', 'CreateArtifact', 'FinalizeArtifact',
        {'name': name, 'version': 4, 'workflowRunBackendId': backend, 'workflowJobRunBackendId': backend}, stream.getvalue())
print('SWEEP_OBJECTS_OK')
'''


def _workflow(ctx, *, outcome="success", objects=False, dispatch=True):
    filename = ctx.name("workflow") + ".yml"
    title = ctx.name("workflow-title")
    event = ("on:\n  workflow_dispatch:\n    inputs:\n      message:\n        description: Sweep marker\n"
             "        type: string\n        default: default-marker\n" if dispatch else "on: push\n")
    command = ("sleep 90" if outcome == "slow" else "sleep 2; echo SWEEP_SUCCESS" if outcome == "short" else
               "echo SWEEP_FAILURE; exit 1" if outcome == "failure" else "echo SWEEP_SUCCESS")
    cache_prefix, artifact_prefix = ctx.name("cache") + "-", ctx.name("artifact") + "-"
    if objects:
        script = _OBJECT_STEP.replace("sweep-cache-", cache_prefix).replace("sweep-artifact-", artifact_prefix)
        command = "python3 -c " + shlex.quote(script)
    if dispatch and outcome not in ("slow", "failure"):
        command += "\nprintf '%s\\n' \"$SWEEP_MESSAGE\""
    source = (f"name: {title}\n" + event + "jobs:\n  proof:\n    runs-on: self-hosted\n"
              "    steps:\n      - name: Sweep proof step\n"
              "        env:\n          SWEEP_MESSAGE: ${{ inputs.message }}\n        run: |\n" +
              "\n".join("          " + line for line in command.splitlines()) + "\n")
    branch = ctx.api("GET", f"repos/{ctx.repo}")["default_branch"]
    commit = ctx.api("PUT", f"repos/{ctx.repo}/contents/.github/workflows/{filename}",
                     {"message": ctx.name("workflow-commit"), "content": base64.b64encode(source.encode()).decode(),
                      "branch": branch})["commit"]["sha"]
    workflows = ctx.api("GET", f"repos/{ctx.repo}/actions/workflows")["workflows"]
    workflow = next(row for row in workflows if row["path"].endswith("/" + filename))
    return {**workflow, "filename": filename, "source": source, "branch": branch, "sha": commit,
            "cache_prefix": cache_prefix, "artifact_prefix": artifact_prefix}


def _dispatch(ctx, workflow):
    ctx.needs_runs()
    result = ctx.api("POST", f"repos/{ctx.repo}/actions/workflows/{workflow['id']}/dispatches",
                     {"ref": workflow["branch"], "return_run_details": True})
    return str(result["workflow_run_id"])


def _wait(ctx, run_id):
    ctx.needs_runs()
    # Bounded independently of the harness deadline, including queue starvation.
    deadline = min(ctx.deadline - 5, time.monotonic() + 100)
    while time.monotonic() < deadline:
        row = ctx.api("GET", f"repos/{ctx.repo}/actions/runs/{run_id}")
        if row["status"] == "completed":
            return row
        time.sleep(0.5)
    raise AssertionError(f"run {run_id} did not complete within 100 seconds")


def _finished(ctx, *, outcome="success", objects=False):
    workflow = _workflow(ctx, outcome=outcome, objects=objects)
    run_id = _dispatch(ctx, workflow)
    run = _wait(ctx, run_id)
    assert run["conclusion"] == outcome, run
    return workflow, run_id, run


def _foreign_finished(ctx):
    """An existing public run with a different owner, for write-denial tests."""
    ctx.needs_runs()
    template = _workflow(ctx)
    other, repo = _other(ctx), _foreign(ctx)
    filename = ctx.name("foreign-workflow") + ".yml"
    ctx.sweep.api(other, "PUT", f"repos/{repo}/contents/.github/workflows/{filename}",
                  {"message": "fixture", "content": base64.b64encode(template["source"].encode()).decode()})
    branch = ctx.sweep.api(other, "GET", f"repos/{repo}")["default_branch"]
    run_id = str(ctx.sweep.api(other, "POST", f"repos/{repo}/actions/workflows/{filename}/dispatches",
                             {"ref": branch, "return_run_details": True})["workflow_run_id"])
    deadline = min(ctx.deadline - 5, time.monotonic() + 100)
    while time.monotonic() < deadline:
        row = ctx.sweep.api(other, "GET", f"repos/{repo}/actions/runs/{run_id}")
        if row["status"] == "completed":
            assert row["conclusion"] == "success", row
            return repo, run_id
        time.sleep(0.5)
    raise AssertionError("foreign permission fixture did not finish")


def _foreign_env(ctx):
    repo, env = _foreign(ctx), ctx.name("foreign-environment")
    ctx.sweep.api(_other(ctx), "PUT", f"repos/{repo}/environments/{env}", {})
    return repo, env


def _jobs(ctx, run_id):
    rows = ctx.api("GET", f"repos/{ctx.repo}/actions/runs/{run_id}/jobs")["jobs"]
    assert rows, rows
    return rows


@case(command="cache list", variant="empty, missing repo, and invalid sorting", needs=("repo",))
def cache_list_empty(ctx):
    expect_json(ctx.gh("cache", "list", "--repo", ctx.repo, "--json", "id,key,ref,sizeInBytes"), [])
    result = expect_ok(ctx.gh("cache", "list", "--repo", ctx.repo))
    text = (result.stdout + result.stderr).strip().lower()
    assert not text or "no caches" in text, result
    expect_unsupported(ctx.gh("cache", "list", "--repo", _foreign(ctx, private=True), ok=None))
    expect_unsupported(ctx.gh("cache", "list", "--repo", _missing(ctx), ok=None))
    _client_error(ctx.gh("cache", "list", "--repo", ctx.repo, "--sort", "invalid", ok=None), "invalid")
    _client_error(ctx.gh("cache", "list", "--repo", ctx.repo, "--order", "invalid", ok=None), "invalid")


@case(command="cache list", variant="populated filters, pagination, sorting, jq and template", needs=("repo",), timeout=240)
def cache_list_objects(ctx):
    workflow, _, _ = _finished(ctx, objects=True)
    rows = expect_json(ctx.gh("cache", "list", "--repo", ctx.repo, "--json",
                             "id,key,ref,sizeInBytes,createdAt,lastAccessedAt,version"),
                       [{"id": int, "key": str, "ref": "refs/heads/" + workflow["branch"],
                         "sizeInBytes": int, "createdAt": str, "lastAccessedAt": str, "version": str}])
    prefix = workflow["cache_prefix"]
    assert {row["key"] for row in rows} == {prefix + "0", prefix + "1", prefix + "2"}, rows
    for sort in ("size_in_bytes", "created_at", "last_accessed_at"):
        ascending = expect_json(ctx.gh("cache", "list", "--repo", ctx.repo, "--key", prefix,
                                      "--ref", "refs/heads/" + workflow["branch"], "--limit", "2",
                                      "--sort", sort, "--order", "asc", "--json", "key,sizeInBytes"),
                                [{"key": str, "sizeInBytes": int}])
        assert len(ascending) == 2, ascending
        if sort == "size_in_bytes":
            assert [row["sizeInBytes"] for row in ascending] == [1, 2], ascending
    descending = expect_json(ctx.gh("cache", "list", "--repo", ctx.repo, "--sort", "size_in_bytes",
                                   "--order", "desc", "--json", "sizeInBytes"), [{"sizeInBytes": int}])
    assert [row["sizeInBytes"] for row in descending] == [3, 2, 1], descending
    assert expect_ok(ctx.gh("cache", "list", "--repo", ctx.repo, "--key", prefix + "0",
                            "--json", "key", "--jq", ".[0].key")).stdout.strip() == prefix + "0"
    assert expect_ok(ctx.gh("cache", "list", "--repo", ctx.repo, "--json", "key", "--template",
                            "{{len .}}\n")).stdout.strip() == "3"
    expect_json(ctx.gh("cache", "list", "--repo", ctx.repo, "--key", ctx.name("absent"), "--json", "id"), [])
    expect_json(ctx.gh("cache", "list", "--repo", ctx.repo, "--ref", "refs/heads/" + ctx.name("absent"),
                       "--json", "id"), [])


@case(command="cache delete", variant="empty --all and succeed-on-no-caches, missing id/key", needs=("repo",))
def cache_delete_empty(ctx):
    result = ctx.gh("cache", "delete", "--repo", ctx.repo, "--all", ok=None)
    _client_error(result, "no caches")
    # The success note is for terminals only.
    result = expect_ok(ctx.gh("cache", "delete", "--repo", ctx.repo, "--all", "--succeed-on-no-caches"))
    assert result.stdout == "", result
    # gh rewrites a 404 for an id or key as "Could not find a cache matching ...".
    _client_error(ctx.gh("cache", "delete", MISSING_ID, "--repo", ctx.repo, ok=None), "Could not find a cache matching")
    _client_error(ctx.gh("cache", "delete", ctx.name("cache"), "--ref", "refs/heads/main",
                         "--repo", ctx.repo, ok=None), "Could not find a cache matching")
    expect_unsupported(ctx.gh("cache", "delete", "--all", "--repo", _missing(ctx), ok=None))
    _client_error(ctx.gh("cache", "delete", "--repo", ctx.repo, "--succeed-on-no-caches", ok=None), "--all")


@case(command="cache delete", variant="by id, key/ref and all/ref, permission", needs=("repo",), timeout=240)
def cache_delete_objects(ctx):
    workflow, _, _ = _finished(ctx, objects=True)
    rows = ctx.api("GET", f"repos/{ctx.repo}/actions/caches")["actions_caches"]
    prefix = workflow["cache_prefix"]
    victim = next(row for row in rows if row["key"] == prefix + "0")
    wrong_ref = "refs/heads/" + ctx.name("absent")
    _client_error(ctx.gh("cache", "delete", prefix + "0", "--repo", ctx.repo, "--ref", wrong_ref, ok=None),
                  "Could not find a cache matching")
    _client_error(ctx.gh("cache", "delete", "--all", "--repo", ctx.repo, "--ref", wrong_ref, ok=None), "no caches")
    expect_ok(ctx.gh("cache", "delete", "--all", "--repo", ctx.repo, "--ref", wrong_ref, "--succeed-on-no-caches"))
    assert len(ctx.api("GET", f"repos/{ctx.repo}/actions/caches")["actions_caches"]) == 3
    expect_ok(ctx.gh("cache", "delete", str(victim["id"]), "--repo", ctx.repo))
    rows = ctx.api("GET", f"repos/{ctx.repo}/actions/caches")["actions_caches"]
    assert len(rows) == 2 and all(row["id"] != victim["id"] for row in rows), rows
    expect_ok(ctx.gh("cache", "delete", prefix + "1", "--repo", ctx.repo,
                     "--ref", "refs/heads/" + workflow["branch"]))
    assert len(ctx.api("GET", f"repos/{ctx.repo}/actions/caches")["actions_caches"]) == 1
    expect_ok(ctx.gh("cache", "delete", "--all", "--repo", ctx.repo,
                     "--ref", "refs/heads/" + workflow["branch"]))
    assert ctx.api("GET", f"repos/{ctx.repo}/actions/caches")["actions_caches"] == []
    expect_error(ctx.gh("cache", "delete", prefix + "0", "--repo", _foreign(ctx), ok=None), status=403)


@case(command="workflow list", variant="empty and 404", needs=("repo",))
def workflow_list_empty(ctx):
    # An empty list prints nothing and succeeds off a terminal, even with --json.
    result = expect_ok(ctx.gh("workflow", "list", "--repo", ctx.repo, "--json", "id,name,path,state"))
    assert result.stdout == "", result
    expect_unsupported(ctx.gh("workflow", "list", "--repo", _missing(ctx), ok=None))


@case(command="workflow list", variant="active versus disabled, limit, jq, template", needs=("repo",))
def workflow_list_states(ctx):
    first, second = _workflow(ctx), _workflow(ctx)
    ctx.api("PUT", f"repos/{ctx.repo}/actions/workflows/{second['id']}/disable")
    rows = expect_json(ctx.gh("workflow", "list", "--repo", ctx.repo, "--json", "id,name,path,state"),
                       [{"id": int, "name": str, "path": str, "state": "active"}])
    assert [row["id"] for row in rows] == [first["id"]], rows
    rows = expect_json(ctx.gh("workflow", "list", "--repo", ctx.repo, "--all", "--json", "id,state"),
                       [{"id": int, "state": str}])
    assert {row["id"]: row["state"] for row in rows} == {first["id"]: "active", second["id"]: "disabled_manually"}, rows
    limited = expect_json(ctx.gh("workflow", "list", "--repo", ctx.repo, "--all", "--limit", "1", "--json", "id"),
                          [{"id": int}])
    assert len(limited) == 1, limited
    assert expect_ok(ctx.gh("workflow", "list", "--repo", ctx.repo, "--json", "name", "--jq", ".[0].name")).stdout.strip() == first["name"]
    assert expect_ok(ctx.gh("workflow", "list", "--repo", ctx.repo, "--all", "--json", "id", "--template", "{{len .}}\n")).stdout.strip() == "2"


@case(command="workflow disable", variant="id, filename and name; 404 and denied", needs=("repo",))
def workflow_disable(ctx):
    workflow = _workflow(ctx)
    expect_ok(ctx.gh("workflow", "disable", str(workflow["id"]), "--repo", ctx.repo))
    assert ctx.api("GET", f"repos/{ctx.repo}/actions/workflows/{workflow['id']}")["state"] == "disabled_manually"
    expect_ok(ctx.gh("workflow", "disable", workflow["filename"], "--repo", ctx.repo))
    # By name gh looks only among active workflows.
    ctx.api("PUT", f"repos/{ctx.repo}/actions/workflows/{workflow['id']}/enable")
    expect_ok(ctx.gh("workflow", "disable", workflow["name"], "--repo", ctx.repo))
    assert ctx.api("GET", f"repos/{ctx.repo}/actions/workflows/{workflow['id']}")["state"] == "disabled_manually"
    _no_workflow(ctx.gh("workflow", "disable", MISSING_ID, "--repo", ctx.repo, ok=None))
    _no_workflow(ctx.gh("workflow", "disable", MISSING_ID, "--repo", _missing(ctx), ok=None))
    foreign = _foreign(ctx)
    # Seed the foreign repository as its owner, never mutate an existing shared repo.
    other = _other(ctx)
    ctx.sweep.api(other, "PUT", f"repos/{foreign}/contents/.github/workflows/proof.yml",
                  {"message": "fixture", "content": base64.b64encode(workflow["source"].encode()).decode()})
    expect_error(ctx.gh("workflow", "disable", "proof.yml", "--repo", foreign, ok=None), status=403)


@case(command="workflow enable", variant="disabled to active, name and missing/denied", needs=("repo",))
def workflow_enable(ctx):
    workflow = _workflow(ctx)
    ctx.api("PUT", f"repos/{ctx.repo}/actions/workflows/{workflow['id']}/disable")
    expect_ok(ctx.gh("workflow", "enable", workflow["filename"], "--repo", ctx.repo))
    assert ctx.api("GET", f"repos/{ctx.repo}/actions/workflows/{workflow['id']}")["state"] == "active"
    expect_ok(ctx.gh("workflow", "enable", str(workflow["id"]), "--repo", ctx.repo))
    # By name gh looks only among disabled workflows.
    ctx.api("PUT", f"repos/{ctx.repo}/actions/workflows/{workflow['id']}/disable")
    expect_ok(ctx.gh("workflow", "enable", workflow["name"], "--repo", ctx.repo))
    assert ctx.api("GET", f"repos/{ctx.repo}/actions/workflows/{workflow['id']}")["state"] == "active"
    _no_workflow(ctx.gh("workflow", "enable", MISSING_ID, "--repo", ctx.repo, ok=None))
    _no_workflow(ctx.gh("workflow", "enable", MISSING_ID, "--repo", _missing(ctx), ok=None))
    foreign = _foreign(ctx)
    ctx.sweep.api(_other(ctx), "PUT", f"repos/{foreign}/contents/.github/workflows/proof.yml",
                  {"message": "fixture", "content": base64.b64encode(workflow["source"].encode()).decode()})
    expect_error(ctx.gh("workflow", "enable", "proof.yml", "--repo", foreign, ok=None), status=403)


@case(command="workflow view", variant="summary, yaml/ref, id/name/file, missing", needs=("repo",))
def workflow_view(ctx):
    workflow = _workflow(ctx)
    summary = expect_ok(ctx.gh("workflow", "view", workflow["filename"], "--repo", ctx.repo)).stdout
    assert workflow["name"] in summary and workflow["filename"] in summary, summary
    source = expect_ok(ctx.gh("workflow", "view", str(workflow["id"]), "--repo", ctx.repo,
                              "--yaml", "--ref", workflow["branch"])).stdout
    assert source.strip() == workflow["source"].strip(), source
    assert workflow["name"] in expect_ok(ctx.gh("workflow", "view", workflow["name"], "--repo", ctx.repo)).stdout
    _no_workflow(ctx.gh("workflow", "view", MISSING_ID, "--repo", ctx.repo, ok=None))
    _no_workflow(ctx.gh("workflow", "view", MISSING_ID, "--repo", _missing(ctx), ok=None))
    _client_error(ctx.gh("workflow", "view", workflow["filename"], "--yaml", "--ref", ctx.name("absent"),
                         "--repo", ctx.repo, ok=None), "could not find workflow file")


@case(command="workflow run", variant="default ref, raw/typed/file fields and JSON stdin", needs=("repo",), timeout=240)
def workflow_run_inputs(ctx):
    workflow = _workflow(ctx)
    expect_ok(ctx.gh("workflow", "run", workflow["filename"], "--repo", ctx.repo))
    marker = ctx.name("input")
    value_file = ctx.directory / "input.txt"
    value_file.write_text(marker)
    expect_ok(ctx.gh("workflow", "run", workflow["name"], "--repo", ctx.repo, "--ref", workflow["branch"],
                     "--raw-field", "message=" + marker))
    expect_ok(ctx.gh("workflow", "run", str(workflow["id"]), "--repo", ctx.repo, "--field", "message=@" + str(value_file)))
    expect_ok(ctx.gh("workflow", "run", workflow["filename"], "--repo", ctx.repo, "--field", "message=" + marker))
    expect_ok(ctx.gh("workflow", "run", workflow["filename"], "--repo", ctx.repo, "--json",
                     input=json.dumps({"message": marker})))
    runs = ctx.api("GET", f"repos/{ctx.repo}/actions/workflows/{workflow['id']}/runs")["workflow_runs"]
    assert len(runs) == 5 and all(row["event"] == "workflow_dispatch" and row["head_sha"] == workflow["sha"] for row in runs), runs
    logs = []
    for row in runs:
        assert _wait(ctx, str(row["id"]))["conclusion"] == "success"
        logs.append(expect_ok(ctx.gh("run", "view", str(row["id"]), "--repo", ctx.repo, "--log")).stdout)
    assert sum(marker in log for log in logs) == 4, logs
    assert sum("default-marker" in log for log in logs) == 1, logs


@case(command="workflow run", variant="missing, invalid ref/inputs/JSON, disabled, no dispatch, denied", needs=("repo",))
def workflow_run_errors(ctx):
    workflow = _workflow(ctx)
    _no_workflow(ctx.gh("workflow", "run", MISSING_ID, "--repo", ctx.repo, ok=None))
    # Without --ref gh first reads the repository's default branch over GraphQL.
    _client_error(ctx.gh("workflow", "run", MISSING_ID, "--repo", _missing(ctx), ok=None),
                  "Could not resolve to a Repository")
    expect_error(ctx.gh("workflow", "run", workflow["filename"], "--repo", ctx.repo, "--ref", ctx.name("absent"), "--raw-field", "message=bad-ref", ok=None), status=422)
    expect_error(ctx.gh("workflow", "run", workflow["filename"], "--repo", ctx.repo,
                        "--raw-field", "unknown=value", ok=None), status=422)
    _client_error(ctx.gh("workflow", "run", workflow["filename"], "--repo", ctx.repo, "--json", input="{", ok=None), "JSON")
    ctx.api("PUT", f"repos/{ctx.repo}/actions/workflows/{workflow['id']}/disable")
    expect_error(ctx.gh("workflow", "run", str(workflow["id"]), "--repo", ctx.repo, ok=None), status=409)
    no_dispatch = _workflow(ctx, dispatch=False)
    _client_error(ctx.gh("workflow", "run", no_dispatch["filename"], "--repo", ctx.repo, ok=None), "workflow_dispatch")
    foreign = _foreign(ctx)
    ctx.sweep.api(_other(ctx), "PUT", f"repos/{foreign}/contents/.github/workflows/proof.yml",
                  {"message": "fixture", "content": base64.b64encode(workflow["source"].encode()).decode()})
    expect_error(ctx.gh("workflow", "run", "proof.yml", "--repo", foreign, ok=None), status=403)


@case(command="run list", variant="empty and missing repo", needs=("repo",))
def run_list_empty(ctx):
    expect_json(ctx.gh("run", "list", "--repo", ctx.repo, "--json", "databaseId,status"), [])
    expect_unsupported(ctx.gh("run", "list", "--repo", _missing(ctx), ok=None))
    expect_unsupported(ctx.gh("run", "list", "--repo", _foreign(ctx, private=True), ok=None))
    _client_error(ctx.gh("run", "list", "--repo", ctx.repo, "--status", "invalid", ok=None), "invalid")
    expect_error(ctx.gh("run", "list", "--repo", ctx.repo, "--created", "not-a-date", ok=None), status=422)


@case(command="run list", variant="filters, disabled workflow/all, jq/template", needs=("repo",), timeout=240)
def run_list_filters(ctx):
    workflow, run_id, run = _finished(ctx)
    rows = expect_json(ctx.gh("run", "list", "--repo", ctx.repo, "--branch", workflow["branch"],
                             "--commit", workflow["sha"], "--created", ">=2000-01-01", "--event", "workflow_dispatch",
                             "--status", "success", "--user", ctx.account.name, "--workflow", workflow["filename"],
                             "--limit", "1", "--json", "databaseId,event,headSha,headBranch,status,conclusion,workflowName,attempt"),
                       [{"databaseId": int, "event": "workflow_dispatch", "headSha": workflow["sha"],
                         "headBranch": workflow["branch"], "status": "completed", "conclusion": "success",
                         "workflowName": workflow["name"], "attempt": 1}])
    assert len(rows) == 1 and str(rows[0]["databaseId"]) == run_id, rows
    assert expect_ok(ctx.gh("run", "list", "--repo", ctx.repo, "--json", "databaseId", "--jq", ".[0].databaseId")).stdout.strip() == run_id
    assert expect_ok(ctx.gh("run", "list", "--repo", ctx.repo, "--json", "status", "--template", "{{(index . 0).status}}\n")).stdout.strip() == "completed"
    expect_json(ctx.gh("run", "list", "--repo", ctx.repo, "--branch", ctx.name("absent"), "--json", "databaseId"), [])
    expect_json(ctx.gh("run", "list", "--repo", ctx.repo, "--status", "failure", "--json", "databaseId"), [])
    expect_json(ctx.gh("run", "list", "--repo", ctx.repo, "--event", "pull_request", "--json", "databaseId"), [])
    expect_json(ctx.gh("run", "list", "--repo", ctx.repo, "--commit", "0" * 40, "--json", "databaseId"), [])
    expect_json(ctx.gh("run", "list", "--repo", ctx.repo, "--created", "2000-01-01..2000-01-02", "--json", "databaseId"), [])
    expect_json(ctx.gh("run", "list", "--repo", ctx.repo, "--user", _other(ctx).name, "--json", "databaseId"), [])
    ctx.api("PUT", f"repos/{ctx.repo}/actions/workflows/{workflow['id']}/disable")
    _client_error(ctx.gh("run", "list", "--repo", ctx.repo, "--workflow", workflow["name"], ok=None), "workflow")
    rows = expect_json(ctx.gh("run", "list", "--repo", ctx.repo, "--workflow", workflow["name"], "--all",
                             "--json", "databaseId"), [{"databaseId": int}])
    assert [str(row["databaseId"]) for row in rows] == [run_id], rows


@case(command="run cancel", variant="normal and forced cancellation, completed conflict, missing/denied", needs=("repo",), timeout=240)
def run_cancel(ctx):
    workflow = _workflow(ctx, outcome="slow")
    run_id = _dispatch(ctx, workflow)
    expect_ok(ctx.gh("run", "cancel", run_id, "--repo", ctx.repo))
    assert _wait(ctx, run_id)["conclusion"] == "cancelled"
    run_id = _dispatch(ctx, workflow)
    expect_ok(ctx.gh("run", "cancel", run_id, "--force", "--repo", ctx.repo))
    assert _wait(ctx, run_id)["conclusion"] == "cancelled"
    expect_error(ctx.gh("run", "cancel", run_id, "--repo", ctx.repo, ok=None), status=409)
    expect_error(ctx.gh("run", "cancel", run_id, "--force", "--repo", ctx.repo, ok=None), status=409)
    expect_unsupported(ctx.gh("run", "cancel", MISSING_ID, "--repo", ctx.repo, ok=None))
    expect_unsupported(ctx.gh("run", "cancel", MISSING_ID, "--force", "--repo", _missing(ctx), ok=None))
    # A real foreign run exercises authority rather than just missing-object lookup.
    other, foreign = _other(ctx), _foreign(ctx)
    ctx.sweep.api(other, "PUT", f"repos/{foreign}/contents/.github/workflows/proof.yml",
                  {"message": "fixture", "content": base64.b64encode(workflow["source"].encode()).decode()})
    branch = ctx.sweep.api(other, "GET", f"repos/{foreign}")["default_branch"]
    foreign_run = ctx.sweep.api(other, "POST", f"repos/{foreign}/actions/workflows/proof.yml/dispatches",
                               {"ref": branch, "return_run_details": True})["workflow_run_id"]
    try:
        expect_error(ctx.gh("run", "cancel", str(foreign_run), "--repo", foreign, ok=None), status=403)
    finally:
        ctx.sweep.api(other, "POST", f"repos/{foreign}/actions/runs/{foreign_run}/cancel")


@case(command="run delete", variant="completed run deletion, repeated/missing, denied", needs=("repo",), timeout=240)
def run_delete(ctx):
    _, run_id, _ = _finished(ctx)
    expect_ok(ctx.gh("run", "delete", run_id, "--repo", ctx.repo))
    rows = ctx.api("GET", f"repos/{ctx.repo}/actions/runs")["workflow_runs"]
    assert all(str(row["id"]) != run_id for row in rows), rows
    expect_unsupported(ctx.gh("run", "delete", run_id, "--repo", ctx.repo, ok=None))
    expect_unsupported(ctx.gh("run", "delete", MISSING_ID, "--repo", _missing(ctx), ok=None))
    expect_unsupported(ctx.gh("run", "delete", MISSING_ID, "--repo", _foreign(ctx, private=True), ok=None))
    foreign, foreign_run = _foreign_finished(ctx)
    expect_error(ctx.gh("run", "delete", foreign_run, "--repo", foreign, ok=None), status=403)


@case(command="run download", variant="all, named, repeated names, pattern, custom directories", needs=("repo",), timeout=240)
def run_download(ctx):
    workflow, run_id, _ = _finished(ctx, objects=True)
    first, second = workflow["artifact_prefix"] + "a", workflow["artifact_prefix"] + "b"
    all_dir = ctx.directory / ctx.name("all")
    expect_ok(ctx.gh("run", "download", run_id, "--repo", ctx.repo, "--dir", str(all_dir)))
    for name in (first, second):
        assert (all_dir / name / "proof.txt").read_text() == name + "\n"
    one_dir = ctx.directory / ctx.name("named")
    expect_ok(ctx.gh("run", "download", run_id, "--repo", ctx.repo, "--name", first, "--dir", str(one_dir)))
    assert (one_dir / "proof.txt").read_text() == first + "\n"
    pattern_dir = ctx.directory / ctx.name("pattern")
    expect_ok(ctx.gh("run", "download", run_id, "--repo", ctx.repo, "--pattern", workflow["artifact_prefix"] + "*", "--dir", str(pattern_dir)))
    assert (pattern_dir / second / "proof.txt").read_text() == second + "\n"
    repeat_dir = ctx.directory / ctx.name("repeated")
    expect_ok(ctx.gh("run", "download", run_id, "--repo", ctx.repo, "--name", first,
                     "--name", second, "--dir", str(repeat_dir)))
    assert (repeat_dir / first / "proof.txt").read_text() == first + "\n"
    latest_dir = ctx.directory / ctx.name("latest")
    expect_ok(ctx.gh("run", "download", "--repo", ctx.repo, "--name", first, "--dir", str(latest_dir)))
    assert (latest_dir / "proof.txt").read_text() == first + "\n"
    _client_error(ctx.gh("run", "download", run_id, "--repo", ctx.repo, "--name", ctx.name("absent"), ok=None), "no artifact")
    _client_error(ctx.gh("run", "download", run_id, "--repo", ctx.repo, "--pattern", "absent-*", ok=None), "no artifact")


@case(command="run download", variant="empty artifacts and missing/private repo", needs=("repo",), timeout=240)
def run_download_errors(ctx):
    _, run_id, _ = _finished(ctx)
    _client_error(ctx.gh("run", "download", run_id, "--repo", ctx.repo, ok=None), "no artifact")
    expect_unsupported(ctx.gh("run", "download", MISSING_ID, "--repo", ctx.repo, ok=None))
    expect_unsupported(ctx.gh("run", "download", MISSING_ID, "--repo", _missing(ctx), ok=None))
    expect_unsupported(ctx.gh("run", "download", MISSING_ID, "--repo", _foreign(ctx, private=True), ok=None))


@case(command="run rerun", variant="whole run/debug, failed and specific job/debug", needs=("repo",), timeout=300)
def run_rerun(ctx):
    _, run_id, _ = _finished(ctx, outcome="failure")
    expect_ok(ctx.gh("run", "rerun", run_id, "--repo", ctx.repo))
    run = _wait(ctx, run_id)
    assert run["run_attempt"] == 2 and run["conclusion"] == "failure", run
    expect_ok(ctx.gh("run", "rerun", run_id, "--debug", "--repo", ctx.repo))
    run = _wait(ctx, run_id)
    assert run["run_attempt"] == 3, run
    expect_ok(ctx.gh("run", "rerun", run_id, "--failed", "--debug", "--repo", ctx.repo))
    run = _wait(ctx, run_id)
    assert run["run_attempt"] == 4 and run["conclusion"] == "failure", run
    expect_ok(ctx.gh("run", "rerun", run_id, "--failed", "--repo", ctx.repo))
    assert _wait(ctx, run_id)["run_attempt"] == 5
    job_id = str(_jobs(ctx, run_id)[0]["id"])
    expect_ok(ctx.gh("run", "rerun", run_id, "--job", job_id, "--debug", "--repo", ctx.repo))
    assert _wait(ctx, run_id)["run_attempt"] == 6
    job_id = str(_jobs(ctx, run_id)[0]["id"])
    expect_ok(ctx.gh("run", "rerun", run_id, "--job", job_id, "--repo", ctx.repo))
    assert _wait(ctx, run_id)["run_attempt"] == 7


@case(command="run rerun", variant="missing run/job/repo, invalid combinations and private repo", needs=("repo",), timeout=240)
def run_rerun_errors(ctx):
    expect_unsupported(ctx.gh("run", "rerun", MISSING_ID, "--repo", ctx.repo, ok=None))
    expect_unsupported(ctx.gh("run", "rerun", MISSING_ID, "--failed", "--repo", ctx.repo, ok=None))
    expect_unsupported(ctx.gh("run", "rerun", "--job", MISSING_ID, "--repo", ctx.repo, ok=None))
    expect_unsupported(ctx.gh("run", "rerun", MISSING_ID, "--repo", _missing(ctx), ok=None))
    expect_unsupported(ctx.gh("run", "rerun", MISSING_ID, "--repo", _foreign(ctx, private=True), ok=None))
    # With --job gh ignores the run ID and --failed, and looks the job up.
    expect_unsupported(ctx.gh("run", "rerun", MISSING_ID, "--failed", "--job", MISSING_ID, "--repo", ctx.repo, ok=None))
    foreign, run_id = _foreign_finished(ctx)
    expect_error(ctx.gh("run", "rerun", run_id, "--repo", foreign, ok=None), status=403)
    expect_error(ctx.gh("run", "rerun", run_id, "--failed", "--repo", foreign, ok=None), status=403)
    jobs = ctx.sweep.api(_other(ctx), "GET", f"repos/{foreign}/actions/runs/{run_id}/jobs")["jobs"]
    assert jobs, jobs
    expect_error(ctx.gh("run", "rerun", run_id, "--job", str(jobs[0]["id"]), "--repo", foreign, ok=None), status=403)


@case(command="run view", variant="summary, verbose/job, attempt, JSON/jq/template, full/job logs", needs=("repo",), timeout=240)
def run_view(ctx):
    workflow, run_id, _ = _finished(ctx)
    rows = expect_json(ctx.gh("run", "view", run_id, "--repo", ctx.repo, "--attempt", "1", "--json",
                             "databaseId,status,conclusion,event,workflowName,jobs,attempt"),
                       {"databaseId": int, "status": "completed", "conclusion": "success", "event": "workflow_dispatch",
                        "workflowName": workflow["name"], "attempt": 1,
                        "jobs": [{"databaseId": int, "name": str, "conclusion": "success"}]})
    assert str(rows["databaseId"]) == run_id and rows["jobs"], rows
    job_id = str(rows["jobs"][0]["databaseId"])
    summary = expect_ok(ctx.gh("run", "view", run_id, "--repo", ctx.repo)).stdout
    assert workflow["name"] in summary and "proof" in summary, summary
    verbose = expect_ok(ctx.gh("run", "view", run_id, "--repo", ctx.repo, "--verbose", "--exit-status")).stdout
    assert "Sweep proof step" in verbose, verbose
    job = expect_ok(ctx.gh("run", "view", "--repo", ctx.repo, "--job", job_id, "--verbose")).stdout
    assert "Sweep proof step" in job, job
    assert expect_ok(ctx.gh("run", "view", run_id, "--repo", ctx.repo, "--json", "conclusion", "--jq", ".conclusion")).stdout.strip() == "success"
    assert expect_ok(ctx.gh("run", "view", run_id, "--repo", ctx.repo, "--json", "status", "--template", "{{.status}}\n")).stdout.strip() == "completed"
    assert "SWEEP_SUCCESS" in expect_ok(ctx.gh("run", "view", run_id, "--repo", ctx.repo, "--log")).stdout
    assert "SWEEP_SUCCESS" in expect_ok(ctx.gh("run", "view", run_id, "--repo", ctx.repo, "--log", "--job", job_id, "--attempt", "1")).stdout
    assert expect_ok(ctx.gh("run", "view", run_id, "--repo", ctx.repo, "--log-failed")).stdout.strip() == ""


@case(command="run view", variant="failure exit status and failed-step/job logs", needs=("repo",), timeout=240)
def run_view_failure(ctx):
    _, run_id, _ = _finished(ctx, outcome="failure")
    result = ctx.gh("run", "view", run_id, "--repo", ctx.repo, "--exit-status", ok=None)
    assert not result.timed_out and result.rc == 1 and "proof" in result.stdout, result
    assert "SWEEP_FAILURE" in expect_ok(ctx.gh("run", "view", run_id, "--repo", ctx.repo, "--log-failed")).stdout
    job_id = str(_jobs(ctx, run_id)[0]["id"])
    assert "SWEEP_FAILURE" in expect_ok(ctx.gh("run", "view", run_id, "--repo", ctx.repo, "--job", job_id, "--log-failed")).stdout


@case(command="run view", variant="404 run, attempt, job, repo/private", needs=("repo",), timeout=240)
def run_view_errors(ctx):
    _, run_id, _ = _finished(ctx)
    expect_unsupported(ctx.gh("run", "view", MISSING_ID, "--repo", ctx.repo, ok=None))
    expect_unsupported(ctx.gh("run", "view", run_id, "--attempt", "999", "--repo", ctx.repo, ok=None))
    expect_unsupported(ctx.gh("run", "view", "--job", MISSING_ID, "--repo", ctx.repo, ok=None))
    expect_unsupported(ctx.gh("run", "view", MISSING_ID, "--log", "--repo", _missing(ctx), ok=None))
    expect_unsupported(ctx.gh("run", "view", MISSING_ID, "--repo", _foreign(ctx, private=True), ok=None))


@case(command="run watch", variant="live success/compact/interval, completed success", needs=("repo",), timeout=240)
def run_watch(ctx):
    workflow = _workflow(ctx, outcome="short")
    run_id = _dispatch(ctx, workflow)
    result = expect_ok(ctx.gh("run", "watch", run_id, "--repo", ctx.repo, "--compact", "--exit-status", "--interval", "1", timeout=100))
    assert "completed" in (result.stdout + result.stderr).lower(), result
    assert _wait(ctx, run_id)["conclusion"] == "success"
    result = expect_ok(ctx.gh("run", "watch", run_id, "--repo", ctx.repo))
    assert "completed" in (result.stdout + result.stderr).lower(), result
    second_run = _dispatch(ctx, workflow)
    result = expect_ok(ctx.gh("run", "watch", second_run, "--repo", ctx.repo, "--interval", "1", timeout=100))
    assert "completed" in (result.stdout + result.stderr).lower(), result
    assert _wait(ctx, second_run)["conclusion"] == "success"


@case(command="run watch", variant="failure with/without exit status and 404", needs=("repo",), timeout=240)
def run_watch_failure(ctx):
    _, run_id, _ = _finished(ctx, outcome="failure")
    result = expect_ok(ctx.gh("run", "watch", run_id, "--repo", ctx.repo, "--interval", "1"))
    assert "failure" in (result.stdout + result.stderr).lower(), result
    result = ctx.gh("run", "watch", run_id, "--repo", ctx.repo, "--exit-status", "--compact", ok=None, timeout=30)
    assert not result.timed_out and result.rc == 1 and "failure" in (result.stdout + result.stderr).lower(), result
    expect_unsupported(ctx.gh("run", "watch", MISSING_ID, "--repo", ctx.repo, ok=None, timeout=15))
    expect_unsupported(ctx.gh("run", "watch", MISSING_ID, "--repo", _missing(ctx), ok=None, timeout=15))
    expect_unsupported(ctx.gh("run", "watch", MISSING_ID, "--repo", _foreign(ctx, private=True), ok=None, timeout=15))


@case(command="variable set", variant="create/update, stdin, dotenv file/stdin", needs=("repo",))
def variable_set_repo(ctx):
    name, value = _name(ctx), ctx.name("value")
    expect_ok(ctx.gh("variable", "set", name, "--repo", ctx.repo, "--body", value))
    assert ctx.api("GET", f"repos/{ctx.repo}/actions/variables/{name}")["value"] == value
    expect_ok(ctx.gh("variable", "set", name, "--repo", ctx.repo, input="replacement"))
    assert ctx.api("GET", f"repos/{ctx.repo}/actions/variables/{name}")["value"] == "replacement"
    expect_ok(ctx.gh("variable", "set", name, "--repo", ctx.repo, "--body", ""))
    assert ctx.api("GET", f"repos/{ctx.repo}/actions/variables/{name}")["value"] == ""
    first, second = _name(ctx), _name(ctx)
    dotenv = ctx.directory / ".env"
    dotenv.write_text(f'{first}="one two"\n{second}=three\n')
    expect_ok(ctx.gh("variable", "set", "--repo", ctx.repo, "--env-file", str(dotenv)))
    assert ctx.api("GET", f"repos/{ctx.repo}/actions/variables/{first}")["value"] == "one two"
    expect_ok(ctx.gh("variable", "set", "--repo", ctx.repo, "--env-file", "-", input=f"{second}=updated\n"))
    assert ctx.api("GET", f"repos/{ctx.repo}/actions/variables/{second}")["value"] == "updated"


@case(command="variable set", variant="environment and org visibility/selection", needs=("repo",))
def variable_set_scopes(ctx):
    name, env, org = _name(ctx), _env(ctx), ctx.org()
    expect_ok(ctx.gh("variable", "set", name, "--repo", ctx.repo, "--env", env, "--body", "environment"))
    assert _values(ctx, "variables", env=env)[0]["value"] == "environment"
    org_repo = ctx.sweep.create_repo(ctx.account, ctx.name("org-repo"), org=org)
    expect_ok(ctx.gh("variable", "set", name, "--org", org, "--body", "private"))
    assert _values(ctx, "variables", org=org)[0]["visibility"] == "private"
    for visibility in ("all", "private"):
        expect_ok(ctx.gh("variable", "set", name, "--org", org, "--visibility", visibility, "--body", visibility))
        assert _values(ctx, "variables", org=org)[0]["visibility"] == visibility
    expect_ok(ctx.gh("variable", "set", name, "--org", org, "--visibility", "selected", "--repos", org_repo["name"], "--body", "selected"))
    assert _values(ctx, "variables", org=org)[0]["visibility"] == "selected"
    selected = ctx.api("GET", f"orgs/{org}/actions/variables/{name}/repositories")["repositories"]
    assert [row["id"] for row in selected] == [org_repo["id"]], selected
    dotenv = ctx.directory / ".env"
    imported = _name(ctx)
    dotenv.write_text(f"{imported}=scoped\n")
    expect_ok(ctx.gh("variable", "set", "--repo", ctx.repo, "--env", env, "--env-file", str(dotenv)))
    expect_ok(ctx.gh("variable", "set", "--org", org, "--visibility", "all", "--env-file", str(dotenv)))
    assert any(row["name"] == imported for row in _values(ctx, "variables", env=env))
    assert any(row["name"] == imported for row in _values(ctx, "variables", org=org))


@case(command="variable set", variant="bad name, missing scope, denied", needs=("repo",))
def variable_set_errors(ctx):
    name = _name(ctx)
    expect_error(ctx.gh("variable", "set", "GITHUB_" + name, "--repo", ctx.repo, "--body", "bad", ok=None), status=422)
    expect_unsupported(ctx.gh("variable", "set", name, "--repo", _missing(ctx), "--body", "bad", ok=None))
    expect_error(ctx.gh("variable", "set", name, "--repo", _foreign(ctx), "--body", "bad", ok=None), status=403)
    foreign_repo, foreign_env = _foreign_env(ctx)
    expect_error(ctx.gh("variable", "set", name, "--repo", foreign_repo, "--env", foreign_env, "--body", "bad", ok=None), status=403)
    expect_unsupported(ctx.gh("variable", "set", name, "--repo", ctx.repo, "--env", ctx.name("missing"), "--body", "bad", ok=None))
    expect_unsupported(ctx.gh("variable", "set", name, "--org", ctx.name("missing"), "--body", "bad", ok=None))
    foreign_org = ctx.sweep.create_org(_other(ctx), ctx.name("foreign-org"))
    expect_error(ctx.gh("variable", "set", name, "--org", foreign_org, "--body", "bad", ok=None), status=403)
    _client_error(ctx.gh("variable", "set", name, "--org", foreign_org, "--visibility", "invalid", "--body", "bad", ok=None), "invalid")


@case(command="variable get", variant="repo plain/JSON/jq/template, environment, org", needs=("repo",))
def variable_get(ctx):
    value, name = ctx.name("plain-value"), _name(ctx)
    _variable(ctx, name, value)
    assert expect_ok(ctx.gh("variable", "get", name, "--repo", ctx.repo)).stdout.strip() == value
    expect_json(ctx.gh("variable", "get", name, "--repo", ctx.repo, "--json", "name,value,createdAt,updatedAt"),
                {"name": name, "value": value, "createdAt": str, "updatedAt": str})
    assert expect_ok(ctx.gh("variable", "get", name, "--repo", ctx.repo, "--json", "value", "--jq", ".value")).stdout.strip() == value
    assert expect_ok(ctx.gh("variable", "get", name, "--repo", ctx.repo, "--json", "name,value", "--template", "{{.name}}={{.value}}\n")).stdout.strip() == name + "=" + value
    env, org = _env(ctx), ctx.org()
    _variable(ctx, name, "environment", env=env)
    _variable(ctx, name, "organization", org=org)
    expect_json(ctx.gh("variable", "get", name, "--repo", ctx.repo, "--env", env, "--json", "name,value"), {"name": name, "value": "environment"})
    expect_json(ctx.gh("variable", "get", name, "--org", org, "--json", "name,value,visibility"), {"name": name, "value": "organization", "visibility": "private"})


@case(command="variable get", variant="missing name/repo/env/org and permission", needs=("repo",))
def variable_get_errors(ctx):
    name = _name(ctx)
    # gh reports any 404 as "variable X was not found".
    _client_error(ctx.gh("variable", "get", name, "--repo", ctx.repo, ok=None), "was not found")
    _client_error(ctx.gh("variable", "get", name, "--repo", _missing(ctx), ok=None), "was not found")
    _client_error(ctx.gh("variable", "get", name, "--repo", ctx.repo, "--env", ctx.name("missing"), ok=None), "was not found")
    _client_error(ctx.gh("variable", "get", name, "--org", ctx.name("missing"), ok=None), "was not found")
    expect_error(ctx.gh("variable", "get", name, "--repo", _foreign(ctx), ok=None), status=403)
    foreign_repo, foreign_env = _foreign_env(ctx)
    expect_error(ctx.gh("variable", "get", name, "--repo", foreign_repo, "--env", foreign_env, ok=None), status=403)
    foreign_org = ctx.sweep.create_org(_other(ctx), ctx.name("foreign-org"))
    expect_error(ctx.gh("variable", "get", name, "--org", foreign_org, ok=None), status=403)


@case(command="variable list", variant="empty and populated repo/env/org, formatting", needs=("repo",))
def variable_list(ctx):
    expect_json(ctx.gh("variable", "list", "--repo", ctx.repo, "--json", "name,value"), [])
    name = _variable(ctx)
    rows = expect_json(ctx.gh("variable", "list", "--repo", ctx.repo, "--json", "name,value,createdAt,updatedAt"),
                       [{"name": name, "value": "sweep value", "createdAt": str, "updatedAt": str}])
    assert len(rows) == 1, rows
    assert name in expect_ok(ctx.gh("variable", "list", "--repo", ctx.repo)).stdout
    assert expect_ok(ctx.gh("variable", "list", "--repo", ctx.repo, "--json", "name", "--jq", ".[0].name")).stdout.strip() == name
    assert expect_ok(ctx.gh("variable", "list", "--repo", ctx.repo, "--json", "value", "--template", "{{(index . 0).value}}\n")).stdout.strip() == "sweep value"
    env, org = _env(ctx), ctx.org()
    expect_json(ctx.gh("variable", "list", "--repo", ctx.repo, "--env", env, "--json", "name"), [])
    expect_json(ctx.gh("variable", "list", "--org", org, "--json", "name"), [])
    _variable(ctx, name, "environment", env=env)
    _variable(ctx, name, "organization", org=org)
    expect_json(ctx.gh("variable", "list", "--repo", ctx.repo, "--env", env, "--json", "name,value"), [{"name": name, "value": "environment"}])
    rows = expect_json(ctx.gh("variable", "list", "--org", org, "--json", "name,value,visibility"), [{"name": name, "value": "organization", "visibility": "private"}])
    assert len(rows) == 1, rows


@case(command="variable list", variant="missing and denied scopes", needs=("repo",))
def variable_list_errors(ctx):
    expect_unsupported(ctx.gh("variable", "list", "--repo", _missing(ctx), ok=None))
    expect_unsupported(ctx.gh("variable", "list", "--repo", ctx.repo, "--env", ctx.name("missing"), ok=None))
    expect_unsupported(ctx.gh("variable", "list", "--org", ctx.name("missing"), ok=None))
    expect_error(ctx.gh("variable", "list", "--repo", _foreign(ctx), ok=None), status=403)
    foreign_repo, foreign_env = _foreign_env(ctx)
    expect_error(ctx.gh("variable", "list", "--repo", foreign_repo, "--env", foreign_env, ok=None), status=403)
    foreign_org = ctx.sweep.create_org(_other(ctx), ctx.name("foreign-org"))
    expect_error(ctx.gh("variable", "list", "--org", foreign_org, ok=None), status=403)


@case(command="variable delete", variant="repo/env/org delete, repeat, missing/denied", needs=("repo",))
def variable_delete(ctx):
    name, env, org = _name(ctx), _env(ctx), ctx.org()
    _variable(ctx, name)
    _variable(ctx, name, env=env)
    _variable(ctx, name, org=org)
    expect_ok(ctx.gh("variable", "delete", name, "--repo", ctx.repo))
    assert _values(ctx, "variables") == []
    assert len(_values(ctx, "variables", env=env)) == 1
    expect_ok(ctx.gh("variable", "delete", name, "--repo", ctx.repo, "--env", env))
    assert _values(ctx, "variables", env=env) == []
    expect_ok(ctx.gh("variable", "delete", name, "--org", org))
    assert _values(ctx, "variables", org=org) == []
    expect_unsupported(ctx.gh("variable", "delete", name, "--repo", ctx.repo, ok=None))
    expect_unsupported(ctx.gh("variable", "delete", name, "--repo", ctx.repo, "--env", env, ok=None))
    expect_unsupported(ctx.gh("variable", "delete", name, "--org", org, ok=None))
    expect_unsupported(ctx.gh("variable", "delete", name, "--repo", _missing(ctx), ok=None))
    expect_error(ctx.gh("variable", "delete", name, "--repo", _foreign(ctx), ok=None), status=403)
    expect_unsupported(ctx.gh("variable", "delete", name, "--repo", ctx.repo, "--env", ctx.name("missing"), ok=None))
    expect_unsupported(ctx.gh("variable", "delete", name, "--org", ctx.name("missing"), ok=None))
    foreign_repo, foreign_env = _foreign_env(ctx)
    expect_error(ctx.gh("variable", "delete", name, "--repo", foreign_repo, "--env", foreign_env, ok=None), status=403)
    foreign_org = ctx.sweep.create_org(_other(ctx), ctx.name("foreign-org"))
    expect_error(ctx.gh("variable", "delete", name, "--org", foreign_org, ok=None), status=403)


@case(command="secret set", variant="repo body/stdin, update, dotenv file/stdin, no-store", needs=("repo",))
def secret_set_repo(ctx):
    name, plaintext = _name(ctx), ctx.name("plaintext")
    expect_ok(ctx.gh("secret", "set", name, "--repo", ctx.repo, "--body", plaintext))
    _secret_present(ctx, name)
    expect_ok(ctx.gh("secret", "set", name, "--repo", ctx.repo, input="updated-value"))
    assert len(_values(ctx, "secrets")) == 1
    no_store = _name(ctx)
    result = expect_ok(ctx.gh("secret", "set", no_store, "--repo", ctx.repo, "--body", plaintext, "--no-store"))
    encrypted = base64.b64decode(result.stdout.strip(), validate=True)
    assert len(encrypted) == len(plaintext.encode()) + 48 and plaintext not in result.stdout, result
    assert all(row["name"] != no_store for row in _values(ctx, "secrets"))
    first, second = _name(ctx), _name(ctx)
    dotenv = ctx.directory / ".env"
    dotenv.write_text(f'{first}="one two"\n{second}=three\n')
    expect_ok(ctx.gh("secret", "set", "--repo", ctx.repo, "--env-file", str(dotenv)))
    _secret_present(ctx, first)
    _secret_present(ctx, second)
    expect_ok(ctx.gh("secret", "set", "--repo", ctx.repo, "--env-file", "-", input=f"{first}=updated\n"))
    assert len(_values(ctx, "secrets")) == 3
    assert plaintext not in json.dumps(_values(ctx, "secrets"))


@case(command="secret set", variant="actions/dependabot app and environment", needs=("repo",))
def secret_set_apps_env(ctx):
    name, env = _name(ctx), _env(ctx)
    expect_ok(ctx.gh("secret", "set", name, "--app", "actions", "--repo", ctx.repo, "--body", "actions-value"))
    expect_ok(ctx.gh("secret", "set", name, "--app", "dependabot", "--repo", ctx.repo, "--body", "dependabot-value"))
    rows = ctx.api("GET", f"repos/{ctx.repo}/dependabot/secrets")["secrets"]
    assert [row["name"] for row in rows] == [name], rows
    expect_ok(ctx.gh("secret", "set", name, "--repo", ctx.repo, "--env", env, "--body", "environment-value"))
    _secret_present(ctx, name, env=env)
    imported = _name(ctx)
    expect_ok(ctx.gh("secret", "set", "--repo", ctx.repo, "--env", env, "--env-file", "-", input=f"{imported}=value\n"))
    _secret_present(ctx, imported, env=env)


@case(command="secret set", variant="organization visibility and repository selections", needs=("repo",))
def secret_set_org(ctx):
    org, name = ctx.org(), _name(ctx)
    repo = ctx.sweep.create_repo(ctx.account, ctx.name("org-repo"), org=org)
    expect_ok(ctx.gh("secret", "set", name, "--org", org, "--body", "private"))
    rows = _values(ctx, "secrets", org=org)
    assert rows[0]["visibility"] == "private", rows
    for visibility in ("all", "private"):
        expect_ok(ctx.gh("secret", "set", name, "--org", org, "--visibility", visibility, "--body", "value"))
        rows = _values(ctx, "secrets", org=org)
        assert rows[0]["visibility"] == visibility, rows
    expect_ok(ctx.gh("secret", "set", name, "--org", org, "--visibility", "selected", "--repos", repo["name"], "--body", "selected"))
    selected = ctx.api("GET", f"orgs/{org}/actions/secrets/{name}/repositories")["repositories"]
    assert [row["id"] for row in selected] == [repo["id"]], selected
    # gh sends visibility=selected without a repository list, so check it on a new secret.
    empty = _name(ctx)
    expect_ok(ctx.gh("secret", "set", empty, "--org", org, "--no-repos-selected", "--body", "none"))
    selected = ctx.api("GET", f"orgs/{org}/actions/secrets/{empty}/repositories")["repositories"]
    assert selected == [], selected
    row = next(row for row in _values(ctx, "secrets", org=org) if row["name"] == empty)
    assert row["visibility"] == "selected", row
    imported = _name(ctx)
    expect_ok(ctx.gh("secret", "set", "--org", org, "--visibility", "all", "--env-file", "-", input=f"{imported}=value\n"))
    _secret_present(ctx, imported, org=org)


@case(command="secret set", variant="cloud-only user/codespaces/agents and invalid/denied scopes", needs=("repo",))
def secret_set_errors(ctx):
    name = _name(ctx)
    expect_unsupported(ctx.gh("secret", "set", name, "--user", "--body", "value", ok=None))
    expect_unsupported(ctx.gh("secret", "set", name, "--user", "--repos", ctx.repo, "--body", "value", ok=None))
    expect_unsupported(ctx.gh("secret", "set", name, "--org", ctx.org(), "--app", "codespaces", "--body", "value", ok=None))
    expect_unsupported(ctx.gh("secret", "set", name, "--app", "codespaces", "--repo", ctx.repo, "--body", "value", ok=None))
    expect_unsupported(ctx.gh("secret", "set", name, "--app", "agents", "--repo", ctx.repo, "--body", "value", ok=None))
    expect_unsupported(ctx.gh("secret", "set", name, "--repo", _missing(ctx), "--body", "value", ok=None))
    expect_unsupported(ctx.gh("secret", "set", name, "--repo", ctx.repo, "--env", ctx.name("missing"), "--body", "value", ok=None))
    expect_unsupported(ctx.gh("secret", "set", name, "--org", ctx.name("missing"), "--body", "value", ok=None))
    expect_error(ctx.gh("secret", "set", "GITHUB_" + name, "--repo", ctx.repo, "--body", "value", ok=None), status=422)
    expect_error(ctx.gh("secret", "set", name, "--repo", _foreign(ctx), "--body", "value", ok=None), status=403)
    foreign_repo, foreign_env = _foreign_env(ctx)
    expect_error(ctx.gh("secret", "set", name, "--repo", foreign_repo, "--env", foreign_env, "--body", "value", ok=None), status=403)
    foreign_org = ctx.sweep.create_org(_other(ctx), ctx.name("foreign-org"))
    expect_error(ctx.gh("secret", "set", name, "--org", foreign_org, "--body", "value", ok=None), status=403)
    _client_error(ctx.gh("secret", "set", name, "--repo", ctx.repo, "--app", "invalid", "--body", "value", ok=None), "invalid")
    _client_error(ctx.gh("secret", "set", name, "--org", foreign_org, "--visibility", "invalid", "--body", "value", ok=None), "invalid")


@case(command="secret list", variant="empty/populated, repo/env/org/app and formatting", needs=("repo",))
def secret_list(ctx):
    expect_json(ctx.gh("secret", "list", "--repo", ctx.repo, "--json", "name,updatedAt"), [])
    name, env, org = _name(ctx), _env(ctx), ctx.org()
    expect_ok(ctx.gh("secret", "set", name, "--repo", ctx.repo, "--body", "secret-plaintext"))
    expect_ok(ctx.gh("secret", "set", name, "--repo", ctx.repo, "--env", env, "--body", "env-plaintext"))
    expect_ok(ctx.gh("secret", "set", name, "--org", org, "--visibility", "all", "--body", "org-plaintext"))
    rows = expect_json(ctx.gh("secret", "list", "--repo", ctx.repo, "--app", "actions", "--json", "name,updatedAt"),
                       [{"name": name, "updatedAt": str}])
    assert len(rows) == 1 and "secret-plaintext" not in json.dumps(rows), rows
    assert name in expect_ok(ctx.gh("secret", "list", "--repo", ctx.repo)).stdout
    assert expect_ok(ctx.gh("secret", "list", "--repo", ctx.repo, "--json", "name", "--jq", ".[0].name")).stdout.strip() == name
    assert expect_ok(ctx.gh("secret", "list", "--repo", ctx.repo, "--json", "name", "--template", "{{(index . 0).name}}\n")).stdout.strip() == name
    rows = expect_json(ctx.gh("secret", "list", "--repo", ctx.repo, "--env", env, "--json", "name,updatedAt"), [{"name": name, "updatedAt": str}])
    assert len(rows) == 1, rows
    rows = expect_json(ctx.gh("secret", "list", "--org", org, "--json", "name,visibility,updatedAt"), [{"name": name, "visibility": "all", "updatedAt": str}])
    assert len(rows) == 1, rows
    expect_json(ctx.gh("secret", "list", "--repo", ctx.repo, "--app", "dependabot", "--json", "name"), [])
    expect_ok(ctx.gh("secret", "set", name, "--repo", ctx.repo, "--app", "dependabot", "--body", "dependabot"))
    rows = expect_json(ctx.gh("secret", "list", "--repo", ctx.repo, "--app", "dependabot", "--json", "name"), [{"name": name}])
    assert len(rows) == 1, rows


@case(command="secret list", variant="cloud-only scopes, missing and denied", needs=("repo",))
def secret_list_errors(ctx):
    expect_unsupported(ctx.gh("secret", "list", "--user", ok=None))
    expect_unsupported(ctx.gh("secret", "list", "--org", ctx.org(), "--app", "codespaces", ok=None))
    expect_unsupported(ctx.gh("secret", "list", "--repo", ctx.repo, "--app", "codespaces", ok=None))
    expect_unsupported(ctx.gh("secret", "list", "--repo", ctx.repo, "--app", "agents", ok=None))
    expect_unsupported(ctx.gh("secret", "list", "--repo", _missing(ctx), ok=None))
    expect_unsupported(ctx.gh("secret", "list", "--repo", ctx.repo, "--env", ctx.name("missing"), ok=None))
    expect_unsupported(ctx.gh("secret", "list", "--org", ctx.name("missing"), ok=None))
    expect_error(ctx.gh("secret", "list", "--repo", _foreign(ctx), ok=None), status=403)
    foreign_repo, foreign_env = _foreign_env(ctx)
    expect_error(ctx.gh("secret", "list", "--repo", foreign_repo, "--env", foreign_env, ok=None), status=403)
    foreign_org = ctx.sweep.create_org(_other(ctx), ctx.name("foreign-org"))
    expect_error(ctx.gh("secret", "list", "--org", foreign_org, ok=None), status=403)


@case(command="secret delete", variant="repo/env/org/dependabot delete, repeated deletion", needs=("repo",))
def secret_delete(ctx):
    name, env, org = _name(ctx), _env(ctx), ctx.org()
    expect_ok(ctx.gh("secret", "set", name, "--repo", ctx.repo, "--body", "repo"))
    expect_ok(ctx.gh("secret", "set", name, "--repo", ctx.repo, "--env", env, "--body", "env"))
    expect_ok(ctx.gh("secret", "set", name, "--org", org, "--body", "org"))
    expect_ok(ctx.gh("secret", "set", name, "--repo", ctx.repo, "--app", "dependabot", "--body", "dependabot"))
    expect_ok(ctx.gh("secret", "delete", name, "--repo", ctx.repo, "--app", "actions"))
    assert _values(ctx, "secrets") == []
    _secret_present(ctx, name, env=env)
    expect_ok(ctx.gh("secret", "delete", name, "--repo", ctx.repo, "--env", env))
    assert _values(ctx, "secrets", env=env) == []
    expect_ok(ctx.gh("secret", "delete", name, "--org", org))
    assert _values(ctx, "secrets", org=org) == []
    expect_ok(ctx.gh("secret", "delete", name, "--repo", ctx.repo, "--app", "dependabot"))
    assert ctx.api("GET", f"repos/{ctx.repo}/dependabot/secrets")["secrets"] == []
    expect_unsupported(ctx.gh("secret", "delete", name, "--repo", ctx.repo, ok=None))
    expect_unsupported(ctx.gh("secret", "delete", name, "--repo", ctx.repo, "--env", env, ok=None))
    expect_unsupported(ctx.gh("secret", "delete", name, "--org", org, ok=None))


@case(command="secret delete", variant="cloud-only scopes and missing/denied", needs=("repo",))
def secret_delete_errors(ctx):
    name = _name(ctx)
    expect_unsupported(ctx.gh("secret", "delete", name, "--user", ok=None))
    expect_unsupported(ctx.gh("secret", "delete", name, "--org", ctx.org(), "--app", "codespaces", ok=None))
    expect_unsupported(ctx.gh("secret", "delete", name, "--repo", ctx.repo, "--app", "codespaces", ok=None))
    expect_unsupported(ctx.gh("secret", "delete", name, "--repo", ctx.repo, "--app", "agents", ok=None))
    expect_unsupported(ctx.gh("secret", "delete", name, "--repo", _missing(ctx), ok=None))
    expect_unsupported(ctx.gh("secret", "delete", name, "--repo", ctx.repo, "--env", ctx.name("missing"), ok=None))
    expect_unsupported(ctx.gh("secret", "delete", name, "--org", ctx.name("missing"), ok=None))
    expect_error(ctx.gh("secret", "delete", name, "--repo", _foreign(ctx), ok=None), status=403)
    foreign_repo, foreign_env = _foreign_env(ctx)
    expect_error(ctx.gh("secret", "delete", name, "--repo", foreign_repo, "--env", foreign_env, ok=None), status=403)
    foreign_org = ctx.sweep.create_org(_other(ctx), ctx.name("foreign-org"))
    expect_error(ctx.gh("secret", "delete", name, "--org", foreign_org, ok=None), status=403)


@case(command="run delete", variant="active run conflict", needs=("repo",), timeout=180)
def run_delete_active(ctx):
    workflow = _workflow(ctx, outcome="slow")
    run_id = _dispatch(ctx, workflow)
    try:
        expect_error(ctx.gh("run", "delete", run_id, "--repo", ctx.repo, ok=None), status=409)
        assert ctx.api("GET", f"repos/{ctx.repo}/actions/runs/{run_id}")["status"] in ("queued", "in_progress")
    finally:
        ctx.api("POST", f"repos/{ctx.repo}/actions/runs/{run_id}/cancel")
    assert _wait(ctx, run_id)["conclusion"] == "cancelled"


@case(command="run rerun", variant="active run conflict", needs=("repo",), timeout=180)
def run_rerun_active(ctx):
    workflow = _workflow(ctx, outcome="slow")
    run_id = _dispatch(ctx, workflow)
    try:
        expect_error(ctx.gh("run", "rerun", run_id, "--repo", ctx.repo, ok=None), status=409)
        expect_error(ctx.gh("run", "rerun", run_id, "--failed", "--repo", ctx.repo, ok=None), status=409)
    finally:
        ctx.api("POST", f"repos/{ctx.repo}/actions/runs/{run_id}/cancel")
    assert _wait(ctx, run_id)["conclusion"] == "cancelled"


@case(command="run view", variant="historical attempt after rerun", needs=("repo",), timeout=240)
def run_view_attempts(ctx):
    _, run_id, _ = _finished(ctx)
    ctx.api("POST", f"repos/{ctx.repo}/actions/runs/{run_id}/rerun", {})
    assert _wait(ctx, run_id)["run_attempt"] == 2
    expect_json(ctx.gh("run", "view", run_id, "--repo", ctx.repo, "--attempt", "1", "--json", "attempt,status,conclusion"),
                {"attempt": 1, "status": "completed", "conclusion": "success"})
    expect_json(ctx.gh("run", "view", run_id, "--repo", ctx.repo, "--attempt", "2", "--json", "attempt,status,conclusion"),
                {"attempt": 2, "status": "completed", "conclusion": "success"})
    assert "SWEEP_SUCCESS" in expect_ok(ctx.gh("run", "view", run_id, "--repo", ctx.repo, "--attempt", "1", "--log")).stdout


@case(command="secret set", variant="organization Dependabot secrets are supported on GHES")
def secret_set_org_dependabot(ctx):
    org, name = ctx.org(), _name(ctx)
    expect_ok(ctx.gh("secret", "set", name, "--org", org, "--app", "dependabot", "--visibility", "all", "--body", "value"))
    rows = ctx.api("GET", f"orgs/{org}/dependabot/secrets")["secrets"]
    assert len(rows) == 1 and rows[0]["name"] == name and rows[0]["visibility"] == "all", rows
    expect_ok(ctx.gh("secret", "set", name, "--org", org, "--app", "dependabot", "--no-repos-selected", "--body", "updated"))
    assert ctx.api("GET", f"orgs/{org}/dependabot/secrets/{name}/repositories")["repositories"] == []


@case(command="secret list", variant="organization selected repositories and Dependabot", needs=("repo",))
def secret_list_org_selection(ctx):
    org, name = ctx.org(), _name(ctx)
    repo = ctx.sweep.create_repo(ctx.account, ctx.name("selected"), org=org)
    expect_ok(ctx.gh("secret", "set", name, "--org", org, "--repos", repo["name"], "--body", "value"))
    rows = expect_json(ctx.gh("secret", "list", "--org", org, "--json", "name,visibility,numSelectedRepos,selectedReposURL"),
                       [{"name": name, "visibility": "selected", "numSelectedRepos": 1, "selectedReposURL": str}])
    assert len(rows) == 1 and rows[0]["selectedReposURL"].endswith(f"/secrets/{name}/repositories"), rows
    expect_ok(ctx.gh("secret", "set", name, "--org", org, "--app", "dependabot", "--visibility", "all", "--body", "value"))
    rows = expect_json(ctx.gh("secret", "list", "--org", org, "--app", "dependabot", "--json", "name,visibility,updatedAt"),
                       [{"name": name, "visibility": "all", "updatedAt": str}])
    assert len(rows) == 1, rows


@case(command="secret delete", variant="organization Dependabot delete and repeated 404")
def secret_delete_org_dependabot(ctx):
    org, name = ctx.org(), _name(ctx)
    expect_ok(ctx.gh("secret", "set", name, "--org", org, "--app", "dependabot", "--visibility", "all", "--body", "value"))
    expect_ok(ctx.gh("secret", "delete", name, "--org", org, "--app", "dependabot"))
    assert ctx.api("GET", f"orgs/{org}/dependabot/secrets")["secrets"] == []
    expect_unsupported(ctx.gh("secret", "delete", name, "--org", org, "--app", "dependabot", ok=None))


@case(command="variable get", variant="organization selected repository metadata")
def variable_get_selection(ctx):
    org, name = ctx.org(), _name(ctx)
    repo = ctx.sweep.create_repo(ctx.account, ctx.name("selected"), org=org)
    expect_ok(ctx.gh("variable", "set", name, "--org", org, "--repos", repo["name"], "--body", "selected-value"))
    row = expect_json(ctx.gh("variable", "get", name, "--org", org, "--json", "name,value,visibility,numSelectedRepos,selectedReposURL"),
                      {"name": name, "value": "selected-value", "visibility": "selected", "numSelectedRepos": 1, "selectedReposURL": str})
    assert row["selectedReposURL"].endswith(f"/variables/{name}/repositories"), row


@case(command="variable list", variant="organization selected repository metadata")
def variable_list_selection(ctx):
    org, name = ctx.org(), _name(ctx)
    repo = ctx.sweep.create_repo(ctx.account, ctx.name("selected"), org=org)
    expect_ok(ctx.gh("variable", "set", name, "--org", org, "--repos", repo["name"], "--body", "selected-value"))
    rows = expect_json(ctx.gh("variable", "list", "--org", org, "--json", "name,value,visibility,numSelectedRepos,selectedReposURL"),
                       [{"name": name, "value": "selected-value", "visibility": "selected", "numSelectedRepos": 1, "selectedReposURL": str}])
    assert len(rows) == 1 and rows[0]["selectedReposURL"].endswith(f"/variables/{name}/repositories"), rows


@case(command="variable set", variant="implicit selected visibility from repos")
def variable_set_implicit_selection(ctx):
    org, name = ctx.org(), _name(ctx)
    repo = ctx.sweep.create_repo(ctx.account, ctx.name("selected"), org=org)
    expect_ok(ctx.gh("variable", "set", name, "--org", org, "--repos", repo["name"], "--body", "value"))
    rows = ctx.api("GET", f"orgs/{org}/actions/variables")["variables"]
    assert len(rows) == 1 and rows[0]["name"] == name and rows[0]["visibility"] == "selected", rows
    assert [row["id"] for row in ctx.api("GET", f"orgs/{org}/actions/variables/{name}/repositories")["repositories"]] == [repo["id"]]
