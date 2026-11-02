"""Real-gh pull request tests. All repositories, branches and metadata are case-local.

Git commands below are fixture setup/verification, never substitutes for gh coverage.
GraphQL errors are HTTP 200 with an errors envelope: assert their printed message,
not a fictitious HTTP 404. REST errors use expect_error instead.
"""
import base64
import re
import threading

from gh_sweep.harness import HOST, case, expect_error, expect_json, expect_ok


def failure(result, *messages):
    assert not result.timed_out and result.rc > 0, result
    text = result.stderr.lower()
    assert not re.search(r"panic:|panic at|segmentation fault|fatal error:", text), text
    assert any(message.lower() in text for message in messages), result.stderr
    return result


def missing(result):
    failure(result, "Could not resolve", "not found", "HTTP 404")


def denied(result):
    failure(result, "denied", "permission", "not authorized", "not found", "Could not resolve", "not accessible")


def other(ctx):
    return next(account for account in ctx.sweep.accounts if account != ctx.account)


def pull(ctx):
    return ctx.api("GET", f"repos/{ctx.repo}/pulls/{ctx.pr['number']}")


def comments(ctx):
    return ctx.api("GET", f"repos/{ctx.repo}/issues/{ctx.pr['number']}/comments")


def new_head(ctx):
    base = ctx.api("GET", f"repos/{ctx.repo}")["default_branch"]
    sha = ctx.api("GET", f"repos/{ctx.repo}/git/ref/heads/{base}")["object"]["sha"]
    head, path = ctx.name("head"), ctx.name("change") + ".txt"
    ctx.api("POST", f"repos/{ctx.repo}/git/refs", {"ref": "refs/heads/" + head, "sha": sha})
    ctx.api("PUT", f"repos/{ctx.repo}/contents/{path}",
            {"branch": head, "message": "Sweep first commit\n\nCommit body", "content": base64.b64encode(b"sweep\n").decode()})
    return base, head


def new_pull(ctx, **data):
    base, head = new_head(ctx)
    return ctx.api("POST", f"repos/{ctx.repo}/pulls",
                   {"title": ctx.name("pull"), "body": "sweep fixture", "base": base, "head": head, **data})


def git(ctx, directory, *args):
    return expect_ok(ctx.sweep.execute(ctx.account, ["git", *args], cwd=directory)).stdout.strip()


def clone(ctx, branch=None):
    directory = ctx.directory / ctx.name("clone")
    directory.mkdir()
    git(ctx, directory, "init")
    git(ctx, directory, "config", "user.name", ctx.account.name)
    git(ctx, directory, "config", "user.email", ctx.account.name + "@example.test")
    # Keep a GHES remote for gh's repository discovery; rewrite Git transport only
    # to the disposable loopback server. Credentials stay out of remote URLs.
    git(ctx, directory, "config", "http.extraHeader", "Authorization: Basic " +
        base64.b64encode((ctx.account.name + ":" + ctx.account.token).encode()).decode())
    git(ctx, directory, "config", "url." + ctx.sweep.base.rstrip("/") + "/.insteadOf", "https://" + HOST + "/")
    git(ctx, directory, "remote", "add", "origin", "https://" + HOST + "/" + ctx.repo + ".git")
    git(ctx, directory, "fetch", "origin")
    base = branch or ctx.api("GET", f"repos/{ctx.repo}")["default_branch"]
    git(ctx, directory, "checkout", "-B", base, "origin/" + base)
    return directory


def with_submodule(ctx):
    source = ctx.sweep.create_repo(ctx.account, ctx.name("submodule"))
    branch = source["default_branch"]
    sha = ctx.api("GET", f"repos/{source['full_name']}/git/ref/heads/{branch}")["object"]["sha"]
    authoring = clone(ctx, ctx.pr["head"]["ref"])
    git(ctx, authoring, "submodule", "add", ctx.sweep.base.rstrip("/") + "/" + source["full_name"] + ".git", "module")
    git(ctx, authoring, "commit", "-am", "Add sweep submodule")
    git(ctx, authoring, "push", "origin", "HEAD:refs/heads/" + ctx.pr["head"]["ref"])
    return sha


def label(ctx):
    name = ctx.name("label")
    ctx.api("POST", f"repos/{ctx.repo}/labels", {"name": name, "color": "abcdef"})
    return name


def milestone(ctx):
    return ctx.api("POST", f"repos/{ctx.repo}/milestones", {"title": ctx.name("milestone")})


def collaborator(ctx):
    account = other(ctx)
    ctx.api("PUT", f"repos/{ctx.repo}/collaborators/{account.name}", {"permission": "push"})
    return account


def project(ctx):
    title = ctx.name("project")
    base = "/users/" + ctx.account.name + "/projects"
    path = ctx.native("form", base, "--action", base, "--has", "title", "--set", "title=" + title,
                      "--status", "303", "--print", "location")
    assert path.startswith(base + "/"), path
    owner, repo = ctx.repo.split("/")
    ctx.native("form", path + "/settings", "--action", path, "--has", "action=link-repository",
               "--set", "owner=" + owner, "--set", "repo=" + repo, "--status", "303")
    return title


def png(ctx):
    path = ctx.directory / (ctx.name("image") + ".png")
    path.write_bytes(base64.b64decode("iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMCAO+aK1sAAAAASUVORK5CYII="))
    return str(path)


def created(ctx, result, title, body, draft=False):
    url = expect_ok(result).stdout.strip()
    assert f"/{ctx.repo}/pull/" in url, url
    number = int(url.rstrip("/").rsplit("/", 1)[1])
    row = ctx.api("GET", f"repos/{ctx.repo}/pulls/{number}")
    assert (row["title"], row["body"], row["draft"]) == (title, body, draft), row
    return row


@case(command="co", variant="branch force recurse and worktree", needs=("pr",))
def co_branch(ctx):
    directory = clone(ctx)
    name = ctx.name("local")
    expect_ok(ctx.gh("co", str(ctx.pr["number"]), "--repo", ctx.repo, "--branch", name,
                     "--force", "--recurse-submodules", cwd=directory))
    assert git(ctx, directory, "branch", "--show-current") == name
    assert git(ctx, directory, "rev-parse", "HEAD") == pull(ctx)["head"]["sha"]
    worktree = ctx.directory / ctx.name("worktree")
    expect_ok(ctx.gh("co", str(ctx.pr["number"]), "--repo", ctx.repo, "--detach", "--worktree", str(worktree), cwd=directory))
    assert git(ctx, worktree, "branch", "--show-current") == ""
    assert git(ctx, worktree, "rev-parse", "HEAD") == pull(ctx)["head"]["sha"]


@case(command="pr checkout", variant="branch force recurse and detached worktree", needs=("pr",))
def checkout_branch(ctx):
    directory = clone(ctx)
    name = ctx.name("local")
    expect_ok(ctx.gh("pr", "checkout", str(ctx.pr["number"]), "--repo", ctx.repo, "--branch", name,
                     "--force", "--recurse-submodules", cwd=directory))
    assert git(ctx, directory, "branch", "--show-current") == name
    assert git(ctx, directory, "rev-parse", "HEAD") == pull(ctx)["head"]["sha"]
    worktree = ctx.directory / ctx.name("worktree")
    expect_ok(ctx.gh("pr", "checkout", str(ctx.pr["number"]), "--repo", ctx.repo,
                     "--detach", "--worktree", str(worktree), cwd=directory))
    assert git(ctx, worktree, "branch", "--show-current") == ""
    assert git(ctx, worktree, "rev-parse", "HEAD") == pull(ctx)["head"]["sha"]


@case(command="co", variant="default head and detach", needs=("pr",))
def co_default(ctx):
    directory = clone(ctx)
    expect_ok(ctx.gh("co", str(ctx.pr["number"]), "--repo", ctx.repo, cwd=directory))
    assert git(ctx, directory, "branch", "--show-current") == ctx.pr["head"]["ref"]
    expect_ok(ctx.gh("co", str(ctx.pr["number"]), "--repo", ctx.repo, "--detach", cwd=directory))
    assert git(ctx, directory, "branch", "--show-current") == ""


@case(command="pr checkout", variant="default head and detach", needs=("pr",))
def checkout_default(ctx):
    directory = clone(ctx)
    expect_ok(ctx.gh("pr", "checkout", str(ctx.pr["number"]), "--repo", ctx.repo, cwd=directory))
    assert git(ctx, directory, "branch", "--show-current") == ctx.pr["head"]["ref"]
    expect_ok(ctx.gh("pr", "checkout", str(ctx.pr["number"]), "--repo", ctx.repo, "--detach", cwd=directory))
    assert git(ctx, directory, "branch", "--show-current") == ""


