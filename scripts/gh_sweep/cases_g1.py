"""Real-gh repository, organization, catalog and ruleset contracts.

Fixtures use REST/native forms; only the literal gh calls below count as coverage.
Assertions are intentionally not relaxed for missing Codeyard implementations of
ordinary GHES features (internal visibility, settings, filters, etc.). Such gaps
must fail the live sweep, not be mislabeled as cloud-only features.
Browser launches are explicitly waived in waivers_g1.json.
"""
import base64
import re
import secrets
import struct
from urllib.parse import urlsplit

from gh_sweep.harness import case, expect_error, expect_json, expect_ok


def clean_failure(result, contains):
    """Client validation and GraphQL errors do not necessarily have HTTP status."""
    assert not result.timed_out and result.rc == 1, result
    assert contains.lower() in result.stderr.lower(), result.stderr
    assert not re.search(r"panic:|panic at|segmentation fault|fatal error:", result.stderr, re.I), result.stderr
    return result


def missing(result):
    if "HTTP 404" in result.stderr:
        return expect_error(result, status=404, contains="Not Found")
    # GraphQL missing nodes come back with HTTP 200 and a NOT_FOUND error, which GitHub words
    # "Could not resolve to a Repository with the name 'owner/name'."
    return clean_failure(result, "Could not resolve to a")


def admin_missing(result):
    # gh replaces an autolink 404 with its own hint, as GitHub hides autolinks from non-admins.
    return expect_error(result, status=404, contains="Perhaps you are missing admin rights")


def refused(result):
    # Admin-only endpoints refuse a non-admin with 403, or hide the repository with 404.
    match = re.search(r"\bHTTP (403|404)\b", result.stderr)
    assert match, result
    return expect_error(result, status=match.group(1))


def no_results(result):
    # gh reports an empty list as "no results", which prints nothing and exits 0 off a terminal.
    expect_ok(result)
    assert result.stdout == "", result.stdout
    return result


def permission_denied(result):
    # gh can reject a mutation from viewerPermission before issuing HTTP, or
    # print the server's GraphQL denial. Both must be explicit permission errors.
    assert not result.timed_out and result.rc == 1, result
    assert re.search(r"permission|denied|forbidden|administrat|not allowed", result.stderr, re.I), result.stderr
    assert not re.search(r"panic:|panic at|segmentation fault|fatal error:", result.stderr, re.I), result.stderr
    return result


def other_account(ctx):
    return next(account for account in ctx.sweep.accounts if account != ctx.account)


def absent_repo(ctx):
    return ctx.account.name + "/" + ctx.name("absent")


def git(ctx, *args, cwd=None, ok=(0,)):
    return ctx.sweep.execute(ctx.account, ["git", *args], cwd=cwd or ctx.directory, ok=ok)


def local_repo(ctx):
    path = ctx.directory / ctx.name("local")
    path.mkdir()
    git(ctx, "init", "-b", "main", cwd=path)
    git(ctx, "config", "user.name", "Sweep", cwd=path)
    git(ctx, "config", "user.email", "sweep@example.test", cwd=path)
    (path / "README.md").write_text("sweep local commit\n")
    git(ctx, "add", "README.md", cwd=path)
    git(ctx, "commit", "-m", "sweep initial", cwd=path)
    return path


def file_fixture(ctx, content=b"sweep file\n", path=None):
    path = path or ctx.name("file") + ".txt"
    ctx.api("PUT", f"repos/{ctx.repo}/contents/{path}",
            {"message": "sweep file", "content": base64.b64encode(content).decode()})
    return path


def autolink_prefix():
    # Short and unique: a key prefix is limited in length, unlike ctx.name().
    return "SW" + secrets.token_hex(4).upper() + "-"


def autolink_fixture(ctx):
    return ctx.api("POST", f"repos/{ctx.repo}/autolinks",
                   {"key_prefix": autolink_prefix(), "url_template": "https://example.test/<num>",
                    "is_alphanumeric": True})


def public_key(ctx):
    # SSH wire-format ed25519 public key; no private key is needed for API tests.
    def ssh_string(value):
        return struct.pack(">I", len(value)) + value
    wire = ssh_string(b"ssh-ed25519") + ssh_string(secrets.token_bytes(32))
    text = "ssh-ed25519 " + base64.b64encode(wire).decode() + " " + ctx.name("key")
    path = ctx.directory / ctx.name("public-key")
    path.write_text(text + "\n")
    return path, text


def key_fixture(ctx):
    _, key = public_key(ctx)
    return ctx.api("POST", f"repos/{ctx.repo}/keys",
                   {"title": ctx.name("deploy"), "key": key, "read_only": True})


def ruleset_fixture(ctx, *, org=None, name=None, enforcement="evaluate"):
    conditions = {"ref_name": {"include": ["~DEFAULT_BRANCH"], "exclude": []}}
    if org:
        conditions["repository_name"] = {"include": ["~ALL"], "exclude": [], "protected": False}
    return ctx.api("POST", f"orgs/{org}/rulesets" if org else f"repos/{ctx.repo}/rulesets",
                   {"name": name or ctx.name("rule"), "target": "branch", "enforcement": enforcement,
                    "conditions": conditions, "rules": [{"type": "deletion"}]})


def fork_fixture(ctx):
    source_account = other_account(ctx)
    source = ctx.sweep.create_repo(source_account, ctx.name("upstream"))["full_name"]
    fork = ctx.api("POST", f"repos/{source}/forks", {"name": ctx.name("fork")})
    return source, fork["full_name"], source_account


@case(command="licenses", variant="bundled dependency notices")
def licenses(ctx):
    result = expect_ok(ctx.gh("licenses"))
    if result.stdout == "License information is only available in official release builds.\n":
        return  # the builder's gh comes from nixpkgs, which does not embed the notices
    assert "Copyright" in result.stdout and "license" in result.stdout.lower(), result.stdout


@case(command="org list", variant="membership and limit")
def org_list(ctx):
    org = ctx.org()
    result = expect_ok(ctx.gh("org", "list", "--limit", "1000"))
    assert org in result.stdout.split(), result.stdout
    limited = expect_ok(ctx.gh("org", "list", "--limit", "1"))
    assert len(limited.stdout.strip().splitlines()) == 1, limited.stdout
    assert expect_ok(ctx.gh("org", "list")).stdout.strip()


@case(command="org list", variant="invalid limit")
def org_list_invalid(ctx):
    clean_failure(ctx.gh("org", "list", "--limit", "0", ok=None), "limit")


@case(command="repo archive", variant="archive and idempotence", needs=("repo",))
def repo_archive(ctx):
    expect_ok(ctx.gh("repo", "archive", ctx.repo, "--yes"))
    assert ctx.api("GET", f"repos/{ctx.repo}")["archived"] is True
    expect_ok(ctx.gh("repo", "archive", ctx.repo, "--yes"))
    assert ctx.api("GET", f"repos/{ctx.repo}")["archived"] is True


@case(command="repo archive", variant="missing and non-owner", needs=("repo",))
def repo_archive_errors(ctx):
    missing(ctx.gh("repo", "archive", absent_repo(ctx), "--yes", ok=None))
    denied = ctx.sweep.gh(other_account(ctx), "repo", "archive", ctx.repo, "--yes", ok=None)
    permission_denied(denied)
    assert ctx.api("GET", f"repos/{ctx.repo}")["archived"] is False


@case(command="repo unarchive", variant="unarchive and idempotence", needs=("repo",))
def repo_unarchive(ctx):
    ctx.api("PATCH", f"repos/{ctx.repo}", {"archived": True})
    expect_ok(ctx.gh("repo", "unarchive", ctx.repo, "--yes"))
    assert ctx.api("GET", f"repos/{ctx.repo}")["archived"] is False
    expect_ok(ctx.gh("repo", "unarchive", ctx.repo, "--yes"))
    assert ctx.api("GET", f"repos/{ctx.repo}")["archived"] is False


