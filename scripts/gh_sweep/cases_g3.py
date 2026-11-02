"""Real-gh issue, discussion and label cases; every repository is case-private.

GraphQL errors arrive over HTTP 200, so _failure checks gh's diagnostic rather
than pretending they contain a REST HTTP status. Browser/editor interactions
are explicitly waived, not counted as successful API coverage.
"""
import base64
import re

from gh_sweep.harness import case, expect_error, expect_json, expect_ok


def _failure(result, *messages):
    assert not result.timed_out and result.rc == 1, result
    assert result.stderr.strip(), result
    assert any(message.lower() in result.stderr.lower() for message in messages), result.stderr
    assert not re.search(r"panic:|panic at|segmentation fault|fatal error:", result.stderr, re.I), result.stderr


def _number(result):
    text = expect_ok(result).stdout.strip()
    assert re.search(r"/\d+$", text), text
    return text.rsplit("/", 1)[1]


def _missing(ctx):
    return ctx.account.name + "/" + ctx.name("absent")


def _other(ctx):
    return next(account for account in ctx.sweep.accounts if account != ctx.account)


def _issue(ctx, title=None, **fields):
    return ctx.api("POST", f"repos/{ctx.repo}/issues", {"title": title or ctx.name("issue"), **fields})


def _label(ctx, name=None, color="123456", description="sweep label"):
    return ctx.api("POST", f"repos/{ctx.repo}/labels",
                   {"name": name or ctx.name("label"), "color": color, "description": description})


def _milestone(ctx):
    return ctx.api("POST", f"repos/{ctx.repo}/milestones", {"title": ctx.name("milestone")})


def _discussions(ctx):
    ctx.api("PATCH", f"repos/{ctx.repo}", {"has_discussions": True})


def _png(ctx):
    path = ctx.directory / (ctx.name("image") + ".png")
    path.write_bytes(base64.b64decode(
        "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMCAO+jRZkAAAAASUVORK5CYII="))
    return str(path)


def _project(ctx):
    title = ctx.name("project")
    base = f"/users/{ctx.account.name}/projects"
    location = ctx.native("form", base, "--action", base, "--has", "title",
                          "--set", "title=" + title, "--status", "303", "--print", "location")
    number = int(location.rstrip("/").rsplit("/", 1)[1])
    path = f"users/{ctx.account.name}/projectsV2/{number}"
    assert ctx.api("GET", path)["title"] == title
    return title, path


def _clone(ctx):
    # Use the disposable loopback Git endpoint, not an external remote. The
    # credential is passed in an argument that the harness redacts on failure.
    path = ctx.directory / "clone"
    expect_ok(ctx.sweep.execute(ctx.account, [
        "git", "-c", "http.extraHeader=Authorization: Bearer " + ctx.account.token,
        "clone", ctx.sweep.base + "/" + ctx.repo + ".git", str(path)]))
    expect_ok(ctx.sweep.execute(ctx.account, ["git", "remote", "set-url", "origin",
                                              "https://codeyard-e2e.test/" + ctx.repo + ".git"], cwd=path))
    return path


@case(command="discussion create", variant="body, labels, stdin and category slug", needs=("repo",))
def discussion_create(ctx):
    _discussions(ctx)
    label = _label(ctx)["name"]
    title = ctx.name("discussion")
    number = _number(ctx.gh("discussion", "create", "--repo", ctx.repo, "--title", title,
                            "--body", "first body", "--category", "General", "--label", label))
    row = expect_json(ctx.gh("discussion", "view", number, "--repo", ctx.repo,
                             "--json", "number,title,body,category,labels"),
                      {"number": int(number), "title": title, "body": "first body",
                       "category": {"name": "General"}, "labels": [{"name": str}]})
    assert [item["name"] for item in row["labels"]] == [label], row
    title2 = ctx.name("stdin")
    number2 = _number(ctx.gh("discussion", "create", "--repo", ctx.repo, "--title", title2,
                             "--body-file", "-", "--category", "ideas", input="stdin discussion\n"))
    expect_json(ctx.gh("discussion", "view", number2, "--repo", ctx.repo, "--json", "title,body,category"),
                {"title": title2, "body": "stdin discussion\n", "category": {"name": "Ideas"}})
    body = ctx.directory / "discussion.md"
    body.write_text("file discussion\n")
    number3 = _number(ctx.gh("discussion", "create", "--repo", ctx.repo, "--title", ctx.name("file"),
                             "--body-file", str(body), "--category", "General"))
    expect_json(ctx.gh("discussion", "view", number3, "--repo", ctx.repo, "--json", "body"),
                {"body": "file discussion\n"})


@case(command="discussion create", variant="missing repo, category, disabled feature", needs=("repo",))
def discussion_create_errors(ctx):
    _failure(ctx.gh("discussion", "create", "--repo", _missing(ctx), "--title", ctx.name("missing"),
                    "--body", "body", "--category", "General", ok=None), "Could not resolve", "Not Found")
    _discussions(ctx)
    _failure(ctx.gh("discussion", "create", "--repo", ctx.repo, "--title", ctx.name("category"),
                    "--body", "body", "--category", ctx.name("absent"), ok=None), "category")
    ctx.api("PATCH", f"repos/{ctx.repo}", {"private": True})
    _failure(ctx.sweep.gh(_other(ctx), "discussion", "create", "--repo", ctx.repo,
                          "--title", ctx.name("denied"), "--body", "body", "--category", "General", ok=None),
             "Could not resolve", "Not Found")
    ctx.api("PATCH", f"repos/{ctx.repo}", {"has_discussions": False})
    _failure(ctx.gh("discussion", "create", "--repo", ctx.repo, "--title", ctx.name("disabled"),
                    "--body", "body", "--category", "General", ok=None), "disabled", "not enabled", "denied")


@case(command="discussion edit", variant="title, body, category, label replacement and files", needs=("repo",))
def discussion_edit(ctx):
    _discussions(ctx)
    old, new = _label(ctx)["name"], _label(ctx)["name"]
    number = _number(ctx.gh("discussion", "create", "--repo", ctx.repo, "--title", ctx.name("before"),
                            "--body", "before", "--category", "General", "--label", old))
    title = ctx.name("after")
    expect_ok(ctx.gh("discussion", "edit", number, "--repo", ctx.repo, "--title", title,
                     "--body", "after", "--category", "Ideas", "--add-label", new, "--remove-label", old))
    row = expect_json(ctx.gh("discussion", "view", number, "--repo", ctx.repo,
                             "--json", "title,body,category,labels"),
                      {"title": title, "body": "after", "category": {"name": "Ideas"}})
    assert [item["name"] for item in row["labels"]] == [new], row
    expect_ok(ctx.gh("discussion", "edit", number, "--repo", ctx.repo, "--body-file", "-", input="stdin edit"))
    expect_json(ctx.gh("discussion", "view", number, "--repo", ctx.repo, "--json", "body"), {"body": "stdin edit"})
    path = ctx.directory / "body.md"
    path.write_text("file edit")
    expect_ok(ctx.gh("discussion", "edit", number, "--repo", ctx.repo, "--body-file", str(path)))
    expect_json(ctx.gh("discussion", "view", number, "--repo", ctx.repo, "--json", "body"), {"body": "file edit"})
    _failure(ctx.gh("discussion", "edit", number, "--repo", ctx.repo, "--category", ctx.name("absent"), ok=None), "category")
    _failure(ctx.gh("discussion", "edit", "999999", "--repo", ctx.repo, "--title", title, ok=None),
             "Could not resolve", "not found")
    _failure(ctx.gh("discussion", "edit", number, "--repo", _missing(ctx), "--title", title, ok=None),
             "Could not resolve", "Not Found")
    _failure(ctx.sweep.gh(_other(ctx), "discussion", "edit", number, "--repo", ctx.repo,
                          "--title", ctx.name("denied"), ok=None), "denied", "not accessible", "permission")