@case(command="pr checks", variant="success JSON jq template and completed watch", needs=("pr",))
def checks_success(ctx):
    name = ctx.name("check")
    ctx.api("POST", f"repos/{ctx.repo}/statuses/{ctx.pr['head']['sha']}", {"state": "success", "context": name})
    number = str(ctx.pr["number"])
    rows = expect_json(ctx.gh("pr", "checks", number, "--repo", ctx.repo, "--json", "name,state,bucket"),
                       [{"name": str, "state": str, "bucket": str}])
    assert {"name": name, "state": "SUCCESS", "bucket": "pass"} in rows, rows
    result = expect_ok(ctx.gh("pr", "checks", number, "--repo", ctx.repo, "--json", "name", "--jq", ".[0].name"))
    assert result.stdout.strip() == name
    result = expect_ok(ctx.gh("pr", "checks", number, "--repo", ctx.repo, "--json", "name", "--template", "{{range .}}{{.name}}{{end}}"))
    assert result.stdout == name
    result = expect_ok(ctx.gh("pr", "checks", number, "--repo", ctx.repo, "--watch", "--interval", "1", "--fail-fast", timeout=15))
    assert name in result.stdout + result.stderr


@case(command="pr checks", variant="pending failed empty and required", needs=("pr",))
def checks_states(ctx):
    number, name = str(ctx.pr["number"]), ctx.name("check")
    failure(ctx.gh("pr", "checks", number, "--repo", ctx.repo, ok=None), "no checks reported")
    ctx.api("POST", f"repos/{ctx.repo}/statuses/{ctx.pr['head']['sha']}", {"state": "pending", "context": name})
    result = ctx.gh("pr", "checks", number, "--repo", ctx.repo, "--json", "name,bucket", ok=(8,))
    assert result.rc == 8 and {"name": name, "bucket": "pending"} in result.json(), result
    ctx.api("POST", f"repos/{ctx.repo}/statuses/{ctx.pr['head']['sha']}", {"state": "failure", "context": name})
    result = ctx.gh("pr", "checks", number, "--repo", ctx.repo, "--watch", "--fail-fast", "--interval", "1", ok=(1,), timeout=15)
    assert result.rc == 1 and name in result.stdout + result.stderr, result
    failure(ctx.gh("pr", "checks", number, "--repo", ctx.repo, "--required", ok=None), "no required checks")


@case(command="pr close", variant="close comment delete branch and idempotency", needs=("pr",))
def close_pull(ctx):
    number, body = str(ctx.pr["number"]), ctx.name("closing-comment")
    expect_ok(ctx.gh("pr", "close", number, "--repo", ctx.repo, "--comment", body, "--delete-branch"))
    assert pull(ctx)["state"] == "closed"
    assert body in [row["body"] for row in comments(ctx)]
    refs = ctx.api("GET", f"repos/{ctx.repo}/git/matching-refs/heads/{ctx.pr['head']['ref']}")
    assert refs == [], refs
    result = expect_ok(ctx.gh("pr", "close", number, "--repo", ctx.repo))
    assert "already closed" in (result.stdout + result.stderr).lower()


@case(command="pr comment", variant="body stdin file edit create delete", needs=("pr",))
def comment_lifecycle(ctx):
    number, body = str(ctx.pr["number"]), ctx.name("comment")
    expect_ok(ctx.gh("pr", "comment", number, "--repo", ctx.repo, "--edit-last", "--create-if-none", "--body", body))
    assert [row["body"] for row in comments(ctx)] == [body]
    path = ctx.directory / ctx.name("body")
    path.write_text("replacement\n")
    expect_ok(ctx.gh("pr", "comment", number, "--repo", ctx.repo, "--edit-last", "--body-file", str(path)))
    assert [row["body"].strip() for row in comments(ctx)] == ["replacement"]
    expect_ok(ctx.gh("pr", "comment", number, "--repo", ctx.repo, "--body-file", "-", input=body))
    assert len(comments(ctx)) == 2
    expect_ok(ctx.gh("pr", "comment", number, "--repo", ctx.repo, "--delete-last", "--yes"))
    assert [row["body"].strip() for row in comments(ctx)] == ["replacement"]


@case(command="pr comment", variant="edit absent invalid combinations and attachment file", needs=("pr",))
def comment_invalid(ctx):
    number = str(ctx.pr["number"])
    failure(ctx.gh("pr", "comment", number, "--repo", ctx.repo, "--edit-last", "--body", "edit", ok=None), "no comments found", "no comment found")
    failure(ctx.gh("pr", "comment", number, "--repo", ctx.repo, "--create-if-none", "--body", "x", ok=None), "--edit-last")
    failure(ctx.gh("pr", "comment", number, "--repo", ctx.repo, "--delete-last", "--yes", ok=None), "no comments found", "no comment found")
    path = ctx.directory / (ctx.name("absent") + ".png")
    failure(ctx.gh("pr", "comment", number, "--repo", ctx.repo, "--body", "attachment", "--attach", str(path), ok=None), "no such file", "does not exist")
    assert comments(ctx) == []


@case(command="pr create", variant="explicit draft metadata and no maintainer edit", needs=("repo",))
def create_metadata(ctx):
    base, head = new_head(ctx)
    name, mark, account = label(ctx), milestone(ctx), collaborator(ctx)
    title, body = ctx.name("title"), ctx.name("body")
    result = ctx.gh("pr", "create", "--repo", ctx.repo, "--head", head, "--base", base, "--title", title,
                    "--body", body, "--draft", "--no-maintainer-edit", "--assignee", "@me", "--label", name,
                    "--milestone", mark["title"], "--reviewer", account.name)
    row = created(ctx, result, title, body, True)
    assert row["maintainer_can_modify"] is False
    assert [item["name"] for item in row["labels"]] == [name]
    assert row["milestone"]["number"] == mark["number"]
    assert ctx.account.name in [item["login"] for item in row["assignees"]]
    assert account.name in [item["login"] for item in row["requested_reviewers"]]


@case(command="pr create", variant="body file and stdin", needs=("repo",))
def create_body_files(ctx):
    base, head = new_head(ctx)
    title, body = ctx.name("title"), ctx.name("body")
    path = ctx.directory / ctx.name("body")
    path.write_text(body)
    created(ctx, ctx.gh("pr", "create", "--repo", ctx.repo, "--head", head, "--base", base,
                        "--title", title, "--body-file", str(path)), title, body)
    base, head = new_head(ctx)
    title = ctx.name("title")
    created(ctx, ctx.gh("pr", "create", "--repo", ctx.repo, "--head", head, "--title", title,
                        "--body-file", "-", input=body), title, body)


@case(command="pr create", variant="fill first verbose and explicit override", needs=("repo",))
def create_fill(ctx):
    base, head = new_head(ctx)
    directory = clone(ctx, head)
    title = ctx.name("override")
    created(ctx, ctx.gh("pr", "create", "--repo", ctx.repo, "--base", base, "--head", head, "--fill",
                        "--title", title, "--body", "override body", cwd=directory), title, "override body")
    base, head = new_head(ctx)
    directory = clone(ctx, head)
    row = created(ctx, ctx.gh("pr", "create", "--repo", ctx.repo, "--base", base, "--head", head,
                             "--fill-first", cwd=directory), "Sweep first commit", "Commit body")
    assert row["head"]["ref"] == head
    base, head = new_head(ctx)
    directory = clone(ctx, head)
    result = expect_ok(ctx.gh("pr", "create", "--repo", ctx.repo, "--base", base, "--head", head, "--fill-verbose", cwd=directory))
    number = int(result.stdout.strip().rstrip("/").rsplit("/", 1)[1])
    row = ctx.api("GET", f"repos/{ctx.repo}/pulls/{number}")
    assert "Sweep first commit" in row["title"] and "Commit body" in row["body"], row


@case(command="pr create", variant="dry run does not create", needs=("repo",))
def create_dry_run(ctx):
    base, head = new_head(ctx)
    title, body = ctx.name("dry-title"), ctx.name("dry-body")
    result = expect_ok(ctx.gh("pr", "create", "--repo", ctx.repo, "--base", base, "--head", head,
                              "--title", title, "--body", body, "--dry-run"))
    assert title in result.stdout and body in result.stdout, result
    assert ctx.api("GET", f"repos/{ctx.repo}/pulls?state=all") == []


@case(command="pr create", variant="invalid head duplicate recover attachment and project", needs=("pr",))
def create_invalid(ctx):
    title = ctx.name("title")
    failure(ctx.gh("pr", "create", "--repo", ctx.repo, "--head", ctx.name("missing-head"), "--title", title,
                   "--body", "x", ok=None), "not found", "Could not resolve", "No commits", "Head ref")
    failure(ctx.gh("pr", "create", "--repo", ctx.repo, "--head", ctx.pr["head"]["ref"], "--title", title,
                   "--body", "x", ok=None), "already exists", "already open")
    directory = clone(ctx, ctx.pr["head"]["ref"])
    failure(ctx.gh("pr", "create", "--repo", ctx.repo, "--recover", str(ctx.directory / ctx.name("missing")), cwd=directory, ok=None), "no such file", "failed to read", "could not read", "could not open")
    base, head = new_head(ctx)
    failure(ctx.gh("pr", "create", "--repo", ctx.repo, "--head", head, "--title", title, "--body", "x",
                   "--attach", str(ctx.directory / (ctx.name("missing") + ".png")), ok=None), "no such file", "does not exist")
    failure(ctx.gh("pr", "create", "--repo", ctx.repo, "--head", head, "--title", title, "--body", "x",
                   "--project", ctx.name("missing-project"), ok=None), "not found", "could not find", "unable to find")