@case(command="repo unarchive", variant="missing and non-owner", needs=("repo",))
def repo_unarchive_errors(ctx):
    missing(ctx.gh("repo", "unarchive", absent_repo(ctx), "--yes", ok=None))
    ctx.api("PATCH", f"repos/{ctx.repo}", {"archived": True})
    permission_denied(ctx.sweep.gh(other_account(ctx), "repo", "unarchive", ctx.repo, "--yes", ok=None))
    assert ctx.api("GET", f"repos/{ctx.repo}")["archived"] is True


@case(command="repo autolink create", variant="alphanumeric and numeric", needs=("repo",))
def autolink_create(ctx):
    alpha, numeric = autolink_prefix(), autolink_prefix()
    expect_ok(ctx.gh("repo", "autolink", "create", alpha, "https://example.test/<num>", "--repo", ctx.repo))
    expect_ok(ctx.gh("repo", "autolink", "create", numeric, "https://example.test/n/<num>", "--numeric", "--repo", ctx.repo))
    rows = ctx.api("GET", f"repos/{ctx.repo}/autolinks")
    assert {(row["key_prefix"], row["is_alphanumeric"]) for row in rows} == {(alpha, True), (numeric, False)}, rows


@case(command="repo autolink create", variant="validation duplicate missing denied", needs=("repo",))
def autolink_create_errors(ctx):
    row = autolink_fixture(ctx)
    expect_error(ctx.gh("repo", "autolink", "create", row["key_prefix"], row["url_template"], "--repo", ctx.repo, ok=None), status=422)
    expect_error(ctx.gh("repo", "autolink", "create", "BAD-", "https://example.test/no-variable", "--repo", ctx.repo, ok=None), status=422)
    # gh rewords any 404 from autolink create as "Must have admin rights to Repository."
    expect_error(ctx.gh("repo", "autolink", "create", "X-", "https://example.test/<num>", "--repo", absent_repo(ctx), ok=None),
                 status=404, contains="Must have admin rights")
    refused(ctx.sweep.gh(other_account(ctx), "repo", "autolink", "create", "X-", "https://example.test/<num>", "--repo", ctx.repo, ok=None))


@case(command="repo autolink list", variant="empty table json jq template", needs=("repo",))
def autolink_list(ctx):
    no_results(ctx.gh("repo", "autolink", "list", "--repo", ctx.repo, "--json", "id,keyPrefix"))
    row = autolink_fixture(ctx)
    assert row["key_prefix"] in expect_ok(ctx.gh("repo", "autolink", "list", "--repo", ctx.repo)).stdout
    rows = expect_json(ctx.gh("repo", "autolink", "list", "--repo", ctx.repo, "--json", "id,keyPrefix,urlTemplate,isAlphanumeric"),
                       [{"id": row["id"], "keyPrefix": row["key_prefix"], "urlTemplate": row["url_template"], "isAlphanumeric": True}])
    assert len(rows) == 1
    assert expect_ok(ctx.gh("repo", "autolink", "list", "--repo", ctx.repo, "--json", "keyPrefix", "--jq", ".[0].keyPrefix")).stdout.strip() == row["key_prefix"]
    assert expect_ok(ctx.gh("repo", "autolink", "list", "--repo", ctx.repo, "--json", "keyPrefix", "--template", "{{range .}}{{.keyPrefix}}{{end}}")).stdout == row["key_prefix"]


@case(command="repo autolink list", variant="missing and admin-only", needs=("repo",))
def autolink_list_errors(ctx):
    admin_missing(ctx.gh("repo", "autolink", "list", "--repo", absent_repo(ctx), ok=None))
    refused(ctx.sweep.gh(other_account(ctx), "repo", "autolink", "list", "--repo", ctx.repo, ok=None))


@case(command="repo autolink view", variant="table json jq template", needs=("repo",))
def autolink_view(ctx):
    row = autolink_fixture(ctx)
    identifier = str(row["id"])
    assert row["key_prefix"] in expect_ok(ctx.gh("repo", "autolink", "view", identifier, "--repo", ctx.repo)).stdout
    expect_json(ctx.gh("repo", "autolink", "view", identifier, "--repo", ctx.repo, "--json", "id,keyPrefix,urlTemplate,isAlphanumeric"),
                {"id": row["id"], "keyPrefix": row["key_prefix"], "urlTemplate": row["url_template"], "isAlphanumeric": True})
    assert expect_ok(ctx.gh("repo", "autolink", "view", identifier, "--repo", ctx.repo, "--json", "keyPrefix", "--jq", ".keyPrefix")).stdout.strip() == row["key_prefix"]
    assert expect_ok(ctx.gh("repo", "autolink", "view", identifier, "--repo", ctx.repo, "--json", "keyPrefix", "--template", "{{.keyPrefix}}")).stdout == row["key_prefix"]


@case(command="repo autolink view", variant="missing id repo and denied", needs=("repo",))
def autolink_view_errors(ctx):
    row = autolink_fixture(ctx)
    admin_missing(ctx.gh("repo", "autolink", "view", "2147483647", "--repo", ctx.repo, ok=None))
    admin_missing(ctx.gh("repo", "autolink", "view", str(row["id"]), "--repo", absent_repo(ctx), ok=None))
    refused(ctx.sweep.gh(other_account(ctx), "repo", "autolink", "view", str(row["id"]), "--repo", ctx.repo, ok=None))


@case(command="repo autolink delete", variant="delete missing and denied", needs=("repo",))
def autolink_delete(ctx):
    row = autolink_fixture(ctx)
    refused(ctx.sweep.gh(other_account(ctx), "repo", "autolink", "delete", str(row["id"]), "--repo", ctx.repo, "--yes", ok=None))
    expect_ok(ctx.gh("repo", "autolink", "delete", str(row["id"]), "--repo", ctx.repo, "--yes"))
    assert ctx.api("GET", f"repos/{ctx.repo}/autolinks") == []
    admin_missing(ctx.gh("repo", "autolink", "delete", str(row["id"]), "--repo", ctx.repo, "--yes", ok=None))
    admin_missing(ctx.gh("repo", "autolink", "delete", str(row["id"]), "--repo", absent_repo(ctx), "--yes", ok=None))


@case(command="repo clone", variant="ordinary repository", needs=("repo",))
def repo_clone(ctx):
    destination = ctx.directory / ctx.name("clone")
    result = expect_ok(ctx.gh("repo", "clone", ctx.repo, str(destination)))
    assert "Cloning into" in result.stderr + result.stdout
    assert git(ctx, "rev-parse", "--is-inside-work-tree", cwd=destination).stdout.strip() == "true"
    assert (destination / "README.md").is_file()
    assert urlsplit(git(ctx, "remote", "get-url", "origin", cwd=destination).stdout.strip()).path.removesuffix(".git") == "/" + ctx.repo


@case(command="repo clone", variant="fork upstream named owner and disabled")
def repo_clone_upstream(ctx):
    source, fork, account = fork_fixture(ctx)
    custom, owner_named, disabled = (ctx.directory / ctx.name(label) for label in ("custom", "owner", "disabled"))
    expect_ok(ctx.gh("repo", "clone", fork, str(custom), "--upstream-remote-name", "parent"))
    assert urlsplit(git(ctx, "remote", "get-url", "parent", cwd=custom).stdout.strip()).path.removesuffix(".git") == "/" + source
    expect_ok(ctx.gh("repo", "clone", fork, str(owner_named), "--upstream-remote-name", "@owner"))
    assert account.name in git(ctx, "remote", cwd=owner_named).stdout.split()
    expect_ok(ctx.gh("repo", "clone", fork, str(disabled), "--no-upstream"))
    assert git(ctx, "remote", cwd=disabled).stdout.split() == ["origin"]
    assert git(ctx, "config", "--get", "remote.parent.gh-resolved", cwd=custom).stdout.strip() == "base"
    # Without an upstream remote gh makes the fork itself the default repository.
    assert git(ctx, "config", "--get", "remote.origin.gh-resolved", cwd=disabled).stdout.strip() == "base"