@case(command="discussion comment", variant="comment, reply, edit and delete by id and URL", needs=("repo",))
def discussion_comment(ctx):
    _discussions(ctx)
    number = _number(ctx.gh("discussion", "create", "--repo", ctx.repo, "--title", ctx.name("thread"),
                            "--body", "thread", "--category", "General"))
    created = expect_ok(ctx.gh("discussion", "comment", number, "--repo", ctx.repo, "--body", "top-level"))
    assert "discussioncomment-" in created.stdout, created.stdout
    rows = expect_json(ctx.gh("discussion", "view", number, "--repo", ctx.repo, "--json", "comments"),
                       {"comments": [{"id": str, "body": str}]})["comments"]
    assert len(rows) == 1 and rows[0]["body"] == "top-level", rows
    comment = rows[0]["id"]
    expect_ok(ctx.gh("discussion", "comment", comment, "--repo", ctx.repo, "--body-file", "-", input="reply"))
    expect_ok(ctx.gh("discussion", "comment", comment, "--repo", ctx.repo, "--edit", "--body", "edited"))
    rows = expect_json(ctx.gh("discussion", "view", number, "--repo", ctx.repo, "--json", "comments"),
                       {"comments": [{"body": str}]})["comments"]
    assert rows[0]["body"] == "edited", rows
    assert rows[0]["replies"][0]["body"] == "reply", rows
    reply = rows[0]["replies"][0]["id"]
    path = ctx.directory / "comment.md"
    path.write_text("file reply")
    expect_ok(ctx.gh("discussion", "comment", reply, "--repo", ctx.repo, "--edit", "--body-file", str(path)))
    rows = expect_json(ctx.gh("discussion", "view", number, "--repo", ctx.repo, "--json", "comments"),
                       {"comments": list})["comments"]
    assert rows[0]["replies"][0]["body"] == "file reply", rows
    _failure(ctx.sweep.gh(_other(ctx), "discussion", "comment", comment, "--repo", ctx.repo,
                          "--edit", "--body", "forbidden", ok=None), "denied", "permission", "not accessible")
    expect_ok(ctx.gh("discussion", "comment", created.stdout.strip(), "--repo", ctx.repo, "--delete", "--yes"))
    expect_json(ctx.gh("discussion", "view", number, "--repo", ctx.repo, "--json", "comments"), {"comments": []})
    _failure(ctx.gh("discussion", "comment", "999999", "--repo", ctx.repo, "--body", "missing", ok=None),
             "Could not resolve", "not found")
    _failure(ctx.gh("discussion", "comment", number, "--repo", _missing(ctx), "--body", "missing", ok=None),
             "Could not resolve", "Not Found")


@case(command="discussion list", variant="filters, ordering, formatting and cursor", needs=("repo",))
def discussion_list(ctx):
    _discussions(ctx)
    label = _label(ctx)["name"]
    title = ctx.name("discussion")
    number = _number(ctx.gh("discussion", "create", "--repo", ctx.repo, "--title", title,
                            "--body", "body", "--category", "Q&A", "--label", label))
    rows = expect_json(ctx.gh("discussion", "list", "--repo", ctx.repo, "--state", "all", "--author", ctx.account.name,
                              "--category", "Q&A", "--label", label, "--limit", "1", "--order", "asc",
                              "--sort", "created", "--json", "number,title,answered"),
                       [{"number": int, "title": str, "answered": bool}])
    assert rows == [{"number": int(number), "title": title, "answered": False}], rows
    expect_json(ctx.gh("discussion", "list", "--repo", ctx.repo, "--answered", "--json", "number"), [])
    expect_json(ctx.gh("discussion", "list", "--repo", ctx.repo, "--state", "closed", "--json", "number"), [])
    found = expect_ok(ctx.gh("discussion", "list", "--repo", ctx.repo, "--search", title,
                             "--json", "title", "--jq", ".[0].title"))
    assert found.stdout.strip() == title, found
    formatted = expect_ok(ctx.gh("discussion", "list", "--repo", ctx.repo, "--sort", "updated", "--order", "desc",
                                 "--json", "title", "--template", "{{range .}}{{.title}}{{end}}"))
    assert formatted.stdout == title, formatted
    owner, repo = ctx.repo.split("/")
    page = expect_json(ctx.gh("api", "graphql", "-f", "owner=" + owner, "-f", "repo=" + repo,
                              "-f", 'query=query($owner:String!,$repo:String!){repository(owner:$owner,name:$repo){discussions(first:1){pageInfo{endCursor}}}}'),
                       {"data": {"repository": {"discussions": {"pageInfo": {"endCursor": str}}}}})
    cursor = page["data"]["repository"]["discussions"]["pageInfo"]["endCursor"]
    expect_json(ctx.gh("discussion", "list", "--repo", ctx.repo, "--after", cursor, "--json", "number"), [])
    plain = expect_ok(ctx.gh("discussion", "list", "--repo", ctx.repo))
    assert title in plain.stdout, plain
    unanswered = expect_json(ctx.gh("discussion", "list", "--repo", ctx.repo, "--answered=false", "--json", "number"),
                             [{"number": int(number)}])
    assert unanswered == [{"number": int(number)}], unanswered
    _failure(ctx.gh("discussion", "list", "--repo", ctx.repo, "--order", "invalid", ok=None), "order")
    _failure(ctx.gh("discussion", "list", "--repo", ctx.repo, "--after", "invalid-cursor", ok=None), "cursor")
    _failure(ctx.gh("discussion", "list", "--repo", _missing(ctx), "--json", "number", ok=None),
             "Could not resolve", "Not Found")
    ctx.api("PATCH", f"repos/{ctx.repo}", {"private": True})
    _failure(ctx.sweep.gh(_other(ctx), "discussion", "list", "--repo", ctx.repo, "--json", "number", ok=None),
             "Could not resolve", "Not Found")


@case(command="discussion view", variant="comments, ordering, cursor, jq and templates", needs=("repo",))
def discussion_view(ctx):
    _discussions(ctx)
    title = ctx.name("view")
    number = _number(ctx.gh("discussion", "create", "--repo", ctx.repo, "--title", title,
                            "--body", "view body", "--category", "General"))
    expect_ok(ctx.gh("discussion", "comment", number, "--repo", ctx.repo, "--body", "first comment"))
    row = expect_json(ctx.gh("discussion", "view", number, "--repo", ctx.repo, "--comments", "--order", "oldest",
                             "--limit", "1", "--json", "number,title,body,comments"),
                      {"number": int(number), "title": title, "body": "view body", "comments": [{"body": "first comment"}]})
    assert len(row["comments"]) == 1, row
    assert expect_ok(ctx.gh("discussion", "view", number, "--repo", ctx.repo, "--json", "title",
                            "--jq", ".title")).stdout.strip() == title
    assert expect_ok(ctx.gh("discussion", "view", number, "--repo", ctx.repo, "--json", "body",
                            "--template", "{{.body}}")).stdout == "view body"
    plain = expect_ok(ctx.gh("discussion", "view", number, "--repo", ctx.repo, "--comments", "--order", "newest"))
    assert "first comment" in plain.stdout, plain
    owner, repo = ctx.repo.split("/")
    page = expect_json(ctx.gh("api", "graphql", "-f", "owner=" + owner, "-f", "repo=" + repo, "-F", "n=" + number,
                              "-f", 'query=query($owner:String!,$repo:String!,$n:Int!){repository(owner:$owner,name:$repo){discussion(number:$n){comments(first:1){pageInfo{endCursor}}}}}'),
                       {"data": {"repository": {"discussion": {"comments": {"pageInfo": {"endCursor": str}}}}}})
    cursor = page["data"]["repository"]["discussion"]["comments"]["pageInfo"]["endCursor"]
    expect_json(ctx.gh("discussion", "view", number, "--repo", ctx.repo, "--comments", "--after", cursor,
                       "--json", "comments"), {"comments": []})
    _failure(ctx.gh("discussion", "view", "999999", "--repo", ctx.repo, "--json", "title", ok=None),
             "Could not resolve", "not found")
    _failure(ctx.gh("discussion", "view", number, "--repo", _missing(ctx), "--json", "title", ok=None),
             "Could not resolve", "Not Found")
    ctx.api("PATCH", f"repos/{ctx.repo}", {"private": True})
    _failure(ctx.sweep.gh(_other(ctx), "discussion", "view", number, "--repo", ctx.repo, "--json", "title", ok=None),
             "Could not resolve", "Not Found")