@case(command="pr create", variant="template editor fails cleanly with disabled editor", needs=("repo",))
def create_template(ctx):
    base, head = new_head(ctx)
    path = ctx.directory / (ctx.name("template") + ".md")
    path.write_text("Sweep template body\n")
    directory = clone(ctx, head)
    failure(ctx.gh("pr", "create", "--repo", ctx.repo, "--head", head, "--base", base,
                   "--template", str(path), "--editor", cwd=directory, ok=None), "editor", "exit status 1")
    assert ctx.api("GET", f"repos/{ctx.repo}/pulls?state=all") == []


@case(command="pr diff", variant="diff patch names exclusion colors and escape handling", needs=("pr",))
def diff_formats(ctx):
    number = str(ctx.pr["number"])
    files = ctx.api("GET", f"repos/{ctx.repo}/pulls/{number}/files")
    names = [item["filename"] for item in files]
    assert names
    result = expect_ok(ctx.gh("pr", "diff", number, "--repo", ctx.repo, "--color", "never"))
    assert "diff --git" in result.stdout and "+sweep" in result.stdout and "\x1b[" not in result.stdout
    result = expect_ok(ctx.gh("pr", "diff", number, "--repo", ctx.repo, "--patch", "--allow-escape-sequences"))
    assert "Subject: [PATCH]" in result.stdout and "+sweep" in result.stdout
    result = expect_ok(ctx.gh("pr", "diff", number, "--repo", ctx.repo, "--name-only"))
    assert result.stdout.splitlines() == names
    result = expect_ok(ctx.gh("pr", "diff", number, "--repo", ctx.repo, "--name-only", "--exclude", "*.txt"))
    assert result.stdout.strip() == ""
    result = expect_ok(ctx.gh("pr", "diff", number, "--repo", ctx.repo, "--color", "always"))
    assert "\x1b[" in result.stdout
    result = expect_ok(ctx.gh("pr", "diff", number, "--repo", ctx.repo, "--color", "auto", "--exclude", ctx.name("unmatched")))
    assert "+sweep" in result.stdout


@case(command="pr edit", variant="title body base add and remove metadata", needs=("pr",))
def edit_metadata(ctx):
    number, title, body = str(ctx.pr["number"]), ctx.name("edited"), ctx.name("body")
    name, mark, account = label(ctx), milestone(ctx), collaborator(ctx)
    base = ctx.name("new-base")
    ctx.api("POST", f"repos/{ctx.repo}/git/refs", {"ref": "refs/heads/" + base, "sha": ctx.pr["base"]["sha"]})
    expect_ok(ctx.gh("pr", "edit", number, "--repo", ctx.repo, "--title", title, "--body", body,
                     "--base", base, "--add-assignee", "@me", "--add-label", name,
                     "--milestone", mark["title"], "--add-reviewer", account.name))
    row = pull(ctx)
    assert (row["title"], row["body"], row["base"]["ref"]) == (title, body, base)
    assert [item["name"] for item in row["labels"]] == [name]
    assert row["milestone"]["number"] == mark["number"]
    assert ctx.account.name in [item["login"] for item in row["assignees"]]
    assert account.name in [item["login"] for item in row["requested_reviewers"]]
    expect_ok(ctx.gh("pr", "edit", number, "--repo", ctx.repo, "--remove-assignee", "@me", "--remove-label", name,
                     "--remove-milestone", "--remove-reviewer", account.name, "--body-file", "-", input="stdin replacement"))
    row = pull(ctx)
    assert row["body"] == "stdin replacement"
    assert row["assignees"] == [] and row["labels"] == [] and row["milestone"] is None and row["requested_reviewers"] == []
    path = ctx.directory / ctx.name("body")
    path.write_text(body)
    expect_ok(ctx.gh("pr", "edit", number, "--repo", ctx.repo, "--body-file", str(path)))
    assert pull(ctx)["body"] == body


@case(command="pr edit", variant="missing metadata attachment and project", needs=("pr",))
def edit_invalid(ctx):
    number = str(ctx.pr["number"])
    failure(ctx.gh("pr", "edit", number, "--repo", ctx.repo, "--add-label", ctx.name("missing-label"), ok=None), "not found", "could not find")
    failure(ctx.gh("pr", "edit", number, "--repo", ctx.repo, "--add-project", ctx.name("missing-project"), ok=None), "not found", "could not find", "unable to find")
    failure(ctx.gh("pr", "edit", number, "--repo", ctx.repo, "--remove-project", ctx.name("missing-project"), ok=None), "not found", "could not find", "unable to find")
    failure(ctx.gh("pr", "edit", number, "--repo", ctx.repo, "--attach", str(ctx.directory / (ctx.name("absent") + ".png")), ok=None), "no such file", "does not exist")
    assert pull(ctx)["body"] == "sweep fixture"


@case(command="pr list", variant="default JSON formatting and individual filters", needs=("pr",))
def list_filters(ctx):
    number, title = ctx.pr["number"], ctx.pr["title"]
    rows = expect_json(ctx.gh("pr", "list", "--repo", ctx.repo, "--json", "number,title,state"), [{"number": int, "title": str, "state": "OPEN"}])
    assert {"number": number, "title": title, "state": "OPEN"} in rows
    result = expect_ok(ctx.gh("pr", "list", "--repo", ctx.repo))
    assert title in result.stdout
    assert expect_ok(ctx.gh("pr", "list", "--repo", ctx.repo, "--json", "number", "--jq", ".[0].number")).stdout.strip() == str(number)
    assert expect_ok(ctx.gh("pr", "list", "--repo", ctx.repo, "--json", "title", "--template", "{{range .}}{{.title}}{{end}}")).stdout == title
    assert expect_json(ctx.gh("pr", "list", "--repo", ctx.repo, "--author", "@me", "--base", ctx.pr["base"]["ref"],
                              "--head", ctx.pr["head"]["ref"], "--limit", "1", "--json", "number"), [{"number": int}]) == [{"number": number}]
    expect_json(ctx.gh("pr", "list", "--repo", ctx.repo, "--draft", "--json", "number"), [])
    expect_json(ctx.gh("pr", "list", "--repo", ctx.repo, "--assignee", other(ctx).name, "--json", "number"), [])
    expect_json(ctx.gh("pr", "list", "--repo", ctx.repo, "--app", "github-actions", "--json", "number"), [])
    name = label(ctx)
    ctx.api("POST", f"repos/{ctx.repo}/issues/{number}/labels", {"labels": [name]})
    rows = expect_json(ctx.gh("pr", "list", "--repo", ctx.repo, "--label", name, "--json", "number"), [{"number": int}])
    assert rows == [{"number": number}], rows
    rows = expect_json(ctx.gh("pr", "list", "--repo", ctx.repo, "--search", title, "--state", "all", "--json", "number"), [{"number": int}])
    assert rows == [{"number": number}], rows


@case(command="pr list", variant="draft closed merged all and empty filters", needs=("pr",))
def list_states(ctx):
    draft = new_pull(ctx, draft=True)
    rows = expect_json(ctx.gh("pr", "list", "--repo", ctx.repo, "--draft", "--json", "number,isDraft"), [{"number": int, "isDraft": True}])
    assert rows == [{"number": draft["number"], "isDraft": True}]
    ctx.api("PATCH", f"repos/{ctx.repo}/pulls/{ctx.pr['number']}", {"state": "closed"})
    rows = expect_json(ctx.gh("pr", "list", "--repo", ctx.repo, "--state", "closed", "--json", "number"), [{"number": int}])
    assert rows == [{"number": ctx.pr["number"]}]
    expect_json(ctx.gh("pr", "list", "--repo", ctx.repo, "--state", "merged", "--json", "number"), [])
    rows = expect_json(ctx.gh("pr", "list", "--repo", ctx.repo, "--state", "all", "--json", "number"), [{"number": int}])
    assert {row["number"] for row in rows} == {ctx.pr["number"], draft["number"]}
    expect_json(ctx.gh("pr", "list", "--repo", ctx.repo, "--head", ctx.name("absent"), "--json", "number"), [])
    failure(ctx.gh("pr", "list", "--repo", ctx.repo, "--state", "invalid", ok=None), "invalid", "must be one of")
    failure(ctx.gh("pr", "list", "--repo", ctx.repo, "--limit", "0", ok=None), "limit")