@case(command="repo clone", variant="missing and private hidden")
def repo_clone_errors(ctx):
    missing(ctx.gh("repo", "clone", absent_repo(ctx), str(ctx.directory / ctx.name("missing")), ok=None))
    account = other_account(ctx)
    private = ctx.sweep.create_repo(account, ctx.name("private"), private=True)["full_name"]
    missing(ctx.gh("repo", "clone", private, str(ctx.directory / ctx.name("hidden")), ok=None))


@case(command="repo create", variant="public initialized license ignore and metadata")
def repo_create_initialized(ctx):
    name = ctx.name("create")
    full = ctx.account.name + "/" + name
    result = expect_ok(ctx.gh("repo", "create", full, "--public", "--add-readme", "--description", "Sweep initialized repository",
                              "--homepage", "https://example.test/sweep", "--gitignore", "Python", "--license", "MIT"))
    assert "/" + full in result.stdout
    row = ctx.api("GET", f"repos/{full}")
    assert row["private"] is False and row["description"] == "Sweep initialized repository"
    assert row["homepage"].rstrip("/") == "https://example.test/sweep"
    paths = {entry["path"] for entry in ctx.api("GET", f"repos/{full}/contents")}
    assert {"README.md", ".gitignore", "LICENSE"} <= paths, paths


@case(command="repo create", variant="private and clone")
def repo_create_clone(ctx):
    name = ctx.name("create-clone")
    result = expect_ok(ctx.gh("repo", "create", name, "--private", "--add-readme", "--clone"))
    assert name in result.stdout + result.stderr
    assert ctx.api("GET", f"repos/{ctx.account.name}/{name}")["private"] is True
    assert (ctx.directory / name / "README.md").is_file()
    assert git(ctx, "rev-parse", "--is-inside-work-tree", cwd=ctx.directory / name).stdout.strip() == "true"


@case(command="repo create", variant="internal visibility")
def repo_create_internal(ctx):
    # Internal repositories are a GHES feature, not a cloud-only waiver.
    full = ctx.org() + "/" + ctx.name("internal")
    assert full in expect_ok(ctx.gh("repo", "create", full, "--internal")).stdout
    assert ctx.api("GET", f"repos/{full}")["visibility"] == "internal"


@case(command="repo create", variant="disabled issues and wiki")
def repo_create_disabled(ctx):
    full = ctx.account.name + "/" + ctx.name("disabled")
    assert full in expect_ok(ctx.gh("repo", "create", full, "--public", "--disable-issues", "--disable-wiki")).stdout
    row = ctx.api("GET", f"repos/{full}")
    assert row["has_issues"] is False and row["has_wiki"] is False, row


@case(command="repo create", variant="source push custom remote")
def repo_create_source(ctx):
    source = local_repo(ctx)
    full = ctx.account.name + "/" + ctx.name("pushed")
    result = expect_ok(ctx.gh("repo", "create", full, "--private", "--source", str(source), "--remote", "published", "--push"))
    assert full in result.stdout + result.stderr
    assert urlsplit(git(ctx, "remote", "get-url", "published", cwd=source).stdout.strip()).path.removesuffix(".git") == "/" + full
    local_sha = git(ctx, "rev-parse", "HEAD", cwd=source).stdout.strip()
    assert ctx.api("GET", f"repos/{full}/git/ref/heads/main")["object"]["sha"] == local_sha


@case(command="repo create", variant="source without push")
def repo_create_source_no_push(ctx):
    source = local_repo(ctx)
    full = ctx.account.name + "/" + ctx.name("no-push")
    assert full in expect_ok(ctx.gh("repo", "create", full, "--public", "--source", str(source))).stdout
    assert full in git(ctx, "remote", "get-url", "origin", cwd=source).stdout
    assert ctx.api("GET", f"repos/{full}")["size"] == 0


@case(command="repo create", variant="organization team grant")
def repo_create_team(ctx):
    org = ctx.org()
    team = ctx.team(org)
    full = org + "/" + ctx.name("team-repo")
    assert full in expect_ok(ctx.gh("repo", "create", full, "--private", "--team", team["slug"])).stdout
    teams = ctx.api("GET", f"repos/{full}/teams")
    assert any(row["slug"] == team["slug"] for row in teams), teams


@case(command="repo create", variant="template default and all branches", needs=("repo",))
def repo_create_template(ctx):
    ctx.api("PATCH", f"repos/{ctx.repo}", {"is_template": True})
    branch = ctx.name("extra")
    default = ctx.api("GET", f"repos/{ctx.repo}")["default_branch"]
    sha = ctx.api("GET", f"repos/{ctx.repo}/git/ref/heads/{default}")["object"]["sha"]
    ctx.api("POST", f"repos/{ctx.repo}/git/refs", {"ref": "refs/heads/" + branch, "sha": sha})
    first = ctx.account.name + "/" + ctx.name("template-default")
    second = ctx.account.name + "/" + ctx.name("template-all")
    assert first in expect_ok(ctx.gh("repo", "create", first, "--private", "--template", ctx.repo)).stdout
    assert second in expect_ok(ctx.gh("repo", "create", second, "--public", "--template", ctx.repo, "--include-all-branches")).stdout
    assert {row["name"] for row in ctx.api("GET", f"repos/{first}/branches")} == {default}
    assert {row["name"] for row in ctx.api("GET", f"repos/{second}/branches")} == {default, branch}


@case(command="repo create", variant="duplicate missing template forbidden owner", needs=("repo",))
def repo_create_errors(ctx):
    clean_failure(ctx.gh("repo", "create", ctx.repo, "--public", ok=None), "already exists")
    missing(ctx.gh("repo", "create", ctx.account.name + "/" + ctx.name("bad-template"), "--public", "--template", absent_repo(ctx), ok=None))
    org = ctx.org()
    permission_denied(ctx.sweep.gh(other_account(ctx), "repo", "create", org + "/" + ctx.name("denied"), "--public", ok=None))


@case(command="repo create", variant="invalid option combinations")
def repo_create_invalid(ctx):
    clean_failure(ctx.gh("repo", "create", ctx.name("invalid"), "--public", "--private", ok=None), "one")
    clean_failure(ctx.gh("repo", "create", ctx.name("invalid"), "--public", "--push", ok=None), "source")
    expect_error(ctx.gh("repo", "create", ctx.name("bad-ignore"), "--public", "--gitignore", ctx.name("unknown"), ok=None), status=422)


@case(command="repo delete", variant="delete missing denied", needs=("repo",))
def repo_delete(ctx):
    expect_error(ctx.sweep.gh(other_account(ctx), "repo", "delete", ctx.repo, "--yes", ok=None), status=403)
    assert ctx.api("GET", f"repos/{ctx.repo}")["full_name"] == ctx.repo
    expect_ok(ctx.gh("repo", "delete", ctx.repo, "--yes"))
    missing(ctx.gh("repo", "delete", ctx.repo, "--yes", ok=None))


@case(command="repo deploy-key add", variant="read only and write", needs=("repo",))
def deploy_key_add(ctx):
    first, first_text = public_key(ctx)
    second, second_text = public_key(ctx)
    title = ctx.name("write-key")
    expect_ok(ctx.gh("repo", "deploy-key", "add", str(first), "--repo", ctx.repo))
    expect_ok(ctx.gh("repo", "deploy-key", "add", str(second), "--repo", ctx.repo, "--title", title, "--allow-write"))
    rows = ctx.api("GET", f"repos/{ctx.repo}/keys")
    readonly = next(row for row in rows if row["key"].split()[:2] == first_text.split()[:2])
    writable = next(row for row in rows if row["key"].split()[:2] == second_text.split()[:2])
    assert readonly["read_only"] is True
    assert writable["read_only"] is False and writable["title"] == title