@case(command="issue create", variant="metadata, body, body file and stdin", needs=("repo",))
def issue_create(ctx):
    title, label = ctx.name("issue"), _label(ctx)["name"]
    milestone = _milestone(ctx)["title"]
    number = _number(ctx.gh("issue", "create", "--repo", ctx.repo, "--title", title, "--body", "created body",
                            "--assignee", "@me", "--label", label, "--milestone", milestone))
    row = ctx.api("GET", f"repos/{ctx.repo}/issues/{number}")
    assert (row["title"], row["body"]) == (title, "created body"), row
    assert [a["login"] for a in row["assignees"]] == [ctx.account.name], row
    assert [a["name"] for a in row["labels"]] == [label] and row["milestone"]["title"] == milestone, row
    path = ctx.directory / "issue.md"
    path.write_text("file body\n")
    number2 = _number(ctx.gh("issue", "create", "--repo", ctx.repo, "--title", ctx.name("file"), "--body-file", str(path)))
    assert ctx.api("GET", f"repos/{ctx.repo}/issues/{number2}")["body"] == "file body\n"
    number3 = _number(ctx.gh("issue", "create", "--repo", ctx.repo, "--title", ctx.name("stdin"),
                             "--body-file", "-", input="stdin body"))
    assert ctx.api("GET", f"repos/{ctx.repo}/issues/{number3}")["body"] == "stdin body"


@case(command="issue create", variant="parent and dependency relationships", needs=("repo",))
def issue_create_relationships(ctx):
    parent, blocker, blocked = _issue(ctx), _issue(ctx), _issue(ctx)
    number = _number(ctx.gh("issue", "create", "--repo", ctx.repo, "--title", ctx.name("child"), "--body", "relationships",
                            "--parent", str(parent["number"]), "--blocked-by", str(blocker["number"]),
                            "--blocking", str(blocked["number"])))
    row = expect_json(ctx.gh("issue", "view", number, "--repo", ctx.repo, "--json", "parent,blockedBy,blocking"),
                      {"parent": {"number": parent["number"]}})
    assert [v["number"] for v in row["blockedBy"]] == [blocker["number"]], row
    assert [v["number"] for v in row["blocking"]] == [blocked["number"]], row


@case(command="issue create", variant="missing repo and invalid metadata", needs=("repo",))
def issue_create_errors(ctx):
    _failure(ctx.gh("issue", "create", "--repo", _missing(ctx), "--title", ctx.name("missing"), "--body", "body", ok=None),
             "Could not resolve", "Not Found")
    _failure(ctx.gh("issue", "create", "--repo", ctx.repo, "--title", ctx.name("label"), "--body", "body",
                    "--label", ctx.name("absent"), ok=None), "label")
    _failure(ctx.gh("issue", "create", "--repo", ctx.repo, "--title", ctx.name("milestone"), "--body", "body",
                    "--milestone", ctx.name("absent"), ok=None), "milestone")
    _failure(ctx.gh("issue", "create", "--repo", ctx.repo, "--title", ctx.name("project"), "--body", "body",
                    "--project", ctx.name("absent"), ok=None), "project")
    _failure(ctx.gh("issue", "create", "--repo", ctx.repo, "--title", ctx.name("type"), "--body", "body",
                    "--type", ctx.name("absent"), ok=None), "type")
    _failure(ctx.gh("issue", "create", "--repo", ctx.repo, "--title", ctx.name("recover"), "--body", "body",
                    "--recover", str(ctx.directory / ctx.name("absent")), ok=None), "recover", "no such file")
    # Private repositories must not leak through a second identity's GraphQL lookup.
    ctx.api("PATCH", f"repos/{ctx.repo}", {"private": True})
    _failure(ctx.sweep.gh(_other(ctx), "issue", "create", "--repo", ctx.repo, "--title", ctx.name("denied"),
                          "--body", "body", ok=None), "Could not resolve", "Not Found")


@case(command="issue create", variant="attachment is rejected by gh on GHES", needs=("repo",))
def issue_create_attach(ctx):
    # gh 2.102 has a host guard: this flag cannot reach HTTP on GHES, even
    # though ordinary issue creation works. Check its actual clean diagnostic.
    result = ctx.gh("issue", "create", "--repo", ctx.repo, "--title", ctx.name("attachment"),
                    "--body", "screenshot", "--attach", _png(ctx), ok=None)
    _failure(result, "attaching files is not supported on GitHub Enterprise Server")


@case(command="issue comment", variant="create, edit-last, create-if-none, delete-last", needs=("issue",))
def issue_comment(ctx):
    number = str(ctx.issue["number"])
    expect_ok(ctx.gh("issue", "comment", number, "--repo", ctx.repo, "--edit-last", "--create-if-none", "--body", "initial"))
    rows = ctx.api("GET", f"repos/{ctx.repo}/issues/{number}/comments")
    assert len(rows) == 1 and rows[0]["body"] == "initial", rows
    path = ctx.directory / "comment.md"
    path.write_text("file comment")
    expect_ok(ctx.gh("issue", "comment", number, "--repo", ctx.repo, "--edit-last", "--body-file", str(path)))
    rows = ctx.api("GET", f"repos/{ctx.repo}/issues/{number}/comments")
    assert len(rows) == 1 and rows[0]["body"] == "file comment", rows
    expect_ok(ctx.gh("issue", "comment", number, "--repo", ctx.repo, "--body-file", "-", input="stdin comment"))
    rows = ctx.api("GET", f"repos/{ctx.repo}/issues/{number}/comments")
    assert [r["body"] for r in rows] == ["file comment", "stdin comment"], rows
    expect_ok(ctx.gh("issue", "comment", number, "--repo", ctx.repo, "--delete-last", "--yes"))
    rows = ctx.api("GET", f"repos/{ctx.repo}/issues/{number}/comments")
    assert [r["body"] for r in rows] == ["file comment"], rows
    expect_ok(ctx.gh("issue", "comment", number, "--repo", ctx.repo, "--delete-last", "--yes"))
    _failure(ctx.gh("issue", "comment", number, "--repo", ctx.repo, "--edit-last", "--body", "absent", ok=None),
             "no comments", "not found", "no comment")
    _failure(ctx.gh("issue", "comment", "999999", "--repo", ctx.repo, "--body", "missing", ok=None),
             "Could not resolve", "not found")
    _failure(ctx.gh("issue", "comment", number, "--repo", _missing(ctx), "--body", "missing", ok=None),
             "Could not resolve", "Not Found")


@case(command="issue comment", variant="locked conversation denies non-owner", needs=("issue",))
def issue_comment_locked(ctx):
    number = str(ctx.issue["number"])
    ctx.api("PUT", f"repos/{ctx.repo}/issues/{number}/lock", {"lock_reason": "resolved"})
    _failure(ctx.sweep.gh(_other(ctx), "issue", "comment", number, "--repo", ctx.repo, "--body", "denied", ok=None),
             "locked", "denied", "not permitted", "not accessible")
    assert ctx.api("GET", f"repos/{ctx.repo}/issues/{number}/comments") == []


@case(command="issue comment", variant="attachment is rejected by gh on GHES", needs=("issue",))
def issue_comment_attach(ctx):
    _failure(ctx.gh("issue", "comment", str(ctx.issue["number"]), "--repo", ctx.repo,
                    "--body", "screenshot", "--attach", _png(ctx), ok=None),
             "attaching files is not supported on GitHub Enterprise Server")