@case(command="pr lock", variant="all reasons default and invalid", needs=("pr",))
def lock_reasons(ctx):
    number = str(ctx.pr["number"])
    # A fresh PR for each reason keeps the command, not an unlock helper, under test.
    expect_ok(ctx.gh("pr", "lock", number, "--repo", ctx.repo))
    assert pull(ctx)["locked"] is True
    for reason in ("off_topic", "resolved", "spam", "too_heated"):
        row = new_pull(ctx)
        expect_ok(ctx.gh("pr", "lock", str(row["number"]), "--repo", ctx.repo, "--reason", reason))
        saved = ctx.api("GET", f"repos/{ctx.repo}/pulls/{row['number']}")
        assert saved["locked"] is True and saved["active_lock_reason"] == reason, saved
    failure(ctx.gh("pr", "lock", number, "--repo", ctx.repo, "--reason", "invalid", ok=None), "invalid", "reason")


@case(command="pr merge", variant="merge body subject author head guard delete", needs=("pr",))
def merge_commit(ctx):
    number, title, body = str(ctx.pr["number"]), ctx.name("merge-title"), ctx.name("merge-body")
    email = ctx.account.name + "@example.test"
    expect_ok(ctx.gh("pr", "merge", number, "--repo", ctx.repo, "--merge", "--subject", title, "--body", body,
                     "--author-email", email, "--match-head-commit", ctx.pr["head"]["sha"], "--delete-branch"))
    row = pull(ctx)
    assert row["merged"] is True and row["state"] == "closed"
    commit = ctx.api("GET", f"repos/{ctx.repo}/commits/{row['merge_commit_sha']}")
    assert title in commit["commit"]["message"] and body in commit["commit"]["message"]
    assert commit["commit"]["author"]["email"] == email
    assert ctx.api("GET", f"repos/{ctx.repo}/git/matching-refs/heads/{ctx.pr['head']['ref']}") == []


@case(command="pr merge", variant="squash admin body-file", needs=("pr",))
def merge_squash(ctx):
    path, title, body = ctx.directory / ctx.name("body"), ctx.name("subject"), ctx.name("body")
    path.write_text(body)
    expect_ok(ctx.gh("pr", "merge", str(ctx.pr["number"]), "--repo", ctx.repo, "--squash", "--admin",
                     "--subject", title, "--body-file", str(path)))
    row = pull(ctx)
    assert row["merged"] is True
    commit = ctx.api("GET", f"repos/{ctx.repo}/commits/{row['merge_commit_sha']}")
    assert len(commit["parents"]) == 1
    assert title in commit["commit"]["message"] and body in commit["commit"]["message"]


@case(command="pr merge", variant="rebase", needs=("pr",))
def merge_rebase(ctx):
    expect_ok(ctx.gh("pr", "merge", str(ctx.pr["number"]), "--repo", ctx.repo, "--rebase"))
    row = pull(ctx)
    assert row["merged"] is True
    commit = ctx.api("GET", f"repos/{ctx.repo}/commits/{row['merge_commit_sha']}")
    assert len(commit["parents"]) == 1


@case(command="pr merge", variant="auto and disable-auto", needs=("pr",))
def merge_auto(ctx):
    # A pending required context blocks immediate merge and makes --auto observable.
    name = ctx.name("required-check")
    ctx.api("PUT", f"repos/{ctx.repo}/branches/{ctx.pr['base']['ref']}/protection",
            {"required_status_checks": {"strict": True, "contexts": [name]}, "enforce_admins": True,
             "required_pull_request_reviews": None, "restrictions": None})
    ctx.api("POST", f"repos/{ctx.repo}/statuses/{ctx.pr['head']['sha']}", {"state": "pending", "context": name})
    expect_ok(ctx.gh("pr", "merge", str(ctx.pr["number"]), "--repo", ctx.repo, "--auto", "--merge"))
    assert pull(ctx)["auto_merge"] is not None and pull(ctx)["merged"] is False
    expect_ok(ctx.gh("pr", "merge", str(ctx.pr["number"]), "--repo", ctx.repo, "--disable-auto"))
    assert pull(ctx)["auto_merge"] is None and pull(ctx)["merged"] is False


@case(command="pr merge", variant="mismatched head draft and conflicting strategies", needs=("pr",))
def merge_invalid(ctx):
    failure(ctx.gh("pr", "merge", str(ctx.pr["number"]), "--repo", ctx.repo, "--merge", "--match-head-commit", "0" * 40, ok=None), "head", "modified")
    assert pull(ctx)["merged"] is False
    failure(ctx.gh("pr", "merge", str(ctx.pr["number"]), "--repo", ctx.repo, "--merge", "--squash", ok=None), "only one", "cannot use", "not allowed")
    row = new_pull(ctx, draft=True)
    failure(ctx.gh("pr", "merge", str(row["number"]), "--repo", ctx.repo, "--merge", ok=None), "draft", "not mergeable")


@case(command="pr ready", variant="draft round trip and already ready", needs=("pr",))
def ready_round_trip(ctx):
    number = str(ctx.pr["number"])
    expect_ok(ctx.gh("pr", "ready", number, "--repo", ctx.repo, "--undo"))
    assert pull(ctx)["draft"] is True
    expect_ok(ctx.gh("pr", "ready", number, "--repo", ctx.repo))
    assert pull(ctx)["draft"] is False
    result = expect_ok(ctx.gh("pr", "ready", number, "--repo", ctx.repo))
    assert "already" in (result.stdout + result.stderr).lower()


@case(command="pr reopen", variant="closed comment and already open", needs=("pr",))
def reopen_pull(ctx):
    ctx.api("PATCH", f"repos/{ctx.repo}/pulls/{ctx.pr['number']}", {"state": "closed"})
    body = ctx.name("reopen-comment")
    expect_ok(ctx.gh("pr", "reopen", str(ctx.pr["number"]), "--repo", ctx.repo, "--comment", body))
    assert pull(ctx)["state"] == "open"
    assert body in [row["body"] for row in comments(ctx)]
    result = expect_ok(ctx.gh("pr", "reopen", str(ctx.pr["number"]), "--repo", ctx.repo))
    assert "already open" in (result.stdout + result.stderr).lower()


@case(command="pr revert", variant="merged title body draft", needs=("pr",))
def revert_merged(ctx):
    ctx.api("PUT", f"repos/{ctx.repo}/pulls/{ctx.pr['number']}/merge", {"merge_method": "merge"})
    title, body = ctx.name("revert"), ctx.name("body")
    result = expect_ok(ctx.gh("pr", "revert", str(ctx.pr["number"]), "--repo", ctx.repo,
                              "--title", title, "--body", body, "--draft"))
    rows = ctx.api("GET", f"repos/{ctx.repo}/pulls?state=open")
    reverted = next(row for row in rows if row["title"] == title)
    assert reverted["draft"] is True and reverted["body"] == body
    assert str(reverted["number"]) in result.stdout + result.stderr
    files = ctx.api("GET", f"repos/{ctx.repo}/pulls/{reverted['number']}/files")
    assert files and all(row["status"] == "removed" for row in files), files


@case(command="pr revert", variant="body-file and unmerged rejection", needs=("pr",))
def revert_file(ctx):
    failure(ctx.gh("pr", "revert", str(ctx.pr["number"]), "--repo", ctx.repo, ok=None), "not merged", "must be merged", "hasn't been merged")
    ctx.api("PUT", f"repos/{ctx.repo}/pulls/{ctx.pr['number']}/merge", {"merge_method": "merge"})
    path, title, body = ctx.directory / ctx.name("body"), ctx.name("revert"), ctx.name("body")
    path.write_text(body)
    expect_ok(ctx.gh("pr", "revert", str(ctx.pr["number"]), "--repo", ctx.repo, "--title", title, "--body-file", str(path)))
    rows = ctx.api("GET", f"repos/{ctx.repo}/pulls?state=open")
    reverted = next(row for row in rows if row["title"] == title)
    assert reverted["body"] == body and reverted["draft"] is False


@case(command="pr review", variant="second account approve comment and request changes", needs=("pr",))
def review_events(ctx):
    account, number = other(ctx), str(ctx.pr["number"])
    # Public repository: a non-collaborator may review but may not merge/edit.
    expect_ok(ctx.sweep.gh(account, "pr", "review", number, "--repo", ctx.repo, "--approve", "--body", "approved"))
    path = ctx.directory / ctx.name("review")
    path.write_text("please revise")
    expect_ok(ctx.sweep.gh(account, "pr", "review", number, "--repo", ctx.repo, "--request-changes", "--body-file", str(path)))
    expect_ok(ctx.sweep.gh(account, "pr", "review", number, "--repo", ctx.repo, "--comment", "--body-file", "-", input="review comment"))
    rows = ctx.api("GET", f"repos/{ctx.repo}/pulls/{number}/reviews")
    assert [(row["state"], row["body"]) for row in rows] == [
        ("APPROVED", "approved"), ("CHANGES_REQUESTED", "please revise"), ("COMMENTED", "review comment")], rows
    assert all(row["user"]["login"] == account.name for row in rows)