@case(command="repo deploy-key add", variant="bad key duplicate missing denied", needs=("repo",))
def deploy_key_add_errors(ctx):
    path, _ = public_key(ctx)
    expect_ok(ctx.gh("repo", "deploy-key", "add", str(path), "--repo", ctx.repo, "--title", ctx.name("key")))
    expect_error(ctx.gh("repo", "deploy-key", "add", str(path), "--repo", ctx.repo, ok=None), status=422)
    bad = ctx.directory / ctx.name("invalid-key")
    bad.write_text("not an ssh key\n")
    expect_error(ctx.gh("repo", "deploy-key", "add", str(bad), "--repo", ctx.repo, ok=None), status=422)
    missing(ctx.gh("repo", "deploy-key", "add", str(path), "--repo", absent_repo(ctx), ok=None))
    expect_error(ctx.sweep.gh(other_account(ctx), "repo", "deploy-key", "add", str(path), "--repo", ctx.repo, ok=None), status=403)


@case(command="repo deploy-key list", variant="empty table json jq template", needs=("repo",))
def deploy_key_list(ctx):
    no_results(ctx.gh("repo", "deploy-key", "list", "--repo", ctx.repo, "--json", "id,title"))
    row = key_fixture(ctx)
    assert row["title"] in expect_ok(ctx.gh("repo", "deploy-key", "list", "--repo", ctx.repo)).stdout
    # gh prints its whole deploy-key struct under the REST names, whatever --json picks.
    rows = expect_json(ctx.gh("repo", "deploy-key", "list", "--repo", ctx.repo, "--json", "id,title,key,readOnly,createdAt"),
                       [{"id": row["id"], "title": row["title"], "key": str, "read_only": True, "created_at": str}])
    assert len(rows) == 1
    assert expect_ok(ctx.gh("repo", "deploy-key", "list", "--repo", ctx.repo, "--json", "title", "--jq", ".[0].title")).stdout.strip() == row["title"]
    assert expect_ok(ctx.gh("repo", "deploy-key", "list", "--repo", ctx.repo, "--json", "title", "--template", "{{range .}}{{.title}}{{end}}")).stdout == row["title"]


@case(command="repo deploy-key list", variant="missing and denied", needs=("repo",))
def deploy_key_list_errors(ctx):
    missing(ctx.gh("repo", "deploy-key", "list", "--repo", absent_repo(ctx), ok=None))
    expect_error(ctx.sweep.gh(other_account(ctx), "repo", "deploy-key", "list", "--repo", ctx.repo, ok=None), status=403)


@case(command="repo deploy-key delete", variant="delete missing denied", needs=("repo",))
def deploy_key_delete(ctx):
    row = key_fixture(ctx)
    expect_error(ctx.sweep.gh(other_account(ctx), "repo", "deploy-key", "delete", str(row["id"]), "--repo", ctx.repo, ok=None), status=403)
    expect_ok(ctx.gh("repo", "deploy-key", "delete", str(row["id"]), "--repo", ctx.repo))
    assert ctx.api("GET", f"repos/{ctx.repo}/keys") == []
    missing(ctx.gh("repo", "deploy-key", "delete", str(row["id"]), "--repo", ctx.repo, ok=None))
    missing(ctx.gh("repo", "deploy-key", "delete", str(row["id"]), "--repo", absent_repo(ctx), ok=None))


@case(command="repo edit", variant="metadata topics template branch", needs=("repo",))
def repo_edit_metadata(ctx):
    topic, branch = ctx.name("topic"), ctx.name("branch")
    default = ctx.api("GET", f"repos/{ctx.repo}")["default_branch"]
    sha = ctx.api("GET", f"repos/{ctx.repo}/git/ref/heads/{default}")["object"]["sha"]
    ctx.api("POST", f"repos/{ctx.repo}/git/refs", {"ref": "refs/heads/" + branch, "sha": sha})
    expect_ok(ctx.gh("repo", "edit", ctx.repo, "--description", "Edited by sweep", "--homepage", "https://example.test/edited",
                     "--add-topic", topic, "--template", "--default-branch", branch))
    row = ctx.api("GET", f"repos/{ctx.repo}")
    assert row["description"] == "Edited by sweep" and row["homepage"].rstrip("/") == "https://example.test/edited"
    assert row["is_template"] is True and row["default_branch"] == branch
    assert topic in ctx.api("GET", f"repos/{ctx.repo}/topics")["names"]
    expect_ok(ctx.gh("repo", "edit", ctx.repo, "--remove-topic", topic, "--template=false", "--description", "", "--homepage", ""))
    row = ctx.api("GET", f"repos/{ctx.repo}")
    assert not row["is_template"] and not row["description"] and not row["homepage"]
    assert topic not in ctx.api("GET", f"repos/{ctx.repo}/topics")["names"]


@case(command="repo edit", variant="visibility changes acknowledgement", needs=("repo",))
def repo_edit_visibility(ctx):
    clean_failure(ctx.gh("repo", "edit", ctx.repo, "--visibility", "private", ok=None), "accept-visibility-change-consequences")
    expect_ok(ctx.gh("repo", "edit", ctx.repo, "--visibility", "private", "--accept-visibility-change-consequences"))
    assert ctx.api("GET", f"repos/{ctx.repo}")["private"] is True
    expect_ok(ctx.gh("repo", "edit", ctx.repo, "--visibility", "public", "--accept-visibility-change-consequences"))
    assert ctx.api("GET", f"repos/{ctx.repo}")["private"] is False


@case(command="repo edit", variant="wiki discussions toggles", needs=("repo",))
def repo_edit_discussions(ctx):
    expect_ok(ctx.gh("repo", "edit", ctx.repo, "--enable-wiki", "--enable-discussions"))
    row = ctx.api("GET", f"repos/{ctx.repo}")
    assert row["has_wiki"] is True and row["has_discussions"] is True
    expect_ok(ctx.gh("repo", "edit", ctx.repo, "--enable-wiki=false", "--enable-discussions=false"))
    row = ctx.api("GET", f"repos/{ctx.repo}")
    assert row["has_wiki"] is False and row["has_discussions"] is False


@case(command="repo edit", variant="merge methods and squash message", needs=("repo",))
def repo_edit_merge(ctx):
    expect_ok(ctx.gh("repo", "edit", ctx.repo, "--enable-merge-commit", "--enable-rebase-merge", "--enable-squash-merge"))
    row = ctx.api("GET", f"repos/{ctx.repo}")
    assert all(row[key] for key in ("allow_merge_commit", "allow_rebase_merge", "allow_squash_merge"))
    expect_ok(ctx.gh("repo", "edit", ctx.repo, "--enable-merge-commit=false", "--enable-rebase-merge=false", "--enable-squash-merge", "--squash-merge-commit-message", "pr-title-description"))
    row = ctx.api("GET", f"repos/{ctx.repo}")
    assert row["allow_merge_commit"] is False and row["allow_rebase_merge"] is False and row["allow_squash_merge"] is True
    assert row["squash_merge_commit_title"] == "PR_TITLE" and row["squash_merge_commit_message"] == "PR_BODY"
    expect_ok(ctx.gh("repo", "edit", ctx.repo, "--enable-merge-commit", "--enable-squash-merge=false"))
    assert ctx.api("GET", f"repos/{ctx.repo}")["allow_squash_merge"] is False


@case(command="repo edit", variant="issues toggled", needs=("repo",))
def repo_edit_issues(ctx):
    expect_ok(ctx.gh("repo", "edit", ctx.repo, "--enable-issues=false"))
    assert ctx.api("GET", f"repos/{ctx.repo}")["has_issues"] is False
    expect_ok(ctx.gh("repo", "edit", ctx.repo, "--enable-issues"))
    assert ctx.api("GET", f"repos/{ctx.repo}")["has_issues"] is True


@case(command="repo edit", variant="projects toggled", needs=("repo",))
def repo_edit_projects(ctx):
    expect_ok(ctx.gh("repo", "edit", ctx.repo, "--enable-projects"))
    assert ctx.api("GET", f"repos/{ctx.repo}")["has_projects"] is True
    expect_ok(ctx.gh("repo", "edit", ctx.repo, "--enable-projects=false"))
    assert ctx.api("GET", f"repos/{ctx.repo}")["has_projects"] is False