@case(command="issue edit", variant="metadata add/remove, title, body, files, multiple issues", needs=("issue",))
def issue_edit(ctx):
    number, label, milestone = str(ctx.issue["number"]), _label(ctx)["name"], _milestone(ctx)["title"]
    title = ctx.name("edited")
    expect_ok(ctx.gh("issue", "edit", number, "--repo", ctx.repo, "--title", title, "--body", "edited body",
                     "--add-assignee", "@me", "--add-label", label, "--milestone", milestone))
    row = ctx.api("GET", f"repos/{ctx.repo}/issues/{number}")
    assert (row["title"], row["body"]) == (title, "edited body"), row
    assert [a["login"] for a in row["assignees"]] == [ctx.account.name], row
    assert [a["name"] for a in row["labels"]] == [label] and row["milestone"]["title"] == milestone, row
    expect_ok(ctx.gh("issue", "edit", number, "--repo", ctx.repo, "--remove-assignee", "@me", "--remove-label", label,
                     "--remove-milestone", "--body-file", "-", input="stdin edit"))
    row = ctx.api("GET", f"repos/{ctx.repo}/issues/{number}")
    assert row["assignees"] == [] and row["labels"] == [] and row["milestone"] is None and row["body"] == "stdin edit", row
    path = ctx.directory / "edit.md"
    path.write_text("file edit")
    second = str(_issue(ctx)["number"])
    expect_ok(ctx.gh("issue", "edit", number, second, "--repo", ctx.repo, "--body-file", str(path)))
    assert ctx.api("GET", f"repos/{ctx.repo}/issues/{number}")["body"] == "file edit"
    assert ctx.api("GET", f"repos/{ctx.repo}/issues/{second}")["body"] == "file edit"


@case(command="issue edit", variant="parent, sub-issues and both dependency directions", needs=("issue",))
def issue_edit_relationships(ctx):
    number = str(ctx.issue["number"])
    parent, child, blocker, blocked = _issue(ctx), _issue(ctx), _issue(ctx), _issue(ctx)
    expect_ok(ctx.gh("issue", "edit", number, "--repo", ctx.repo, "--parent", str(parent["number"]),
                     "--add-sub-issue", str(child["number"]), "--add-blocked-by", str(blocker["number"]),
                     "--add-blocking", str(blocked["number"])))
    row = expect_json(ctx.gh("issue", "view", number, "--repo", ctx.repo, "--json", "parent,subIssues,blockedBy,blocking"),
                      {"parent": {"number": parent["number"]}})
    assert [v["number"] for v in row["subIssues"]] == [child["number"]], row
    assert [v["number"] for v in row["blockedBy"]] == [blocker["number"]], row
    assert [v["number"] for v in row["blocking"]] == [blocked["number"]], row
    expect_ok(ctx.gh("issue", "edit", number, "--repo", ctx.repo, "--remove-parent",
                     "--remove-sub-issue", str(child["number"]), "--remove-blocked-by", str(blocker["number"]),
                     "--remove-blocking", str(blocked["number"])))
    expect_json(ctx.gh("issue", "view", number, "--repo", ctx.repo, "--json", "parent,subIssues,blockedBy,blocking"),
                {"parent": None, "subIssues": [], "blockedBy": [], "blocking": []})
    _failure(ctx.gh("issue", "edit", number, "--repo", ctx.repo, "--parent", number, ok=None),
             "itself", "cycle", "invalid", "own")


@case(command="issue edit", variant="type set/remove on organization repository")
def issue_edit_type(ctx):
    org = ctx.org()
    repo = ctx.sweep.create_repo(ctx.account, ctx.name("typed"), org=org)["full_name"]
    kind = ctx.api("POST", f"orgs/{org}/issue-types", {"name": ctx.name("type"), "color": "blue", "is_enabled": True})
    issue = ctx.api("POST", f"repos/{repo}/issues", {"title": ctx.name("typed")})
    number = str(issue["number"])
    expect_ok(ctx.gh("issue", "edit", number, "--repo", repo, "--type", kind["name"]))
    expect_json(ctx.gh("issue", "view", number, "--repo", repo, "--json", "issueType"), {"issueType": {"name": kind["name"]}})
    expect_ok(ctx.gh("issue", "edit", number, "--repo", repo, "--remove-type"))
    expect_json(ctx.gh("issue", "view", number, "--repo", repo, "--json", "issueType"), {"issueType": None})


@case(command="issue edit", variant="missing metadata, issue, repository and permission", needs=("issue",))
def issue_edit_errors(ctx):
    number = str(ctx.issue["number"])
    _failure(ctx.gh("issue", "edit", number, "--repo", ctx.repo, "--add-project", ctx.name("absent"), ok=None), "project")
    _failure(ctx.gh("issue", "edit", number, "--repo", ctx.repo, "--remove-project", ctx.name("absent"), ok=None), "project")
    _failure(ctx.gh("issue", "edit", number, "--repo", ctx.repo, "--type", ctx.name("absent"), ok=None), "type")
    _failure(ctx.gh("issue", "edit", number, "--repo", ctx.repo, "--add-label", ctx.name("absent"), ok=None), "label")
    _failure(ctx.gh("issue", "edit", "999999", "--repo", ctx.repo, "--body", "absent", ok=None), "Could not resolve", "not found")
    _failure(ctx.gh("issue", "edit", number, "--repo", _missing(ctx), "--body", "absent", ok=None), "Could not resolve", "Not Found")
    _failure(ctx.sweep.gh(_other(ctx), "issue", "edit", number, "--repo", ctx.repo, "--title", ctx.name("denied"), ok=None),
             "denied", "permission", "not accessible", "not permitted", "write access")
    assert ctx.api("GET", f"repos/{ctx.repo}/issues/{number}")["title"] == ctx.issue["title"]


@case(command="issue edit", variant="attachment is rejected by gh on GHES", needs=("issue",))
def issue_edit_attach(ctx):
    _failure(ctx.gh("issue", "edit", str(ctx.issue["number"]), "--repo", ctx.repo,
                    "--attach", _png(ctx), ok=None),
             "attaching files is not supported on GitHub Enterprise Server")


@case(command="issue close", variant="completed, not planned, duplicate and comment", needs=("issue",))
def issue_close(ctx):
    number = str(ctx.issue["number"])
    expect_ok(ctx.gh("issue", "close", number, "--repo", ctx.repo, "--reason", "completed", "--comment", "closing comment"))
    row = ctx.api("GET", f"repos/{ctx.repo}/issues/{number}")
    assert (row["state"], row["state_reason"]) == ("closed", "completed"), row
    assert ctx.api("GET", f"repos/{ctx.repo}/issues/{number}/comments")[-1]["body"] == "closing comment"
    repeated = expect_ok(ctx.gh("issue", "close", number, "--repo", ctx.repo))
    assert "already closed" in (repeated.stdout + repeated.stderr).lower(), repeated
    second = str(_issue(ctx)["number"])
    expect_ok(ctx.gh("issue", "close", second, "--repo", ctx.repo, "--reason", "not planned"))
    assert ctx.api("GET", f"repos/{ctx.repo}/issues/{second}")["state_reason"] == "not_planned"
    third = str(_issue(ctx)["number"])
    expect_ok(ctx.gh("issue", "close", third, "--repo", ctx.repo, "--duplicate-of", number))
    expect_json(ctx.gh("issue", "view", third, "--repo", ctx.repo, "--json", "state,stateReason"),
                {"state": "CLOSED", "stateReason": "DUPLICATE"})
    fourth = str(_issue(ctx)["number"])
    expect_ok(ctx.gh("issue", "close", fourth, "--repo", ctx.repo, "--reason", "duplicate", "--duplicate-of", number))
    assert ctx.api("GET", f"repos/{ctx.repo}/issues/{fourth}")["state"] == "closed"
    _failure(ctx.gh("issue", "close", "999999", "--repo", ctx.repo, ok=None), "Could not resolve", "not found")
    _failure(ctx.gh("issue", "close", number, "--repo", _missing(ctx), ok=None), "Could not resolve", "Not Found")
    _failure(ctx.gh("issue", "close", number, "--repo", ctx.repo, "--reason", "invalid", ok=None), "reason")