@case(command="pr review", variant="self approval and changes denied", needs=("pr",))
def review_self(ctx):
    failure(ctx.gh("pr", "review", str(ctx.pr["number"]), "--repo", ctx.repo, "--approve", ok=None), "own pull request")
    failure(ctx.gh("pr", "review", str(ctx.pr["number"]), "--repo", ctx.repo, "--request-changes", "--body", "no", ok=None), "own pull request")
    expect_ok(ctx.gh("pr", "review", str(ctx.pr["number"]), "--repo", ctx.repo, "--comment", "--body", "self comment"))
    rows = ctx.api("GET", f"repos/{ctx.repo}/pulls/{ctx.pr['number']}/reviews")
    assert len(rows) == 1 and rows[0]["body"] == "self comment"


@case(command="pr status", variant="plain conflicts JSON jq template", needs=("pr",))
def status_formats(ctx):
    result = expect_ok(ctx.gh("pr", "status", "--repo", ctx.repo, "--conflict-status"))
    assert ctx.pr["title"] in result.stdout and ctx.repo in result.stdout
    value = expect_json(ctx.gh("pr", "status", "--repo", ctx.repo, "--json", "number,title"),
                        {"createdBy": [{"number": int, "title": str}], "needsReview": list})
    assert any(row["number"] == ctx.pr["number"] for row in value["createdBy"])
    result = expect_ok(ctx.gh("pr", "status", "--repo", ctx.repo, "--json", "number", "--jq", ".createdBy[].number"))
    assert str(ctx.pr["number"]) in result.stdout.splitlines()
    result = expect_ok(ctx.gh("pr", "status", "--repo", ctx.repo, "--json", "title", "--template", "{{range .createdBy}}{{.title}}{{end}}"))
    assert ctx.pr["title"] in result.stdout


@case(command="pr unlock", variant="locked and already unlocked", needs=("pr",))
def unlock_pull(ctx):
    number = str(ctx.pr["number"])
    ctx.api("PUT", f"repos/{ctx.repo}/issues/{number}/lock", {"lock_reason": "resolved"})
    expect_ok(ctx.gh("pr", "unlock", number, "--repo", ctx.repo))
    assert pull(ctx)["locked"] is False
    expect_ok(ctx.gh("pr", "unlock", number, "--repo", ctx.repo))
    assert pull(ctx)["locked"] is False


@case(command="pr update-branch", variant="merge newer base and already current", needs=("pr",))
def update_branch(ctx):
    before = pull(ctx)["head"]["sha"]
    base = ctx.pr["base"]["ref"]
    ctx.api("PUT", f"repos/{ctx.repo}/contents/{ctx.name('base-change')}.txt",
            {"branch": base, "message": "advance base", "content": base64.b64encode(b"base\n").decode()})
    expect_ok(ctx.gh("pr", "update-branch", str(ctx.pr["number"]), "--repo", ctx.repo))
    row = pull(ctx)
    assert row["head"]["sha"] != before and row["state"] == "open"
    commit = ctx.api("GET", f"repos/{ctx.repo}/commits/{row['head']['sha']}")
    assert len(commit["parents"]) == 2 and before in [parent["sha"] for parent in commit["parents"]]
    result = expect_ok(ctx.gh("pr", "update-branch", str(ctx.pr["number"]), "--repo", ctx.repo))
    assert "up to date" in (result.stdout + result.stderr).lower()


@case(command="pr update-branch", variant="rebase onto newer base (GHES parity)", needs=("pr",))
def update_rebase(ctx):
    advanced = ctx.api("PUT", f"repos/{ctx.repo}/contents/{ctx.name('base-change')}.txt",
                       {"branch": ctx.pr["base"]["ref"], "message": "advance base", "content": base64.b64encode(b"base\n").decode()})
    before = pull(ctx)["head"]["sha"]
    # GHES supports this. Do not bless Codeyard's current GraphQL rejection:
    # this case should expose that implementation gap when run live.
    expect_ok(ctx.gh("pr", "update-branch", str(ctx.pr["number"]), "--repo", ctx.repo, "--rebase"))
    row = pull(ctx)
    assert row["head"]["sha"] != before and row["state"] == "open"
    commit = ctx.api("GET", f"repos/{ctx.repo}/commits/{row['head']['sha']}")
    assert [parent["sha"] for parent in commit["parents"]] == [advanced["commit"]["sha"]]


@case(command="pr view", variant="plain comments JSON jq template branch and URL", needs=("pr",))
def view_formats(ctx):
    number, body = str(ctx.pr["number"]), ctx.name("comment")
    ctx.api("POST", f"repos/{ctx.repo}/issues/{number}/comments", {"body": body})
    result = expect_ok(ctx.gh("pr", "view", number, "--repo", ctx.repo))
    assert ctx.pr["title"] in result.stdout and "sweep fixture" in result.stdout
    result = expect_ok(ctx.gh("pr", "view", number, "--repo", ctx.repo, "--comments"))
    assert body in result.stdout
    expect_json(ctx.gh("pr", "view", number, "--repo", ctx.repo, "--json", "number,title,state,comments"),
                {"number": ctx.pr["number"], "title": ctx.pr["title"], "state": "OPEN", "comments": [{"body": body}]})
    result = expect_ok(ctx.gh("pr", "view", ctx.pr["head"]["ref"], "--repo", ctx.repo, "--json", "title", "--jq", ".title"))
    assert result.stdout.strip() == ctx.pr["title"]
    result = expect_ok(ctx.gh("pr", "view", "https://" + HOST + "/" + ctx.repo + "/pull/" + number,
                              "--json", "number,title", "--template", "{{.number}}:{{.title}}"))
    assert result.stdout == number + ":" + ctx.pr["title"]


@case(command="co", variant="missing repository private repository and incompatible flags", needs=("pr",))
def co_errors(ctx):
    directory = clone(ctx)
    missing(ctx.gh("co", "1", "--repo", ctx.account.name + "/" + ctx.name("missing"), cwd=directory, ok=None))
    failure(ctx.gh("co", str(ctx.pr["number"]), "--repo", ctx.repo, "--branch", ctx.name("branch"), "--detach", cwd=directory, ok=None), "cannot", "not allowed", "mutually exclusive")
    ctx.api("PATCH", f"repos/{ctx.repo}", {"private": True})
    missing(ctx.sweep.gh(other(ctx), "co", str(ctx.pr["number"]), "--repo", ctx.repo, cwd=directory, ok=None))


@case(command="pr checkout", variant="missing repository private repository and incompatible flags", needs=("pr",))
def checkout_errors(ctx):
    directory = clone(ctx)
    missing(ctx.gh("pr", "checkout", "1", "--repo", ctx.account.name + "/" + ctx.name("missing"), cwd=directory, ok=None))
    failure(ctx.gh("pr", "checkout", str(ctx.pr["number"]), "--repo", ctx.repo, "--branch", ctx.name("branch"), "--detach", cwd=directory, ok=None), "cannot", "not allowed", "mutually exclusive")
    ctx.api("PATCH", f"repos/{ctx.repo}", {"private": True})
    missing(ctx.sweep.gh(other(ctx), "pr", "checkout", str(ctx.pr["number"]), "--repo", ctx.repo, cwd=directory, ok=None))


@case(command="pr checks", variant="missing repo pull and inaccessible private repo", needs=("pr",))
def checks_errors(ctx):
    missing(ctx.gh("pr", "checks", "1", "--repo", ctx.account.name + "/" + ctx.name("missing"), ok=None))
    missing(ctx.gh("pr", "checks", "999999999", "--repo", ctx.repo, ok=None))
    ctx.api("PATCH", f"repos/{ctx.repo}", {"private": True})
    missing(ctx.sweep.gh(other(ctx), "pr", "checks", str(ctx.pr["number"]), "--repo", ctx.repo, ok=None))


@case(command="pr close", variant="missing repo pull and non-owner denied", needs=("pr",))
def close_errors(ctx):
    missing(ctx.gh("pr", "close", "1", "--repo", ctx.account.name + "/" + ctx.name("missing"), ok=None))
    missing(ctx.gh("pr", "close", "999999999", "--repo", ctx.repo, ok=None))
    denied(ctx.sweep.gh(other(ctx), "pr", "close", str(ctx.pr["number"]), "--repo", ctx.repo, ok=None))
    assert pull(ctx)["state"] == "open"


@case(command="pr comment", variant="missing repo private repo and authorship", needs=("pr",))
def comment_errors(ctx):
    missing(ctx.gh("pr", "comment", "1", "--repo", ctx.account.name + "/" + ctx.name("missing"), "--body", "x", ok=None))
    account, body = other(ctx), ctx.name("other-comment")
    expect_ok(ctx.sweep.gh(account, "pr", "comment", str(ctx.pr["number"]), "--repo", ctx.repo, "--body", body))
    failure(ctx.gh("pr", "comment", str(ctx.pr["number"]), "--repo", ctx.repo, "--edit-last", "--body", "not mine", ok=None), "no comments found", "no comment found")
    assert [row["body"] for row in comments(ctx)] == [body]
    ctx.api("PATCH", f"repos/{ctx.repo}", {"private": True})
    missing(ctx.sweep.gh(account, "pr", "comment", str(ctx.pr["number"]), "--repo", ctx.repo, "--body", "x", ok=None))