@case(command="repo edit", variant="forking organization repository")
def repo_edit_forking(ctx):
    repo = ctx.sweep.create_repo(ctx.account, ctx.name("org-repo"), org=ctx.org())["full_name"]
    expect_ok(ctx.gh("repo", "edit", repo, "--allow-forking=false"))
    assert ctx.api("GET", f"repos/{repo}")["allow_forking"] is False
    expect_ok(ctx.gh("repo", "edit", repo, "--allow-forking"))
    assert ctx.api("GET", f"repos/{repo}")["allow_forking"] is True


@case(command="repo edit", variant="update branch toggled", needs=("repo",))
def repo_edit_update_branch(ctx):
    expect_ok(ctx.gh("repo", "edit", ctx.repo, "--allow-update-branch"))
    assert ctx.api("GET", f"repos/{ctx.repo}")["allow_update_branch"] is True
    expect_ok(ctx.gh("repo", "edit", ctx.repo, "--allow-update-branch=false"))
    assert ctx.api("GET", f"repos/{ctx.repo}")["allow_update_branch"] is False


@case(command="repo edit", variant="delete merged branches toggled", needs=("repo",))
def repo_edit_delete_branch(ctx):
    expect_ok(ctx.gh("repo", "edit", ctx.repo, "--delete-branch-on-merge"))
    assert ctx.api("GET", f"repos/{ctx.repo}")["delete_branch_on_merge"] is True
    expect_ok(ctx.gh("repo", "edit", ctx.repo, "--delete-branch-on-merge=false"))
    assert ctx.api("GET", f"repos/{ctx.repo}")["delete_branch_on_merge"] is False


@case(command="repo edit", variant="auto merge toggled", needs=("repo",))
def repo_edit_auto_merge(ctx):
    expect_ok(ctx.gh("repo", "edit", ctx.repo, "--enable-auto-merge"))
    assert ctx.api("GET", f"repos/{ctx.repo}")["allow_auto_merge"] is True
    expect_ok(ctx.gh("repo", "edit", ctx.repo, "--enable-auto-merge=false"))
    assert ctx.api("GET", f"repos/{ctx.repo}")["allow_auto_merge"] is False


@case(command="repo edit", variant="advanced security unavailable on unlicensed host", needs=("repo",))
def repo_edit_advanced_security(ctx):
    expect_error(ctx.gh("repo", "edit", ctx.repo, "--enable-advanced-security", ok=None), status=422, contains="Security analysis services are not available")


@case(command="repo edit", variant="secret scanning unavailable on unlicensed host", needs=("repo",))
def repo_edit_secret_scanning(ctx):
    expect_error(ctx.gh("repo", "edit", ctx.repo, "--enable-secret-scanning", ok=None), status=422, contains="Security analysis services are not available")


@case(command="repo edit", variant="push protection requires scanning on unlicensed host", needs=("repo",))
def repo_edit_push_protection(ctx):
    expect_error(ctx.gh("repo", "edit", ctx.repo, "--enable-secret-scanning", "--enable-secret-scanning-push-protection", ok=None), status=422, contains="Security analysis services are not available")
    expect_error(ctx.gh("repo", "edit", ctx.repo, "--enable-secret-scanning-push-protection", ok=None), status=422)


@case(command="repo edit", variant="missing forbidden bad branch and enum", needs=("repo",))
def repo_edit_errors(ctx):
    missing(ctx.gh("repo", "edit", absent_repo(ctx), "--description", "missing", ok=None))
    expect_error(ctx.sweep.gh(other_account(ctx), "repo", "edit", ctx.repo, "--description", "denied", ok=None), status=403)
    expect_error(ctx.gh("repo", "edit", ctx.repo, "--default-branch", ctx.name("absent"), ok=None), status=422)
    clean_failure(ctx.gh("repo", "edit", ctx.repo, "--visibility", "invalid", "--accept-visibility-change-consequences", ok=None), "visibility")
    clean_failure(ctx.gh("repo", "edit", ctx.repo, "--enable-squash-merge", "--squash-merge-commit-message", "invalid", ok=None), "squash-merge-commit-message")


@case(command="repo fork", variant="named org fork default branch only")
def repo_fork(ctx):
    account = other_account(ctx)
    source = ctx.sweep.create_repo(account, ctx.name("source"))["full_name"]
    default = ctx.api("GET", f"repos/{source}")["default_branch"]
    sha = ctx.api("GET", f"repos/{source}/git/ref/heads/{default}")["object"]["sha"]
    branch = ctx.name("extra")
    ctx.sweep.api(account, "POST", f"repos/{source}/git/refs", {"ref": "refs/heads/" + branch, "sha": sha})
    org, name = ctx.org(), ctx.name("fork")
    result = expect_ok(ctx.gh("repo", "fork", source, "--org", org, "--fork-name", name,
                              "--default-branch-only", "--clone=false"))
    assert name in result.stdout + result.stderr
    row = ctx.api("GET", f"repos/{org}/{name}")
    assert row["fork"] is True and row["parent"]["full_name"] == source
    assert {entry["name"] for entry in ctx.api("GET", f"repos/{org}/{name}/branches")} == {default}
    full_name = ctx.name("all-branches")
    expect_ok(ctx.gh("repo", "fork", source, "--fork-name", full_name, "--clone=false"))
    assert {entry["name"] for entry in ctx.api("GET", f"repos/{ctx.account.name}/{full_name}/branches")} == {default, branch}


@case(command="repo fork", variant="clone fork and local custom remote")
def repo_fork_clone_remote(ctx):
    account = other_account(ctx)
    source = ctx.sweep.create_repo(account, ctx.name("source"))["full_name"]
    name = ctx.name("fork")
    expect_ok(ctx.gh("repo", "fork", source, "--clone", "--fork-name", name))
    path = ctx.directory / name
    assert (path / "README.md").is_file()
    assert ctx.api("GET", f"repos/{ctx.account.name}/{name}")["parent"]["full_name"] == source
    local = local_repo(ctx)
    git(ctx, "remote", "add", "origin", "https://codeyard-e2e.test/" + source + ".git", cwd=local)
    expect_ok(ctx.gh("repo", "fork", "--remote", "--remote-name", "mine", cwd=local))
    assert "/" + ctx.account.name + "/" + name in git(ctx, "remote", "get-url", "mine", cwd=local).stdout
    assert "/" + source in git(ctx, "remote", "get-url", "origin", cwd=local).stdout


@case(command="repo fork", variant="missing and private source hidden")
def repo_fork_errors(ctx):
    missing(ctx.gh("repo", "fork", absent_repo(ctx), "--clone=false", ok=None))
    account = other_account(ctx)
    private = ctx.sweep.create_repo(account, ctx.name("private"), private=True)["full_name"]
    missing(ctx.gh("repo", "fork", private, "--clone=false", ok=None))


@case(command="repo gitignore list", variant="catalog")
def gitignore_list(ctx):
    output = expect_ok(ctx.gh("repo", "gitignore", "list")).stdout
    assert "Python" in output and "Rust" in output, output


@case(command="repo gitignore view", variant="template text and missing")
def gitignore_view(ctx):
    output = expect_ok(ctx.gh("repo", "gitignore", "view", "Python")).stdout
    assert "__pycache__" in output, output
    # gh turns the 404 into its own hint.
    clean_failure(ctx.gh("repo", "gitignore", "view", ctx.name("unknown"), ok=None), "is not a valid gitignore template")


@case(command="repo license list", variant="catalog")
def license_list(ctx):
    output = expect_ok(ctx.gh("repo", "license", "list")).stdout
    assert "MIT" in output and "Apache" in output, output


@case(command="repo license view", variant="key spdx and missing")
def license_view(ctx):
    by_key = expect_ok(ctx.gh("repo", "license", "view", "mit")).stdout
    by_spdx = expect_ok(ctx.gh("repo", "license", "view", "MIT")).stdout
    assert by_key == by_spdx and "Permission is hereby granted" in by_key, by_key
    clean_failure(ctx.gh("repo", "license", "view", ctx.name("unknown"), ok=None), "is not a valid license name or SPDX ID")