@case(command="issue close", variant="non-owner cannot close", needs=("issue",))
def issue_close_denied(ctx):
    number = str(ctx.issue["number"])
    _failure(ctx.sweep.gh(_other(ctx), "issue", "close", number, "--repo", ctx.repo, ok=None),
             "denied", "permission", "not permitted", "not accessible", "write access")
    assert ctx.api("GET", f"repos/{ctx.repo}/issues/{number}")["state"] == "open"


@case(command="issue reopen", variant="reopen with comment and already open", needs=("issue",))
def issue_reopen(ctx):
    number = str(ctx.issue["number"])
    ctx.api("PATCH", f"repos/{ctx.repo}/issues/{number}", {"state": "closed"})
    _failure(ctx.sweep.gh(_other(ctx), "issue", "reopen", number, "--repo", ctx.repo, ok=None),
             "denied", "permission", "not permitted", "not accessible", "write access")
    expect_ok(ctx.gh("issue", "reopen", number, "--repo", ctx.repo, "--comment", "reopening comment"))
    assert ctx.api("GET", f"repos/{ctx.repo}/issues/{number}")["state"] == "open"
    assert ctx.api("GET", f"repos/{ctx.repo}/issues/{number}/comments")[-1]["body"] == "reopening comment"
    result = expect_ok(ctx.gh("issue", "reopen", number, "--repo", ctx.repo))
    assert "already open" in (result.stdout + result.stderr).lower(), result
    _failure(ctx.gh("issue", "reopen", "999999", "--repo", ctx.repo, ok=None), "Could not resolve", "not found")
    _failure(ctx.gh("issue", "reopen", number, "--repo", _missing(ctx), ok=None), "Could not resolve", "Not Found")


@case(command="issue delete", variant="permission, confirmed delete and missing", needs=("issue",))
def issue_delete(ctx):
    number = str(ctx.issue["number"])
    _failure(ctx.sweep.gh(_other(ctx), "issue", "delete", number, "--repo", ctx.repo, "--yes", ok=None),
             "denied", "permission", "not permitted", "not accessible", "write access")
    result = expect_ok(ctx.gh("issue", "delete", number, "--repo", ctx.repo, "--yes"))
    assert "deleted" in (result.stdout + result.stderr).lower(), result
    assert ctx.api("GET", f"repos/{ctx.repo}/issues") == []
    _failure(ctx.gh("issue", "delete", number, "--repo", ctx.repo, "--yes", ok=None), "Could not resolve", "not found")
    _failure(ctx.gh("issue", "delete", number, "--repo", _missing(ctx), "--yes", ok=None), "Could not resolve", "Not Found")


@case(command="issue lock", variant="reason, default, invalid and permission", needs=("issue",))
def issue_lock(ctx):
    number = str(ctx.issue["number"])
    expect_error(ctx.sweep.gh(_other(ctx), "issue", "lock", number, "--repo", ctx.repo, ok=None), status=403)
    for reason in ("off_topic", "resolved", "spam", "too_heated"):
        expect_ok(ctx.gh("issue", "lock", number, "--repo", ctx.repo, "--reason", reason))
        row = ctx.api("GET", f"repos/{ctx.repo}/issues/{number}")
        assert row["locked"] is True and row["active_lock_reason"] == reason, row
        ctx.api("DELETE", f"repos/{ctx.repo}/issues/{number}/lock")
    expect_ok(ctx.gh("issue", "lock", number, "--repo", ctx.repo))
    assert ctx.api("GET", f"repos/{ctx.repo}/issues/{number}")["locked"] is True
    _failure(ctx.gh("issue", "lock", number, "--repo", ctx.repo, "--reason", "invalid", ok=None), "reason")
    expect_error(ctx.gh("issue", "lock", "999999", "--repo", ctx.repo, ok=None), status=404, contains="Not Found")
    expect_error(ctx.gh("issue", "lock", number, "--repo", _missing(ctx), ok=None), status=404, contains="Not Found")


@case(command="issue unlock", variant="unlock, idempotence, missing and permission", needs=("issue",))
def issue_unlock(ctx):
    number = str(ctx.issue["number"])
    ctx.api("PUT", f"repos/{ctx.repo}/issues/{number}/lock", {"lock_reason": "resolved"})
    expect_error(ctx.sweep.gh(_other(ctx), "issue", "unlock", number, "--repo", ctx.repo, ok=None), status=403)
    expect_ok(ctx.gh("issue", "unlock", number, "--repo", ctx.repo))
    row = ctx.api("GET", f"repos/{ctx.repo}/issues/{number}")
    assert row["locked"] is False and row["active_lock_reason"] is None, row
    expect_ok(ctx.gh("issue", "unlock", number, "--repo", ctx.repo))
    expect_error(ctx.gh("issue", "unlock", "999999", "--repo", ctx.repo, ok=None), status=404, contains="Not Found")
    expect_error(ctx.gh("issue", "unlock", number, "--repo", _missing(ctx), ok=None), status=404, contains="Not Found")


@case(command="issue pin", variant="pin, permission, three-pin capacity and missing", needs=("issue",))
def issue_pin(ctx):
    number = str(ctx.issue["number"])
    _failure(ctx.sweep.gh(_other(ctx), "issue", "pin", number, "--repo", ctx.repo, ok=None),
             "write access", "denied", "not accessible", "permission")
    expect_ok(ctx.gh("issue", "pin", number, "--repo", ctx.repo))
    expect_json(ctx.gh("issue", "view", number, "--repo", ctx.repo, "--json", "isPinned"), {"isPinned": True})
    for _ in range(2):
        expect_ok(ctx.gh("issue", "pin", str(_issue(ctx)["number"]), "--repo", ctx.repo))
    _failure(ctx.gh("issue", "pin", str(_issue(ctx)["number"]), "--repo", ctx.repo, ok=None), "three", "3", "limit")
    _failure(ctx.gh("issue", "pin", "999999", "--repo", ctx.repo, ok=None), "Could not resolve", "not found")
    _failure(ctx.gh("issue", "pin", number, "--repo", _missing(ctx), ok=None), "Could not resolve", "Not Found")


@case(command="issue unpin", variant="unpin, permission and missing", needs=("issue",))
def issue_unpin(ctx):
    number = str(ctx.issue["number"])
    expect_ok(ctx.gh("issue", "pin", number, "--repo", ctx.repo))
    _failure(ctx.sweep.gh(_other(ctx), "issue", "unpin", number, "--repo", ctx.repo, ok=None),
             "write access", "denied", "not accessible", "permission")
    expect_ok(ctx.gh("issue", "unpin", number, "--repo", ctx.repo))
    expect_json(ctx.gh("issue", "view", number, "--repo", ctx.repo, "--json", "isPinned"), {"isPinned": False})
    _failure(ctx.gh("issue", "unpin", "999999", "--repo", ctx.repo, ok=None), "Could not resolve", "not found")
    _failure(ctx.gh("issue", "unpin", number, "--repo", _missing(ctx), ok=None), "Could not resolve", "Not Found")