@case(command="pr create", variant="missing repo private repo and invalid metadata", needs=("repo",))
def create_errors(ctx):
    base, head = new_head(ctx)
    title = ctx.name("title")
    missing(ctx.gh("pr", "create", "--repo", ctx.account.name + "/" + ctx.name("missing"), "--head", head, "--title", title, "--body", "x", ok=None))
    failure(ctx.gh("pr", "create", "--repo", ctx.repo, "--head", head, "--title", title, "--body", "x", "--label", ctx.name("absent"), ok=None), "not found", "could not find")
    ctx.api("PATCH", f"repos/{ctx.repo}", {"private": True})
    missing(ctx.sweep.gh(other(ctx), "pr", "create", "--repo", ctx.repo, "--head", head, "--title", title, "--body", "x", ok=None))
    assert ctx.api("GET", f"repos/{ctx.repo}/pulls?state=all") == []


@case(command="pr diff", variant="missing repo pull and private repo", needs=("pr",))
def diff_errors(ctx):
    expect_error(ctx.gh("pr", "diff", "1", "--repo", ctx.account.name + "/" + ctx.name("missing"), ok=None), status=404)
    expect_error(ctx.gh("pr", "diff", "999999999", "--repo", ctx.repo, ok=None), status=404)
    ctx.api("PATCH", f"repos/{ctx.repo}", {"private": True})
    expect_error(ctx.sweep.gh(other(ctx), "pr", "diff", str(ctx.pr["number"]), "--repo", ctx.repo, ok=None), status=404)


@case(command="pr edit", variant="missing repo pull and non-owner denied", needs=("pr",))
def edit_errors(ctx):
    title = ctx.name("edited")
    missing(ctx.gh("pr", "edit", "1", "--repo", ctx.account.name + "/" + ctx.name("missing"), "--title", title, ok=None))
    missing(ctx.gh("pr", "edit", "999999999", "--repo", ctx.repo, "--title", title, ok=None))
    denied(ctx.sweep.gh(other(ctx), "pr", "edit", str(ctx.pr["number"]), "--repo", ctx.repo, "--title", title, ok=None))
    assert pull(ctx)["title"] == ctx.pr["title"]


@case(command="pr list", variant="missing and inaccessible repository", needs=("pr",))
def list_errors(ctx):
    missing(ctx.gh("pr", "list", "--repo", ctx.account.name + "/" + ctx.name("missing"), ok=None))
    ctx.api("PATCH", f"repos/{ctx.repo}", {"private": True})
    missing(ctx.sweep.gh(other(ctx), "pr", "list", "--repo", ctx.repo, ok=None))


@case(command="pr lock", variant="missing repo pull and permission denied", needs=("pr",))
def lock_errors(ctx):
    missing(ctx.gh("pr", "lock", "1", "--repo", ctx.account.name + "/" + ctx.name("missing"), ok=None))
    missing(ctx.gh("pr", "lock", "999999999", "--repo", ctx.repo, ok=None))
    denied(ctx.sweep.gh(other(ctx), "pr", "lock", str(ctx.pr["number"]), "--repo", ctx.repo, "--reason", "spam", ok=None))
    assert pull(ctx)["locked"] is False


@case(command="pr merge", variant="missing repo pull and non-owner denied", needs=("pr",))
def merge_errors(ctx):
    missing(ctx.gh("pr", "merge", "1", "--repo", ctx.account.name + "/" + ctx.name("missing"), "--merge", ok=None))
    missing(ctx.gh("pr", "merge", "999999999", "--repo", ctx.repo, "--merge", ok=None))
    denied(ctx.sweep.gh(other(ctx), "pr", "merge", str(ctx.pr["number"]), "--repo", ctx.repo, "--merge", ok=None))
    assert pull(ctx)["merged"] is False


@case(command="pr ready", variant="missing repo pull and non-owner denied", needs=("pr",))
def ready_errors(ctx):
    missing(ctx.gh("pr", "ready", "1", "--repo", ctx.account.name + "/" + ctx.name("missing"), ok=None))
    missing(ctx.gh("pr", "ready", "999999999", "--repo", ctx.repo, ok=None))
    denied(ctx.sweep.gh(other(ctx), "pr", "ready", str(ctx.pr["number"]), "--repo", ctx.repo, "--undo", ok=None))
    assert pull(ctx)["draft"] is False


@case(command="pr reopen", variant="missing repo pull and non-owner denied", needs=("pr",))
def reopen_errors(ctx):
    missing(ctx.gh("pr", "reopen", "1", "--repo", ctx.account.name + "/" + ctx.name("missing"), ok=None))
    missing(ctx.gh("pr", "reopen", "999999999", "--repo", ctx.repo, ok=None))
    ctx.api("PATCH", f"repos/{ctx.repo}/pulls/{ctx.pr['number']}", {"state": "closed"})
    denied(ctx.sweep.gh(other(ctx), "pr", "reopen", str(ctx.pr["number"]), "--repo", ctx.repo, ok=None))
    assert pull(ctx)["state"] == "closed"


@case(command="pr revert", variant="missing repo pull and non-owner denied", needs=("pr",))
def revert_errors(ctx):
    missing(ctx.gh("pr", "revert", "1", "--repo", ctx.account.name + "/" + ctx.name("missing"), ok=None))
    missing(ctx.gh("pr", "revert", "999999999", "--repo", ctx.repo, ok=None))
    ctx.api("PUT", f"repos/{ctx.repo}/pulls/{ctx.pr['number']}/merge", {"merge_method": "merge"})
    denied(ctx.sweep.gh(other(ctx), "pr", "revert", str(ctx.pr["number"]), "--repo", ctx.repo, ok=None))
    assert ctx.api("GET", f"repos/{ctx.repo}/pulls?state=open") == []


@case(command="pr review", variant="missing repo pull and private repo", needs=("pr",))
def review_errors(ctx):
    missing(ctx.gh("pr", "review", "1", "--repo", ctx.account.name + "/" + ctx.name("missing"), "--comment", "--body", "x", ok=None))
    missing(ctx.gh("pr", "review", "999999999", "--repo", ctx.repo, "--comment", "--body", "x", ok=None))
    ctx.api("PATCH", f"repos/{ctx.repo}", {"private": True})
    missing(ctx.sweep.gh(other(ctx), "pr", "review", str(ctx.pr["number"]), "--repo", ctx.repo, "--approve", ok=None))


@case(command="pr status", variant="missing and inaccessible repository", needs=("pr",))
def status_errors(ctx):
    missing(ctx.gh("pr", "status", "--repo", ctx.account.name + "/" + ctx.name("missing"), ok=None))
    ctx.api("PATCH", f"repos/{ctx.repo}", {"private": True})
    missing(ctx.sweep.gh(other(ctx), "pr", "status", "--repo", ctx.repo, ok=None))


@case(command="pr unlock", variant="missing repo pull and permission denied", needs=("pr",))
def unlock_errors(ctx):
    missing(ctx.gh("pr", "unlock", "1", "--repo", ctx.account.name + "/" + ctx.name("missing"), ok=None))
    missing(ctx.gh("pr", "unlock", "999999999", "--repo", ctx.repo, ok=None))
    ctx.api("PUT", f"repos/{ctx.repo}/issues/{ctx.pr['number']}/lock", {"lock_reason": "resolved"})
    denied(ctx.sweep.gh(other(ctx), "pr", "unlock", str(ctx.pr["number"]), "--repo", ctx.repo, ok=None))
    assert pull(ctx)["locked"] is True


@case(command="pr update-branch", variant="missing repo pull and non-owner denied", needs=("pr",))
def update_errors(ctx):
    missing(ctx.gh("pr", "update-branch", "1", "--repo", ctx.account.name + "/" + ctx.name("missing"), ok=None))
    missing(ctx.gh("pr", "update-branch", "999999999", "--repo", ctx.repo, ok=None))
    # Advance base so gh cannot short-circuit before the mutation's authority check.
    ctx.api("PUT", f"repos/{ctx.repo}/contents/{ctx.name('base')}.txt",
            {"branch": ctx.pr["base"]["ref"], "message": "advance base", "content": base64.b64encode(b"base\n").decode()})
    before = pull(ctx)["head"]["sha"]
    denied(ctx.sweep.gh(other(ctx), "pr", "update-branch", str(ctx.pr["number"]), "--repo", ctx.repo, ok=None))
    assert pull(ctx)["head"]["sha"] == before