@case(command="repo list", variant="owner private source json jq template")
def repo_list(ctx):
    org = ctx.org()
    repo = ctx.sweep.create_repo(ctx.account, ctx.name("listed"), org=org, private=True)["full_name"]
    name = repo.split("/")[1]
    rows = expect_json(ctx.gh("repo", "list", org, "--limit", "10", "--visibility", "private", "--source", "--json", "nameWithOwner,isPrivate,isFork"),
                       [{"nameWithOwner": repo, "isPrivate": True, "isFork": False}])
    assert len(rows) == 1
    assert repo in expect_ok(ctx.gh("repo", "list", org)).stdout
    assert expect_ok(ctx.gh("repo", "list", org, "--json", "name", "--jq", ".[0].name")).stdout.strip() == name
    assert expect_ok(ctx.gh("repo", "list", org, "--json", "name", "--template", "{{range .}}{{.name}}{{end}}")).stdout == name
    expect_json(ctx.gh("repo", "list", org, "--visibility", "public", "--json", "name"), [])


@case(command="repo list", variant="archived and no archived")
def repo_list_archived(ctx):
    org = ctx.org()
    active = ctx.sweep.create_repo(ctx.account, ctx.name("active"), org=org)["full_name"]
    archived = ctx.sweep.create_repo(ctx.account, ctx.name("archived"), org=org)["full_name"]
    ctx.api("PATCH", f"repos/{archived}", {"archived": True})
    rows = expect_json(ctx.gh("repo", "list", org, "--archived", "--json", "nameWithOwner,isArchived"),
                       [{"nameWithOwner": archived, "isArchived": True}])
    assert len(rows) == 1
    rows = expect_json(ctx.gh("repo", "list", org, "--no-archived", "--json", "nameWithOwner,isArchived"),
                       [{"nameWithOwner": active, "isArchived": False}])
    assert len(rows) == 1


@case(command="repo list", variant="language topic intersection and limit")
def repo_list_search(ctx):
    org = ctx.org()
    repo = ctx.sweep.create_repo(ctx.account, ctx.name("python"), org=org)["full_name"]
    topic = ctx.name("topic")
    ctx.api("PUT", f"repos/{repo}/contents/main.py", {"message": "language fixture", "content": base64.b64encode(b"print('sweep')\n").decode()})
    ctx.api("PUT", f"repos/{repo}/topics", {"names": [topic]})
    rows = expect_json(ctx.gh("repo", "list", org, "--language", "Python", "--topic", topic, "--limit", "1", "--json", "nameWithOwner"), [{"nameWithOwner": repo}])
    assert len(rows) == 1
    expect_json(ctx.gh("repo", "list", org, "--language", "Rust", "--topic", topic, "--json", "nameWithOwner"), [])


@case(command="repo list", variant="fork filter")
def repo_list_forks(ctx):
    account = other_account(ctx)
    source = ctx.sweep.create_repo(account, ctx.name("source"))["full_name"]
    org = ctx.org()
    name = ctx.name("fork")
    ctx.api("POST", f"repos/{source}/forks", {"organization": org, "name": name})
    rows = expect_json(ctx.gh("repo", "list", org, "--fork", "--json", "nameWithOwner,isFork"), [{"nameWithOwner": org + "/" + name, "isFork": True}])
    assert len(rows) == 1
    expect_json(ctx.gh("repo", "list", org, "--source", "--json", "name"), [])


@case(command="repo list", variant="unknown owner and invalid limit")
def repo_list_errors(ctx):
    # GitHub answers an unknown repositoryOwner with null, which gh lists as nothing; a NOT_FOUND
    # error is also a clean answer.
    result = ctx.gh("repo", "list", ctx.name("absent-owner"), ok=None)
    if result.rc == 0:
        assert result.stdout == "", result.stdout
    else:
        missing(result)
    clean_failure(ctx.gh("repo", "list", ctx.account.name, "--limit", "0", ok=None), "limit")


@case(command="repo read-dir", variant="root subdir ref json jq template", needs=("repo",))
def repo_read_dir(ctx):
    folder = ctx.name("dir")
    file_fixture(ctx, path=folder + "/entry.txt")
    default = ctx.api("GET", f"repos/{ctx.repo}")["default_branch"]
    assert folder in expect_ok(ctx.gh("repo", "read-dir", "--repo", ctx.repo)).stdout
    # gh exports the directory itself: {gitSHA, id, entries: [...]}, with the fields picked per entry.
    tree = expect_json(ctx.gh("repo", "read-dir", folder, "--repo", ctx.repo, "--ref", default, "--json", "name,path,type,size,gitSHA"),
                       {"gitSHA": str, "entries": [{"name": "entry.txt", "path": folder + "/entry.txt", "type": "file", "size": len(b"sweep file\n"), "gitSHA": str}]})
    assert len(tree["entries"]) == 1
    assert expect_ok(ctx.gh("repo", "read-dir", folder, "--repo", ctx.repo, "--json", "name", "--jq", ".entries[0].name")).stdout.strip() == "entry.txt"
    assert expect_ok(ctx.gh("repo", "read-dir", folder, "--repo", ctx.repo, "--json", "path", "--template", "{{range .entries}}{{.path}}{{end}}")).stdout == folder + "/entry.txt"


@case(command="repo read-dir", variant="nondefault ref changes directory contents", needs=("repo",))
def repo_read_dir_ref(ctx):
    default = ctx.api("GET", f"repos/{ctx.repo}")["default_branch"]
    sha = ctx.api("GET", f"repos/{ctx.repo}/git/ref/heads/{default}")["object"]["sha"]
    branch, path = ctx.name("branch"), ctx.name("branch-only") + ".txt"
    ctx.api("POST", f"repos/{ctx.repo}/git/refs", {"ref": "refs/heads/" + branch, "sha": sha})
    ctx.api("PUT", f"repos/{ctx.repo}/contents/{path}",
            {"message": "branch-only file", "branch": branch, "content": base64.b64encode(b"branch only\n").decode()})
    original = expect_json(ctx.gh("repo", "read-dir", "--repo", ctx.repo, "--json", "name"), {"entries": [{"name": str}]})
    selected = expect_json(ctx.gh("repo", "read-dir", "--repo", ctx.repo, "--ref", branch, "--json", "name"), {"entries": [{"name": str}]})
    assert path not in {row["name"] for row in original["entries"]}, original
    assert path in {row["name"] for row in selected["entries"]}, selected


@case(command="repo read-dir", variant="missing path ref repo and private hidden", needs=("repo",))
def repo_read_dir_errors(ctx):
    # GitHub returns a null object for a missing path or ref, which gh reports as "could not find".
    clean_failure(ctx.gh("repo", "read-dir", ctx.name("absent"), "--repo", ctx.repo, ok=None), "could not find")
    clean_failure(ctx.gh("repo", "read-dir", "--repo", ctx.repo, "--ref", ctx.name("bad-ref"), ok=None), "could not find")
    missing(ctx.gh("repo", "read-dir", "--repo", absent_repo(ctx), ok=None))
    ctx.api("PATCH", f"repos/{ctx.repo}", {"private": True})
    missing(ctx.sweep.gh(other_account(ctx), "repo", "read-dir", "--repo", ctx.repo, ok=None))


@case(command="repo read-file", variant="raw ref json jq template", needs=("repo",))
def repo_read_file(ctx):
    content = b"sweep file\n"
    path = file_fixture(ctx, content)
    default = ctx.api("GET", f"repos/{ctx.repo}")["default_branch"]
    assert expect_ok(ctx.gh("repo", "read-file", path, "--repo", ctx.repo, "--ref", default)).stdout == content.decode()
    expect_json(ctx.gh("repo", "read-file", path, "--repo", ctx.repo, "--json", "name,path,size,type,encoding,content"),
                {"name": path, "path": path, "size": len(content), "type": "file", "encoding": "base64", "content": str})
    assert expect_ok(ctx.gh("repo", "read-file", path, "--repo", ctx.repo, "--json", "path", "--jq", ".path")).stdout.strip() == path
    assert expect_ok(ctx.gh("repo", "read-file", path, "--repo", ctx.repo, "--json", "path", "--template", "{{.path}}")).stdout == path