@case(command="issue transfer", variant="transfer preserves body and comments; missing and permission", needs=("issue",))
def issue_transfer(ctx):
    number = str(ctx.issue["number"])
    destination = ctx.sweep.create_repo(ctx.account, ctx.name("destination"))["full_name"]
    ctx.api("POST", f"repos/{ctx.repo}/issues/{number}/comments", {"body": "preserved comment"})
    _failure(ctx.sweep.gh(_other(ctx), "issue", "transfer", number, destination, "--repo", ctx.repo, ok=None),
             "write access", "denied", "not accessible", "permission")
    _failure(ctx.gh("issue", "transfer", number, ctx.repo, "--repo", ctx.repo, ok=None), "same", "another", "destination")
    _failure(ctx.gh("issue", "transfer", number, _missing(ctx), "--repo", ctx.repo, ok=None), "Could not resolve", "Not Found")
    _failure(ctx.gh("issue", "transfer", number, destination, "--repo", _missing(ctx), ok=None), "Could not resolve", "Not Found")
    _failure(ctx.gh("issue", "transfer", "999999", destination, "--repo", ctx.repo, ok=None), "Could not resolve", "not found")
    expect_ok(ctx.gh("issue", "transfer", number, destination, "--repo", ctx.repo))
    rows = ctx.api("GET", f"repos/{destination}/issues")
    assert len(rows) == 1 and rows[0]["title"] == ctx.issue["title"] and rows[0]["body"] == ctx.issue["body"], rows
    comments = ctx.api("GET", f"repos/{destination}/issues/{rows[0]['number']}/comments")
    assert [c["body"] for c in comments] == ["preserved comment"], comments
    assert ctx.api("GET", f"repos/{ctx.repo}/issues") == []


@case(command="issue develop", variant="list and create named branch in alternate repo", needs=("issue",))
def issue_develop(ctx):
    number, name = str(ctx.issue["number"]), ctx.name("branch")
    empty = expect_ok(ctx.gh("issue", "develop", number, "--repo", ctx.repo, "--list"))
    assert not empty.stdout.strip(), empty
    destination = ctx.sweep.create_repo(ctx.account, ctx.name("branch-repo"))["full_name"]
    base = ctx.api("GET", f"repos/{destination}")["default_branch"]
    expect_ok(ctx.gh("issue", "develop", number, "--repo", ctx.repo, "--branch-repo", destination, "--base", base, "--name", name))
    result = expect_ok(ctx.gh("issue", "develop", number, "--repo", ctx.repo, "--list"))
    assert name in result.stdout, result
    sha = ctx.api("GET", f"repos/{destination}/git/ref/heads/{name}")["object"]["sha"]
    assert sha == ctx.api("GET", f"repos/{destination}/git/ref/heads/{base}")["object"]["sha"]
    _failure(ctx.gh("issue", "develop", "999999", "--repo", ctx.repo, "--list", ok=None), "Could not resolve", "not found")
    _failure(ctx.gh("issue", "develop", number, "--repo", _missing(ctx), "--list", ok=None), "Could not resolve", "Not Found")
    _failure(ctx.gh("issue", "develop", number, "--repo", ctx.repo, "--branch-repo", _missing(ctx),
                    "--name", ctx.name("missing-target"), ok=None), "Could not resolve", "Not Found")
    _failure(ctx.gh("issue", "develop", number, "--repo", ctx.repo, "--base", ctx.name("absent"), "--name", ctx.name("bad-base"), ok=None),
             "Could not resolve", "not found", "reference", "base")
    _failure(ctx.sweep.gh(_other(ctx), "issue", "develop", number, "--repo", ctx.repo, "--name", ctx.name("denied"), ok=None),
             "write access", "denied", "not accessible", "permission", "not permitted")


@case(command="issue develop", variant="checkout and separate worktree", needs=("issue",), timeout=180)
def issue_develop_checkout(ctx):
    number, name = str(ctx.issue["number"]), ctx.name("checkout")
    clone = _clone(ctx)
    # Configure loopback fetch authentication only in this private clone.
    expect_ok(ctx.sweep.execute(ctx.account, ["git", "config", "http.extraHeader", "Authorization: Bearer " + ctx.account.token], cwd=clone))
    expect_ok(ctx.gh("issue", "develop", number, "--repo", ctx.repo, "--name", name, "--checkout", cwd=clone))
    branch = expect_ok(ctx.sweep.execute(ctx.account, ["git", "branch", "--show-current"], cwd=clone))
    assert branch.stdout.strip() == name, branch
    worktree, other = ctx.directory / "worktree", ctx.name("worktree")
    expect_ok(ctx.gh("issue", "develop", number, "--repo", ctx.repo, "--name", other,
                     "--checkout", "--worktree", str(worktree), cwd=clone))
    branch = expect_ok(ctx.sweep.execute(ctx.account, ["git", "branch", "--show-current"], cwd=worktree))
    assert branch.stdout.strip() == other, branch


@case(command="issue view", variant="plain, comments, JSON, jq, template and missing", needs=("issue",))
def issue_view(ctx):
    number = str(ctx.issue["number"])
    ctx.api("POST", f"repos/{ctx.repo}/issues/{number}/comments", {"body": "visible comment"})
    expect_json(ctx.gh("issue", "view", number, "--repo", ctx.repo, "--json", "number,title,body,state,comments"),
                {"number": int(number), "title": ctx.issue["title"], "body": "sweep fixture", "state": "OPEN",
                 "comments": [{"body": "visible comment"}]})
    plain = expect_ok(ctx.gh("issue", "view", number, "--repo", ctx.repo, "--comments"))
    assert "visible comment" in plain.stdout, plain
    assert expect_ok(ctx.gh("issue", "view", number, "--repo", ctx.repo, "--json", "title", "--jq", ".title")).stdout.strip() == ctx.issue["title"]
    assert expect_ok(ctx.gh("issue", "view", number, "--repo", ctx.repo, "--json", "body", "--template", "{{.body}}")).stdout == "sweep fixture"
    _failure(ctx.gh("issue", "view", "999999", "--repo", ctx.repo, ok=None), "Could not resolve", "not found")
    _failure(ctx.gh("issue", "view", number, "--repo", _missing(ctx), ok=None), "Could not resolve", "Not Found")
    ctx.api("PATCH", f"repos/{ctx.repo}", {"private": True})
    _failure(ctx.sweep.gh(_other(ctx), "issue", "view", number, "--repo", ctx.repo, ok=None), "Could not resolve", "Not Found")


@case(command="issue list", variant="filters, search, closed state and output formats", needs=("repo",))
def issue_list(ctx):
    label, milestone, title = _label(ctx)["name"], _milestone(ctx), ctx.name("listed")
    issue = _issue(ctx, title, body="mention @" + ctx.account.name, labels=[label],
                   assignees=[ctx.account.name], milestone=milestone["number"])
    rows = expect_json(ctx.gh("issue", "list", "--repo", ctx.repo, "--state", "open", "--assignee", "@me",
                              "--author", ctx.account.name, "--label", label, "--milestone", milestone["title"],
                              "--limit", "1", "--json", "number,title,state"), [{"number": int, "title": str, "state": str}])
    assert rows == [{"number": issue["number"], "title": title, "state": "OPEN"}], rows
    assert expect_ok(ctx.gh("issue", "list", "--repo", ctx.repo, "--search", title, "--mention", ctx.account.name,
                            "--json", "title", "--jq", ".[0].title")).stdout.strip() == title
    assert expect_ok(ctx.gh("issue", "list", "--repo", ctx.repo, "--json", "title",
                            "--template", "{{range .}}{{.title}}{{end}}")).stdout == title
    expect_json(ctx.gh("issue", "list", "--repo", ctx.repo, "--app", ctx.name("absent-app"), "--json", "number"), [])
    expect_json(ctx.gh("issue", "list", "--repo", ctx.repo, "--type", ctx.name("absent-type"), "--json", "number"), [])
    expect_json(ctx.gh("issue", "list", "--repo", ctx.repo, "--state", "closed", "--json", "number"), [])
    ctx.api("PATCH", f"repos/{ctx.repo}/issues/{issue['number']}", {"state": "closed"})
    closed = expect_json(ctx.gh("issue", "list", "--repo", ctx.repo, "--state", "all", "--json", "state"), [{"state": "CLOSED"}])
    assert closed == [{"state": "CLOSED"}], closed
    _failure(ctx.gh("issue", "list", "--repo", _missing(ctx), "--json", "number", ok=None), "Could not resolve", "Not Found")
    _failure(ctx.gh("issue", "list", "--repo", ctx.repo, "--state", "invalid", ok=None), "state")
    ctx.api("PATCH", f"repos/{ctx.repo}", {"private": True})
    _failure(ctx.sweep.gh(_other(ctx), "issue", "list", "--repo", ctx.repo, "--json", "number", ok=None), "Could not resolve", "Not Found")