@case(command="pr view", variant="missing repo pull private repo and invalid JSON field", needs=("pr",))
def view_errors(ctx):
    missing(ctx.gh("pr", "view", "1", "--repo", ctx.account.name + "/" + ctx.name("missing"), ok=None))
    missing(ctx.gh("pr", "view", "999999999", "--repo", ctx.repo, ok=None))
    failure(ctx.gh("pr", "view", str(ctx.pr["number"]), "--repo", ctx.repo, "--json", "notAField", ok=None), "Unknown JSON field")
    ctx.api("PATCH", f"repos/{ctx.repo}", {"private": True})
    missing(ctx.sweep.gh(other(ctx), "pr", "view", str(ctx.pr["number"]), "--repo", ctx.repo, ok=None))


@case(command="pr create", variant="project association", needs=("repo",))
def create_project(ctx):
    name = project(ctx)
    base, head = new_head(ctx)
    title, body = ctx.name("title"), ctx.name("body")
    row = created(ctx, ctx.gh("pr", "create", "--repo", ctx.repo, "--head", head, "--base", base,
                             "--title", title, "--body", body, "--project", name), title, body)
    value = expect_json(ctx.gh("pr", "view", str(row["number"]), "--repo", ctx.repo, "--json", "projectItems"),
                        {"projectItems": [{"title": str}]})
    assert any(item["title"] == name for item in value["projectItems"]), value


@case(command="pr edit", variant="add remove real project", needs=("pr",))
def edit_project(ctx):
    name, number = project(ctx), str(ctx.pr["number"])
    expect_ok(ctx.gh("pr", "edit", number, "--repo", ctx.repo, "--add-project", name))
    value = expect_json(ctx.gh("pr", "view", number, "--repo", ctx.repo, "--json", "projectItems"),
                        {"projectItems": [{"title": str}]})
    assert any(item["title"] == name for item in value["projectItems"]), value
    expect_ok(ctx.gh("pr", "edit", number, "--repo", ctx.repo, "--remove-project", name))
    expect_json(ctx.gh("pr", "view", number, "--repo", ctx.repo, "--json", "projectItems"), {"projectItems": []})


@case(command="pr checks", variant="required filters optional status", needs=("pr",))
def checks_required(ctx):
    required, optional = ctx.name("required"), ctx.name("optional")
    ctx.api("PUT", f"repos/{ctx.repo}/branches/{ctx.pr['base']['ref']}/protection",
            {"required_status_checks": {"strict": True, "contexts": [required]}, "enforce_admins": True,
             "required_pull_request_reviews": None, "restrictions": None})
    for name in (required, optional):
        ctx.api("POST", f"repos/{ctx.repo}/statuses/{ctx.pr['head']['sha']}", {"state": "success", "context": name})
    rows = expect_json(ctx.gh("pr", "checks", str(ctx.pr["number"]), "--repo", ctx.repo, "--required",
                              "--json", "name,bucket"), [{"name": str, "bucket": "pass"}])
    assert rows == [{"name": required, "bucket": "pass"}], rows


@case(command="co", variant="URL branch worktree and force resets divergent local commit", needs=("pr",))
def co_force_worktree(ctx):
    directory = clone(ctx)
    url = "https://" + HOST + "/" + ctx.repo + "/pull/" + str(ctx.pr["number"])
    branch, worktree = ctx.name("local"), ctx.directory / ctx.name("worktree")
    expect_ok(ctx.gh("co", url, "--repo", ctx.repo, "--branch", branch, "--worktree", str(worktree), cwd=directory))
    assert git(ctx, worktree, "branch", "--show-current") == branch
    git(ctx, worktree, "commit", "--allow-empty", "-m", "local divergence")
    divergent = git(ctx, worktree, "rev-parse", "HEAD")
    ctx.api("PUT", f"repos/{ctx.repo}/contents/{ctx.name('remote-change')}.txt",
            {"branch": ctx.pr["head"]["ref"], "message": "advance remote", "content": base64.b64encode(b"remote\n").decode()})
    failure(ctx.gh("co", str(ctx.pr["number"]), "--repo", ctx.repo, "--branch", branch, cwd=worktree, ok=None), "diverged", "fast-forward", "not possible")
    assert git(ctx, worktree, "rev-parse", "HEAD") == divergent
    expect_ok(ctx.gh("co", ctx.pr["head"]["ref"], "--repo", ctx.repo, "--branch", branch, "--force", cwd=worktree))
    assert git(ctx, worktree, "rev-parse", "HEAD") == pull(ctx)["head"]["sha"]


@case(command="pr checkout", variant="URL branch worktree and force resets divergent local commit", needs=("pr",))
def checkout_force_worktree(ctx):
    directory = clone(ctx)
    url = "https://" + HOST + "/" + ctx.repo + "/pull/" + str(ctx.pr["number"])
    branch, worktree = ctx.name("local"), ctx.directory / ctx.name("worktree")
    expect_ok(ctx.gh("pr", "checkout", url, "--repo", ctx.repo, "--branch", branch, "--worktree", str(worktree), cwd=directory))
    assert git(ctx, worktree, "branch", "--show-current") == branch
    git(ctx, worktree, "commit", "--allow-empty", "-m", "local divergence")
    divergent = git(ctx, worktree, "rev-parse", "HEAD")
    ctx.api("PUT", f"repos/{ctx.repo}/contents/{ctx.name('remote-change')}.txt",
            {"branch": ctx.pr["head"]["ref"], "message": "advance remote", "content": base64.b64encode(b"remote\n").decode()})
    failure(ctx.gh("pr", "checkout", str(ctx.pr["number"]), "--repo", ctx.repo, "--branch", branch, cwd=worktree, ok=None), "diverged", "fast-forward", "not possible")
    assert git(ctx, worktree, "rev-parse", "HEAD") == divergent
    expect_ok(ctx.gh("pr", "checkout", ctx.pr["head"]["ref"], "--repo", ctx.repo, "--branch", branch, "--force", cwd=worktree))
    assert git(ctx, worktree, "rev-parse", "HEAD") == pull(ctx)["head"]["sha"]


@case(command="pr comment", variant="valid attachment reaches GHES missing upload endpoint", needs=("pr",))
def comment_upload(ctx):
    path = png(ctx)
    expect_error(ctx.gh("pr", "comment", str(ctx.pr["number"]), "--repo", ctx.repo, "--body", "image",
                        "--attach", path + "#Sweep pixel", ok=None), status=404)
    assert comments(ctx) == []


@case(command="pr create", variant="valid attachment clean upload error", needs=("repo",))
def create_upload(ctx):
    base, head = new_head(ctx)
    title, body, path = ctx.name("title"), ctx.name("body"), png(ctx)
    result = ctx.gh("pr", "create", "--repo", ctx.repo, "--base", base, "--head", head,
                    "--title", title, "--body", body, "--attach", path, ok=None)
    expect_error(result, status=404)
    # gh documents partial creation despite upload failure. Any saved PR must
    # retain the user's text and must not contain a broken local file reference.
    rows = ctx.api("GET", f"repos/{ctx.repo}/pulls?state=all")
    assert len(rows) <= 1, rows
    if rows:
        assert rows[0]["title"] == title and rows[0]["body"] == body, rows


@case(command="pr edit", variant="valid attachment clean upload error preserves body", needs=("pr",))
def edit_upload(ctx):
    path = png(ctx)
    expect_error(ctx.gh("pr", "edit", str(ctx.pr["number"]), "--repo", ctx.repo, "--attach", path, ok=None), status=404)
    assert pull(ctx)["body"] == "sweep fixture"


@case(command="pr comment", variant="editor exits cleanly without posting", needs=("pr",))
def comment_editor(ctx):
    failure(ctx.gh("pr", "comment", str(ctx.pr["number"]), "--repo", ctx.repo, "--editor", ok=None), "editor", "exit status 1")
    assert comments(ctx) == []


@case(command="pr close", variant="without branch deletion preserves remote", needs=("pr",))
def close_preserves_branch(ctx):
    expect_ok(ctx.gh("pr", "close", str(ctx.pr["number"]), "--repo", ctx.repo))
    assert pull(ctx)["state"] == "closed" and comments(ctx) == []
    ref = ctx.api("GET", f"repos/{ctx.repo}/git/ref/heads/{ctx.pr['head']['ref']}")
    assert ref["object"]["sha"] == ctx.pr["head"]["sha"]


@case(command="pr reopen", variant="without comment", needs=("pr",))
def reopen_without_comment(ctx):
    ctx.api("PATCH", f"repos/{ctx.repo}/pulls/{ctx.pr['number']}", {"state": "closed"})
    expect_ok(ctx.gh("pr", "reopen", str(ctx.pr["number"]), "--repo", ctx.repo))
    assert pull(ctx)["state"] == "open" and comments(ctx) == []


@case(command="pr ready", variant="initial draft", needs=("repo",))
def ready_initial_draft(ctx):
    row = new_pull(ctx, draft=True)
    expect_ok(ctx.gh("pr", "ready", str(row["number"]), "--repo", ctx.repo))
    assert ctx.api("GET", f"repos/{ctx.repo}/pulls/{row['number']}")["draft"] is False