@case(command="repo read-file", variant="nondefault branch content", needs=("repo",))
def repo_read_file_ref(ctx):
    path = file_fixture(ctx, b"default content\n")
    default = ctx.api("GET", f"repos/{ctx.repo}")["default_branch"]
    sha = ctx.api("GET", f"repos/{ctx.repo}/git/ref/heads/{default}")["object"]["sha"]
    branch = ctx.name("branch")
    ctx.api("POST", f"repos/{ctx.repo}/git/refs", {"ref": "refs/heads/" + branch, "sha": sha})
    blob = ctx.api("GET", f"repos/{ctx.repo}/contents/{path}")
    ctx.api("PUT", f"repos/{ctx.repo}/contents/{path}",
            {"message": "branch content", "branch": branch, "sha": blob["sha"],
             "content": base64.b64encode(b"selected branch content\n").decode()})
    assert expect_ok(ctx.gh("repo", "read-file", path, "--repo", ctx.repo)).stdout == "default content\n"
    assert expect_ok(ctx.gh("repo", "read-file", path, "--repo", ctx.repo, "--ref", branch)).stdout == "selected branch content\n"


@case(command="repo read-file", variant="output existing path and clobber", needs=("repo",))
def repo_read_file_output(ctx):
    path = file_fixture(ctx)
    output = ctx.directory / ctx.name("download")
    expect_ok(ctx.gh("repo", "read-file", path, "--repo", ctx.repo, "--output", str(output)))
    assert output.read_bytes() == b"sweep file\n"
    output.write_bytes(b"keep me\n")
    clean_failure(ctx.gh("repo", "read-file", path, "--repo", ctx.repo, "--output", str(output), ok=None), "exist")
    assert output.read_bytes() == b"keep me\n"
    expect_ok(ctx.gh("repo", "read-file", path, "--repo", ctx.repo, "--output", str(output), "--clobber"))
    assert output.read_bytes() == b"sweep file\n"


@case(command="repo read-file", variant="escape sequence safety and disk bypass", needs=("repo",))
def repo_read_file_escape(ctx):
    content = b"\x1b[31mred\x1b[0m\n"
    path = file_fixture(ctx, content)
    clean_failure(ctx.gh("repo", "read-file", path, "--repo", ctx.repo, ok=None), "escape")
    assert expect_ok(ctx.gh("repo", "read-file", path, "--repo", ctx.repo, "--allow-escape-sequences")).stdout == content.decode()
    output = ctx.directory / ctx.name("escape-output")
    expect_ok(ctx.gh("repo", "read-file", path, "--repo", ctx.repo, "--output", str(output)))
    assert output.read_bytes() == content


@case(command="repo read-file", variant="missing file ref repo private hidden", needs=("repo",))
def repo_read_file_errors(ctx):
    missing(ctx.gh("repo", "read-file", ctx.name("absent"), "--repo", ctx.repo, ok=None))
    missing(ctx.gh("repo", "read-file", "README.md", "--repo", ctx.repo, "--ref", ctx.name("bad-ref"), ok=None))
    missing(ctx.gh("repo", "read-file", "README.md", "--repo", absent_repo(ctx), ok=None))
    ctx.api("PATCH", f"repos/{ctx.repo}", {"private": True})
    missing(ctx.sweep.gh(other_account(ctx), "repo", "read-file", "README.md", "--repo", ctx.repo, ok=None))


@case(command="repo rename", variant="explicit repo yes", needs=("repo",))
def repo_rename(ctx):
    name = ctx.name("renamed")
    expect_ok(ctx.gh("repo", "rename", name, "--repo", ctx.repo, "--yes"))
    assert ctx.api("GET", f"repos/{ctx.account.name}/{name}")["name"] == name


@case(command="repo rename", variant="missing denied collision", needs=("repo",))
def repo_rename_errors(ctx):
    missing(ctx.gh("repo", "rename", ctx.name("new"), "--repo", absent_repo(ctx), "--yes", ok=None))
    permission_denied(ctx.sweep.gh(other_account(ctx), "repo", "rename", ctx.name("new"), "--repo", ctx.repo, "--yes", ok=None))
    existing = ctx.sweep.create_repo(ctx.account, ctx.name("collision"))["name"]
    clean_failure(ctx.gh("repo", "rename", existing, "--repo", ctx.repo, "--yes", ok=None), "already reserved")
    assert ctx.api("GET", f"repos/{ctx.repo}")["full_name"] == ctx.repo


@case(command="repo set-default", variant="set view unset isolated local config", needs=("repo",))
def repo_set_default(ctx):
    local = local_repo(ctx)
    git(ctx, "remote", "add", "origin", "https://codeyard-e2e.test/" + ctx.repo + ".git", cwd=local)
    expect_ok(ctx.gh("repo", "set-default", ctx.repo, cwd=local))
    assert ctx.repo in expect_ok(ctx.gh("repo", "set-default", "--view", cwd=local)).stdout
    assert git(ctx, "config", "--get", "remote.origin.gh-resolved", cwd=local).stdout.strip() == "base"
    expect_ok(ctx.gh("repo", "set-default", "--unset", cwd=local))
    result = git(ctx, "config", "--get", "remote.origin.gh-resolved", cwd=local, ok=(1,))
    assert not result.stdout
    # With no default set, --view explains that on stderr and still exits 0.
    result = expect_ok(ctx.gh("repo", "set-default", "--view", cwd=local))
    assert "No default remote repository has been set" in result.stderr, result


@case(command="repo set-default", variant="outside git repository")
def repo_set_default_errors(ctx):
    clean_failure(ctx.gh("repo", "set-default", "--view", ok=None), "git repository")
    clean_failure(ctx.gh("repo", "set-default", "--unset", ok=None), "git repository")


@case(command="repo sync", variant="parent fast forward branch and forced reset")
def repo_sync(ctx):
    source, fork, account = fork_fixture(ctx)
    branch = ctx.api("GET", f"repos/{source}")["default_branch"]
    ctx.sweep.api(account, "PUT", f"repos/{source}/contents/advance.txt",
                  {"message": "advance upstream", "content": base64.b64encode(b"advance\n").decode()})
    expect_ok(ctx.gh("repo", "sync", fork))
    source_sha = ctx.api("GET", f"repos/{source}/git/ref/heads/{branch}")["object"]["sha"]
    fork_sha = ctx.api("GET", f"repos/{fork}/git/ref/heads/{branch}")["object"]["sha"]
    assert fork_sha == source_sha, ("fast-forward sync", fork_sha, source_sha)
    # A conflicting change on each side: merge-upstream refuses it with 409, and gh --force then
    # resets the fork's branch to the source commit. (A clean divergence would be merged instead.)
    fork_blob = ctx.api("GET", f"repos/{fork}/contents/advance.txt")["sha"]
    ctx.api("PUT", f"repos/{fork}/contents/advance.txt",
            {"message": "diverge", "sha": fork_blob, "content": base64.b64encode(b"fork side\n").decode()})
    source_blob = ctx.sweep.api(account, "GET", f"repos/{source}/contents/advance.txt")["sha"]
    ctx.sweep.api(account, "PUT", f"repos/{source}/contents/advance.txt",
                  {"message": "upstream side", "sha": source_blob, "content": base64.b64encode(b"upstream side\n").decode()})
    source_sha = ctx.api("GET", f"repos/{source}/git/ref/heads/{branch}")["object"]["sha"]
    expect_ok(ctx.gh("repo", "sync", fork, "--source", source, "--branch", branch, "--force"))
    fork_sha = ctx.api("GET", f"repos/{fork}/git/ref/heads/{branch}")["object"]["sha"]
    assert fork_sha == source_sha, ("forced sync", fork_sha, source_sha)