@case(command="issue status", variant="assigned, mentioned, authored JSON and rendering", needs=("repo",))
def issue_status(ctx):
    title = ctx.name("status")
    issue = _issue(ctx, title, body="hello @" + ctx.account.name, assignees=[ctx.account.name])
    row = expect_json(ctx.gh("issue", "status", "--repo", ctx.repo, "--json", "number,title"),
                      {"assignedToYou": list, "createdByYou": list, "mentioningYou": list})
    for key in ("assignedToYou", "createdByYou", "mentioningYou"):
        assert {"number": issue["number"], "title": title} in row[key], row
    assert expect_ok(ctx.gh("issue", "status", "--repo", ctx.repo, "--json", "title",
                            "--jq", ".createdByYou[0].title")).stdout.strip() == title
    assert expect_ok(ctx.gh("issue", "status", "--repo", ctx.repo, "--json", "title",
                            "--template", "{{range .createdByYou}}{{.title}}{{end}}")).stdout == title
    plain = expect_ok(ctx.gh("issue", "status", "--repo", ctx.repo))
    assert title in plain.stdout, plain
    _failure(ctx.gh("issue", "status", "--repo", _missing(ctx), ok=None), "Could not resolve", "Not Found")
    ctx.api("PATCH", f"repos/{ctx.repo}", {"private": True})
    _failure(ctx.sweep.gh(_other(ctx), "issue", "status", "--repo", ctx.repo, ok=None), "Could not resolve", "Not Found")


@case(command="label create", variant="color, description, duplicate, force and defaults", needs=("repo",))
def label_create(ctx):
    name = ctx.name("label")
    expect_ok(ctx.gh("label", "create", name, "--repo", ctx.repo, "--color", "AABBCC", "--description", "first description"))
    row = ctx.api("GET", f"repos/{ctx.repo}/labels/{name}")
    assert (row["name"], row["color"], row["description"]) == (name, "aabbcc", "first description"), row
    expect_error(ctx.gh("label", "create", name, "--repo", ctx.repo, ok=None), status=422)
    expect_ok(ctx.gh("label", "create", name, "--repo", ctx.repo, "--force", "--color", "001122", "--description", "replacement"))
    row = ctx.api("GET", f"repos/{ctx.repo}/labels/{name}")
    assert (row["color"], row["description"]) == ("001122", "replacement"), row
    default = ctx.name("random-color")
    expect_ok(ctx.gh("label", "create", default, "--repo", ctx.repo))
    assert re.fullmatch("[0-9a-fA-F]{6}", ctx.api("GET", f"repos/{ctx.repo}/labels/{default}")["color"])
    _failure(ctx.gh("label", "create", ctx.name("invalid"), "--repo", ctx.repo, "--color", "xyz", ok=None), "color", "hex")
    expect_error(ctx.gh("label", "create", ctx.name("missing"), "--repo", _missing(ctx), ok=None), status=404, contains="Not Found")
    expect_error(ctx.sweep.gh(_other(ctx), "label", "create", ctx.name("denied"), "--repo", ctx.repo, ok=None), status=403)


@case(command="label edit", variant="rename, color, clear description, missing and permission", needs=("repo",))
def label_edit(ctx):
    name, renamed = _label(ctx)["name"], ctx.name("renamed")
    expect_error(ctx.sweep.gh(_other(ctx), "label", "edit", name, "--repo", ctx.repo, "--description", "denied", ok=None), status=403)
    expect_ok(ctx.gh("label", "edit", name, "--repo", ctx.repo, "--name", renamed, "--color", "ABCDEF", "--description", "new description"))
    row = ctx.api("GET", f"repos/{ctx.repo}/labels/{renamed}")
    assert (row["name"], row["color"], row["description"]) == (renamed, "abcdef", "new description"), row
    existing = _label(ctx)["name"]
    expect_error(ctx.gh("label", "edit", renamed, "--repo", ctx.repo, "--name", existing, ok=None), status=422)
    expect_ok(ctx.gh("label", "edit", renamed, "--repo", ctx.repo, "--description", ""))
    assert ctx.api("GET", f"repos/{ctx.repo}/labels/{renamed}")["description"] in (None, "")
    expect_error(ctx.gh("label", "edit", name, "--repo", ctx.repo, "--description", "missing", ok=None), status=404, contains="Not Found")
    expect_error(ctx.gh("label", "edit", renamed, "--repo", _missing(ctx), "--description", "missing", ok=None), status=404, contains="Not Found")
    _failure(ctx.gh("label", "edit", renamed, "--repo", ctx.repo, "--color", "xyz", ok=None), "color", "hex")


@case(command="label delete", variant="confirmed delete, missing and permission", needs=("repo",))
def label_delete(ctx):
    name = _label(ctx)["name"]
    expect_error(ctx.sweep.gh(_other(ctx), "label", "delete", name, "--repo", ctx.repo, "--yes", ok=None), status=403)
    expect_ok(ctx.gh("label", "delete", name, "--repo", ctx.repo, "--yes"))
    assert name not in [label["name"] for label in ctx.api("GET", f"repos/{ctx.repo}/labels")]
    expect_error(ctx.gh("label", "delete", name, "--repo", ctx.repo, "--yes", ok=None), status=404, contains="Not Found")
    expect_error(ctx.gh("label", "delete", name, "--repo", _missing(ctx), "--yes", ok=None), status=404, contains="Not Found")


@case(command="label clone", variant="copy, skip conflicts, force overwrite, preserve destination", needs=("repo",))
def label_clone(ctx):
    source = ctx.sweep.create_repo(ctx.account, ctx.name("source"))["full_name"]
    name, extra = ctx.name("shared"), _label(ctx)["name"]
    ctx.api("POST", f"repos/{source}/labels", {"name": name, "color": "abcdef", "description": "source"})
    _label(ctx, name, "123456", "destination")
    expect_ok(ctx.gh("label", "clone", source, "--repo", ctx.repo))
    assert ctx.api("GET", f"repos/{ctx.repo}/labels/{name}")["color"] == "123456"
    expect_ok(ctx.gh("label", "clone", source, "--repo", ctx.repo, "--force"))
    row = ctx.api("GET", f"repos/{ctx.repo}/labels/{name}")
    assert (row["color"], row["description"]) == ("abcdef", "source"), row
    assert ctx.api("GET", f"repos/{ctx.repo}/labels/{extra}")["name"] == extra
    fresh = ctx.name("new")
    ctx.api("POST", f"repos/{source}/labels", {"name": fresh, "color": "654321"})
    expect_ok(ctx.gh("label", "clone", source, "--repo", ctx.repo))
    assert ctx.api("GET", f"repos/{ctx.repo}/labels/{fresh}")["color"] == "654321"
    _failure(ctx.gh("label", "clone", _missing(ctx), "--repo", ctx.repo, ok=None), "Could not resolve", "Not Found")
    _failure(ctx.gh("label", "clone", source, "--repo", _missing(ctx), ok=None), "Could not resolve", "Not Found")
    expect_error(ctx.sweep.gh(_other(ctx), "label", "clone", source, "--repo", ctx.repo, "--force", ok=None), status=403)