@case(command="pr merge", variant="body stdin", needs=("pr",))
def merge_body_stdin(ctx):
    body = ctx.name("merge-body")
    expect_ok(ctx.gh("pr", "merge", str(ctx.pr["number"]), "--repo", ctx.repo, "--merge", "--body-file", "-", input=body))
    row = pull(ctx)
    assert row["merged"] is True
    commit = ctx.api("GET", f"repos/{ctx.repo}/commits/{row['merge_commit_sha']}")
    assert body in commit["commit"]["message"]


@case(command="pr review", variant="incompatible actions and empty change request", needs=("pr",))
def review_bad_input(ctx):
    account = other(ctx)
    failure(ctx.sweep.gh(account, "pr", "review", str(ctx.pr["number"]), "--repo", ctx.repo,
                         "--approve", "--request-changes", "--body", "x", ok=None), "only one", "cannot", "not allowed")
    failure(ctx.sweep.gh(account, "pr", "review", str(ctx.pr["number"]), "--repo", ctx.repo,
                         "--request-changes", "--body", "", ok=None), "body", "required", "empty")
    assert ctx.api("GET", f"repos/{ctx.repo}/pulls/{ctx.pr['number']}/reviews") == []


@case(command="pr list", variant="combined metadata filters and pagination", needs=("pr",))
def list_combined(ctx):
    name, number = label(ctx), ctx.pr["number"]
    ctx.api("POST", f"repos/{ctx.repo}/issues/{number}/assignees", {"assignees": [ctx.account.name]})
    ctx.api("POST", f"repos/{ctx.repo}/issues/{number}/labels", {"labels": [name]})
    rows = expect_json(ctx.gh("pr", "list", "--repo", ctx.repo, "--assignee", "@me", "--author", ctx.account.name,
                              "--base", ctx.pr["base"]["ref"], "--head", ctx.pr["head"]["ref"], "--label", name,
                              "--state", "open", "--json", "number"), [{"number": int}])
    assert rows == [{"number": number}], rows
    second = new_pull(ctx)
    third = new_pull(ctx)
    rows = expect_json(ctx.gh("pr", "list", "--repo", ctx.repo, "--limit", "2", "--json", "number"), [{"number": int}])
    assert len(rows) == 2 and {row["number"] for row in rows}.issubset({number, second["number"], third["number"]}), rows


@case(command="pr status", variant="empty repository", needs=("repo",))
def status_empty(ctx):
    value = expect_json(ctx.gh("pr", "status", "--repo", ctx.repo, "--json", "number"),
                        {"createdBy": [], "needsReview": []})
    assert value.get("currentBranch") is None, value
    result = expect_ok(ctx.gh("pr", "status", "--repo", ctx.repo))
    assert ctx.repo in result.stdout and "no pull requests" in result.stdout.lower()


@case(command="pr diff", variant="invalid color and incompatible output flags", needs=("pr",))
def diff_bad_input(ctx):
    failure(ctx.gh("pr", "diff", str(ctx.pr["number"]), "--repo", ctx.repo, "--color", "invalid", ok=None), "color", "invalid")
    failure(ctx.gh("pr", "diff", str(ctx.pr["number"]), "--repo", ctx.repo, "--patch", "--name-only", ok=None), "cannot", "not allowed", "only one")


@case(command="pr edit", variant="body and body-file conflict", needs=("pr",))
def edit_bad_input(ctx):
    failure(ctx.gh("pr", "edit", str(ctx.pr["number"]), "--repo", ctx.repo, "--body", "x", "--body-file", "-", input="y", ok=None), "cannot", "not allowed", "only one")
    assert pull(ctx)["body"] == "sweep fixture"


@case(command="pr create", variant="missing title body bad base and body-file conflict", needs=("repo",))
def create_bad_input(ctx):
    base, head = new_head(ctx)
    title = ctx.name("title")
    failure(ctx.gh("pr", "create", "--repo", ctx.repo, "--head", head, ok=None), "--title", "--body", "not a terminal", "not running interactively")
    failure(ctx.gh("pr", "create", "--repo", ctx.repo, "--head", head, "--base", ctx.name("absent-base"),
                   "--title", title, "--body", "x", ok=None), "base", "not found", "Could not resolve")
    failure(ctx.gh("pr", "create", "--repo", ctx.repo, "--head", head, "--title", title,
                   "--body", "x", "--body-file", "-", input="y", ok=None), "cannot", "not allowed", "only one")
    assert ctx.api("GET", f"repos/{ctx.repo}/pulls?state=all") == []


@case(command="pr checks", variant="watch polls pending to success", needs=("pr",), timeout=60)
def checks_watch_transition(ctx):
    name = ctx.name("check")
    path = f"repos/{ctx.repo}/statuses/{ctx.pr['head']['sha']}"
    ctx.api("POST", path, {"state": "pending", "context": name})
    errors = []

    def complete():
        try:
            ctx.check_deadline()
            ctx.api("POST", path, {"state": "success", "context": name})
        except Exception as error:
            errors.append(error)

    timer = threading.Timer(2, complete)
    timer.daemon = True
    timer.start()
    try:
        result = expect_ok(ctx.gh("pr", "checks", str(ctx.pr["number"]), "--repo", ctx.repo,
                                  "--watch", "--interval", "1", timeout=20))
        assert name in result.stdout + result.stderr
    finally:
        timer.cancel()
        timer.join(timeout=30)
    assert not timer.is_alive() and not errors, errors
    status = ctx.api("GET", f"repos/{ctx.repo}/commits/{ctx.pr['head']['sha']}/status")
    assert status["state"] == "success", status


@case(command="pr create", variant="multi-commit fill versus fill-first versus fill-verbose", needs=("repo",), timeout=240)
def create_multi_fill(ctx):
    base, head = new_head(ctx)
    ctx.api("PUT", f"repos/{ctx.repo}/contents/{ctx.name('second')}.txt",
            {"branch": head, "message": "Sweep second commit\n\nSecond body", "content": base64.b64encode(b"second\n").decode()})
    directory = clone(ctx, head)
    result = expect_ok(ctx.gh("pr", "create", "--repo", ctx.repo, "--base", base, "--head", head, "--fill", cwd=directory))
    number = int(result.stdout.strip().rstrip("/").rsplit("/", 1)[1])
    row = ctx.api("GET", f"repos/{ctx.repo}/pulls/{number}")
    assert "Sweep first commit" in row["body"] and "Sweep second commit" in row["body"], row
    assert "Second body" not in row["body"], row
    base, head = new_head(ctx)
    ctx.api("PUT", f"repos/{ctx.repo}/contents/{ctx.name('second')}.txt",
            {"branch": head, "message": "Sweep second commit\n\nSecond body", "content": base64.b64encode(b"second\n").decode()})
    directory = clone(ctx, head)
    created(ctx, ctx.gh("pr", "create", "--repo", ctx.repo, "--base", base, "--head", head,
                        "--fill-first", cwd=directory), "Sweep first commit", "Commit body")
    base, head = new_head(ctx)
    ctx.api("PUT", f"repos/{ctx.repo}/contents/{ctx.name('second')}.txt",
            {"branch": head, "message": "Sweep second commit\n\nSecond body", "content": base64.b64encode(b"second\n").decode()})
    directory = clone(ctx, head)
    result = expect_ok(ctx.gh("pr", "create", "--repo", ctx.repo, "--base", base, "--head", head, "--fill-verbose", cwd=directory))
    number = int(result.stdout.strip().rstrip("/").rsplit("/", 1)[1])
    row = ctx.api("GET", f"repos/{ctx.repo}/pulls/{number}")
    assert all(text in row["body"] for text in ("Sweep first commit", "Commit body", "Sweep second commit", "Second body")), row


@case(command="co", variant="recurse initializes real submodule", needs=("pr",), timeout=240)
def co_submodule(ctx):
    sha = with_submodule(ctx)
    directory = clone(ctx)
    assert not (directory / "module").exists()
    expect_ok(ctx.gh("co", str(ctx.pr["number"]), "--repo", ctx.repo, "--recurse-submodules", cwd=directory))
    assert (directory / "module/.git").is_file()
    assert git(ctx, directory / "module", "rev-parse", "HEAD") == sha
    assert git(ctx, directory, "rev-parse", "HEAD") == pull(ctx)["head"]["sha"]


@case(command="pr checkout", variant="recurse initializes real submodule", needs=("pr",), timeout=240)
def checkout_submodule(ctx):
    sha = with_submodule(ctx)
    directory = clone(ctx)
    assert not (directory / "module").exists()
    expect_ok(ctx.gh("pr", "checkout", str(ctx.pr["number"]), "--repo", ctx.repo, "--recurse-submodules", cwd=directory))
    assert (directory / "module/.git").is_file()
    assert git(ctx, directory / "module", "rev-parse", "HEAD") == sha
    assert git(ctx, directory, "rev-parse", "HEAD") == pull(ctx)["head"]["sha"]