@case(command="repo sync", variant="missing branch destination and non-owner")
def repo_sync_errors(ctx):
    source, fork, _ = fork_fixture(ctx)
    missing(ctx.gh("repo", "sync", absent_repo(ctx), "--source", source, ok=None))
    missing(ctx.gh("repo", "sync", fork, "--source", source, "--branch", ctx.name("absent"), ok=None))
    expect_error(ctx.sweep.gh(other_account(ctx), "repo", "sync", fork, "--source", source, ok=None), status=403)


@case(command="repo view", variant="text branch json jq template", needs=("repo",))
def repo_view(ctx):
    readme = ctx.api("GET", f"repos/{ctx.repo}/contents/README.md")
    ctx.api("PUT", f"repos/{ctx.repo}/contents/README.md",
            {"message": "sweep readme", "sha": readme["sha"],
             "content": base64.b64encode(b"# sweep readme marker\n").decode()})
    default = ctx.api("GET", f"repos/{ctx.repo}")["default_branch"]
    result = expect_ok(ctx.gh("repo", "view", ctx.repo, "--branch", default))
    assert ctx.repo in result.stdout and "sweep readme marker" in result.stdout, result.stdout
    expect_json(ctx.gh("repo", "view", ctx.repo, "--json", "nameWithOwner,isPrivate,viewerPermission"),
                {"nameWithOwner": ctx.repo, "isPrivate": False, "viewerPermission": "ADMIN"})
    assert expect_ok(ctx.gh("repo", "view", ctx.repo, "--json", "nameWithOwner", "--jq", ".nameWithOwner")).stdout.strip() == ctx.repo
    assert expect_ok(ctx.gh("repo", "view", ctx.repo, "--json", "nameWithOwner", "--template", "{{.nameWithOwner}}")).stdout == ctx.repo


@case(command="repo view", variant="nondefault branch readme", needs=("repo",))
def repo_view_branch(ctx):
    default = ctx.api("GET", f"repos/{ctx.repo}")["default_branch"]
    sha = ctx.api("GET", f"repos/{ctx.repo}/git/ref/heads/{default}")["object"]["sha"]
    branch, marker = ctx.name("branch"), ctx.name("readme-marker")
    ctx.api("POST", f"repos/{ctx.repo}/git/refs", {"ref": "refs/heads/" + branch, "sha": sha})
    blob = ctx.api("GET", f"repos/{ctx.repo}/contents/README.md")
    ctx.api("PUT", f"repos/{ctx.repo}/contents/README.md",
            {"message": "branch readme", "branch": branch, "sha": blob["sha"],
             "content": base64.b64encode((marker + "\n").encode()).decode()})
    assert marker not in expect_ok(ctx.gh("repo", "view", ctx.repo)).stdout
    assert marker in expect_ok(ctx.gh("repo", "view", ctx.repo, "--branch", branch)).stdout


@case(command="repo view", variant="missing and private hidden", needs=("repo",))
def repo_view_errors(ctx):
    missing(ctx.gh("repo", "view", absent_repo(ctx), "--json", "name", ok=None))
    ctx.api("PATCH", f"repos/{ctx.repo}", {"private": True})
    missing(ctx.sweep.gh(other_account(ctx), "repo", "view", ctx.repo, "--json", "name", ok=None))


@case(command="ruleset check", variant="default explicit branch and nonmatching branch", needs=("repo",))
def ruleset_check(ctx):
    # Only active rulesets apply to a branch; the other cases use evaluate mode.
    ruleset_fixture(ctx, enforcement="active")
    default = ctx.api("GET", f"repos/{ctx.repo}")["default_branch"]
    result = expect_ok(ctx.gh("ruleset", "check", "--default", "--repo", ctx.repo))
    assert "deletion" in result.stdout.lower(), result.stdout
    result = expect_ok(ctx.gh("ruleset", "check", default, "--repo", ctx.repo))
    assert "deletion" in result.stdout.lower(), result.stdout
    result = expect_ok(ctx.gh("ruleset", "check", ctx.name("nonmatching"), "--repo", ctx.repo))
    assert "no rules" in (result.stdout + result.stderr).lower(), result


@case(command="ruleset check", variant="missing repo and private hidden", needs=("repo",))
def ruleset_check_errors(ctx):
    missing(ctx.gh("ruleset", "check", "main", "--repo", absent_repo(ctx), ok=None))
    ctx.api("PATCH", f"repos/{ctx.repo}", {"private": True})
    missing(ctx.sweep.gh(other_account(ctx), "ruleset", "check", "--default", "--repo", ctx.repo, ok=None))


@case(command="ruleset list", variant="repo organization inheritance limits")
def ruleset_list(ctx):
    org = ctx.org()
    repo = ctx.sweep.create_repo(ctx.account, ctx.name("repo"), org=org)["full_name"]
    ctx.fixtures["repo"] = repo
    parent = ruleset_fixture(ctx, org=org)
    child = ruleset_fixture(ctx)
    result = expect_ok(ctx.gh("ruleset", "list", "--repo", repo, "--parents", "--limit", "100"))
    assert parent["name"] in result.stdout and child["name"] in result.stdout, result.stdout
    result = expect_ok(ctx.gh("ruleset", "list", "--repo", repo, "--parents=false"))
    assert child["name"] in result.stdout and parent["name"] not in result.stdout, result.stdout
    result = expect_ok(ctx.gh("ruleset", "list", "--org", org, "--parents", "--limit", "1"))
    assert parent["name"] in result.stdout and child["name"] not in result.stdout, result.stdout


@case(command="ruleset list", variant="empty missing and unauthorized org", needs=("repo",))
def ruleset_list_errors(ctx):
    no_results(ctx.gh("ruleset", "list", "--repo", ctx.repo))
    missing(ctx.gh("ruleset", "list", "--repo", absent_repo(ctx), ok=None))
    org = ctx.org()
    clean_failure(ctx.sweep.gh(other_account(ctx), "ruleset", "list", "--org", org, ok=None), "organization access")


@case(command="ruleset view", variant="repository and organization inherited rules")
def ruleset_view(ctx):
    org = ctx.org()
    ctx.fixtures["repo"] = ctx.sweep.create_repo(ctx.account, ctx.name("repo"), org=org)["full_name"]
    parent, child = ruleset_fixture(ctx, org=org), ruleset_fixture(ctx)
    result = expect_ok(ctx.gh("ruleset", "view", str(child["id"]), "--repo", ctx.repo, "--parents=false"))
    assert child["name"] in result.stdout and "deletion" in result.stdout.lower(), result.stdout
    result = expect_ok(ctx.gh("ruleset", "view", str(parent["id"]), "--repo", ctx.repo, "--parents"))
    assert parent["name"] in result.stdout, result.stdout
    result = expect_ok(ctx.gh("ruleset", "view", str(parent["id"]), "--org", org, "--parents=false"))
    assert parent["name"] in result.stdout and "deletion" in result.stdout.lower(), result.stdout
    # --parents only filters gh's interactive picker; a ruleset id is fetched as it is.
    result = expect_ok(ctx.gh("ruleset", "view", str(parent["id"]), "--repo", ctx.repo, "--parents=false"))
    assert parent["name"] in result.stdout, result.stdout


@case(command="ruleset view", variant="missing id repo org and denied", needs=("repo",))
def ruleset_view_errors(ctx):
    missing(ctx.gh("ruleset", "view", "2147483647", "--repo", ctx.repo, ok=None))
    missing(ctx.gh("ruleset", "view", "2147483647", "--repo", absent_repo(ctx), ok=None))
    missing(ctx.gh("ruleset", "view", "2147483647", "--org", ctx.name("absent-org"), ok=None))
    org = ctx.org()
    parent = ruleset_fixture(ctx, org=org)
    expect_error(ctx.sweep.gh(other_account(ctx), "ruleset", "view", str(parent["id"]), "--org", org, ok=None), status=403)