@case(command="label list", variant="sort, order, search, limit, jq and template", needs=("repo",))
def label_list(ctx):
    prefix = ctx.name("labels")
    names = [prefix + "-a", prefix + "-z"]
    for name in names:
        _label(ctx, name, "abcdef", "searchable " + prefix)
    rows = expect_json(ctx.gh("label", "list", "--repo", ctx.repo, "--sort", "name", "--order", "asc", "--limit", "100",
                              "--json", "name,color,description"), [{"name": str, "color": str}])
    actual = [row["name"] for row in rows]
    assert actual == sorted(actual) and set(names) <= set(actual), rows
    found = expect_json(ctx.gh("label", "list", "--repo", ctx.repo, "--search", prefix, "--json", "name"), [{"name": str}])
    assert {row["name"] for row in found} == set(names), found
    limited = expect_json(ctx.gh("label", "list", "--repo", ctx.repo, "--sort", "created", "--order", "desc", "--limit", "1",
                                 "--json", "name"), [{"name": str}])
    assert limited == [{"name": names[-1]}], limited
    assert expect_ok(ctx.gh("label", "list", "--repo", ctx.repo, "--search", names[0], "--json", "name",
                            "--jq", ".[0].name")).stdout.strip() == names[0]
    assert expect_ok(ctx.gh("label", "list", "--repo", ctx.repo, "--search", names[1], "--json", "name",
                            "--template", "{{range .}}{{.name}}{{end}}")).stdout == names[1]
    plain = expect_ok(ctx.gh("label", "list", "--repo", ctx.repo, "--search", prefix))
    assert all(name in plain.stdout for name in names), plain
    _failure(ctx.gh("label", "list", "--repo", _missing(ctx), "--json", "name", ok=None), "Could not resolve", "Not Found")
    _failure(ctx.gh("label", "list", "--repo", ctx.repo, "--sort", "invalid", ok=None), "sort")
    ctx.api("PATCH", f"repos/{ctx.repo}", {"private": True})
    _failure(ctx.sweep.gh(_other(ctx), "label", "list", "--repo", ctx.repo, "--json", "name", ok=None), "Could not resolve", "Not Found")


@case(command="issue create", variant="organization issue type")
def issue_create_type(ctx):
    org = ctx.org()
    repo = ctx.sweep.create_repo(ctx.account, ctx.name("typed"), org=org)["full_name"]
    kind = ctx.api("POST", f"orgs/{org}/issue-types", {"name": ctx.name("type"), "color": "blue", "is_enabled": True})
    number = _number(ctx.gh("issue", "create", "--repo", repo, "--title", ctx.name("typed-create"),
                            "--body", "typed body", "--type", kind["name"]))
    expect_json(ctx.gh("issue", "view", number, "--repo", repo, "--json", "issueType"), {"issueType": {"name": kind["name"]}})


@case(command="issue list", variant="positive issue type filter")
def issue_list_type(ctx):
    org = ctx.org()
    repo = ctx.sweep.create_repo(ctx.account, ctx.name("typed"), org=org)["full_name"]
    kind = ctx.api("POST", f"orgs/{org}/issue-types", {"name": ctx.name("type"), "color": "blue", "is_enabled": True})
    issue = ctx.api("POST", f"repos/{repo}/issues", {"title": ctx.name("typed"), "type": kind["name"]})
    ctx.api("POST", f"repos/{repo}/issues", {"title": ctx.name("untyped")})
    rows = expect_json(ctx.gh("issue", "list", "--repo", repo, "--type", kind["name"], "--json", "number,issueType"),
                       [{"number": int, "issueType": {"name": kind["name"]}}])
    assert len(rows) == 1 and rows[0]["number"] == issue["number"], rows


@case(command="issue create", variant="add new issue to project", needs=("repo",))
def issue_create_project(ctx):
    title, path = _project(ctx)
    number = _number(ctx.gh("issue", "create", "--repo", ctx.repo, "--title", ctx.name("project-issue"),
                            "--body", "project body", "--project", title))
    rows = ctx.api("GET", path + "/items")
    assert len(rows) == 1 and rows[0]["content"]["number"] == int(number), rows


@case(command="issue edit", variant="project add and remove", needs=("issue",))
def issue_edit_project(ctx):
    title, path = _project(ctx)
    number = str(ctx.issue["number"])
    expect_ok(ctx.gh("issue", "edit", number, "--repo", ctx.repo, "--add-project", title))
    rows = ctx.api("GET", path + "/items")
    assert len(rows) == 1 and rows[0]["content"]["number"] == int(number), rows
    expect_ok(ctx.gh("issue", "edit", number, "--repo", ctx.repo, "--remove-project", title))
    assert ctx.api("GET", path + "/items") == []


@case(command="discussion comment", variant="mutually exclusive action and body flags", needs=("repo",))
def discussion_comment_bad_flags(ctx):
    _failure(ctx.gh("discussion", "comment", "1", "--repo", ctx.repo, "--edit", "--delete", "--yes", ok=None),
             "cannot", "mutually exclusive", "only one")
    _failure(ctx.gh("discussion", "comment", "1", "--repo", ctx.repo, "--body", "body", "--body-file", "-", input="body", ok=None),
             "cannot", "mutually exclusive", "only one")


@case(command="issue comment", variant="invalid create-if-none and conflicting body input", needs=("issue",))
def issue_comment_bad_flags(ctx):
    number = str(ctx.issue["number"])
    _failure(ctx.gh("issue", "comment", number, "--repo", ctx.repo, "--create-if-none", "--body", "body", ok=None), "edit-last")
    _failure(ctx.gh("issue", "comment", number, "--repo", ctx.repo, "--body", "body", "--body-file", "-", input="body", ok=None),
             "cannot", "mutually exclusive", "only one")


@case(command="issue create", variant="public contributor versus restricted metadata", needs=("repo",))
def issue_create_contributor(ctx):
    other = _other(ctx)
    title = ctx.name("contributor")
    number = _number(ctx.sweep.gh(other, "issue", "create", "--repo", ctx.repo,
                                  "--title", title, "--body", "contributor body"))
    row = ctx.api("GET", f"repos/{ctx.repo}/issues/{number}")
    assert row["title"] == title and row["user"]["login"] == other.name, row
    label = _label(ctx)["name"]
    _failure(ctx.sweep.gh(other, "issue", "create", "--repo", ctx.repo, "--title", ctx.name("metadata-denied"),
                          "--body", "body", "--label", label, ok=None),
             "not accessible", "denied", "permission", "not permitted")


@case(command="issue comment", variant="edit/delete-last are scoped to current author", needs=("issue",))
def issue_comment_authors(ctx):
    number, other = str(ctx.issue["number"]), _other(ctx)
    expect_ok(ctx.gh("issue", "comment", number, "--repo", ctx.repo, "--body", "owner comment"))
    expect_ok(ctx.sweep.gh(other, "issue", "comment", number, "--repo", ctx.repo, "--body", "contributor comment"))
    expect_ok(ctx.gh("issue", "comment", number, "--repo", ctx.repo, "--edit-last", "--body", "owner edit"))
    rows = ctx.api("GET", f"repos/{ctx.repo}/issues/{number}/comments")
    assert [(r["user"]["login"], r["body"]) for r in rows] == [
        (ctx.account.name, "owner edit"), (other.name, "contributor comment")], rows
    expect_ok(ctx.gh("issue", "comment", number, "--repo", ctx.repo, "--delete-last", "--yes"))
    rows = ctx.api("GET", f"repos/{ctx.repo}/issues/{number}/comments")
    assert len(rows) == 1 and rows[0]["user"]["login"] == other.name and rows[0]["body"] == "contributor comment", rows


@case(command="issue create", variant="conflicting body flags", needs=("repo",))
def issue_create_bad_flags(ctx):
    _failure(ctx.gh("issue", "create", "--repo", ctx.repo, "--title", ctx.name("conflict"),
                    "--body", "body", "--body-file", "-", input="body", ok=None),
             "cannot", "mutually exclusive", "only one")

