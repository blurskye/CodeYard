"""Projects v2 against real gh (including its GraphQL, not REST, errors).

Fixtures use native forms/REST. Every gh invocation lives in a registered case.
Each project/organization is private to its case; no shared account settings change.
Browser flags are deliberately waived. GraphQL failures normally have HTTP 200,
so expect_error (which requires an HTTP status in stderr) is inappropriate here.
"""
import re
from urllib.parse import urlsplit

from gh_sweep.harness import case, expect_json, expect_ok


def _failure(result, contains):
    assert not result.timed_out and result.rc == 1, result
    assert contains.lower() in result.stderr.lower(), result.stderr
    assert not re.search(r"panic:|panic at|segmentation fault|fatal error:|Service unavailable", result.stderr, re.I), result.stderr
    return result


def _other(ctx):
    return next(a for a in ctx.sweep.accounts if a.name != ctx.account.name)


def _project(ctx, *, org=None, public=False):
    owner = org or ctx.account.name
    base = f"/{'orgs' if org else 'users'}/{owner}/projects"
    title = ctx.name("project")
    args = ["form", base, "--action", base, "--has", "title", "--set", "title=" + title]
    if public:
        args += ["--set", "public=on"]
    location = ctx.native(*args, "--status", "303", "--print", "location")
    number = int(urlsplit(location).path.rstrip("/").rsplit("/", 1)[1])
    rest = f"{'orgs' if org else 'users'}/{owner}/projectsV2/{number}"
    p = ctx.api("GET", rest)
    assert p["title"] == title and p["public"] == public, p
    p.update(_rest=rest, _owner=owner, _number=str(number))
    return p


def _field(ctx, p, kind="text", **configuration):
    return ctx.api("POST", p["_rest"] + "/fields",
                   {"name": ctx.name(kind), "data_type": kind, **configuration})


def _draft(ctx, p):
    prefix = (f"orgs/{p['_owner']}" if p["_rest"].startswith("orgs/")
              else f"user/{p['owner']['id']}")
    return ctx.api("POST", f"{prefix}/projectsV2/{p['number']}/drafts",
                   {"title": ctx.name("draft"), "body": ctx.name("body")})


def _item(ctx, p, item):
    return ctx.api("GET", f"{p['_rest']}/items/{item['id']}")


def _value(ctx, p, item, field):
    row = ctx.api("GET", f"{p['_rest']}/items/{item['id']}?fields={field['id']}")
    return next(f["value"] for f in row["fields"] if f["id"] == field["id"])


@case(command="project create", variant="user JSON, jq, template and organization")
def create(ctx):
    title = ctx.name("create")
    p = expect_json(ctx.gh("project", "create", "--owner", "@me", "--title", title,
                           "--format", "json"), {"id": str, "number": int, "title": title})
    assert p["id"] and p["number"] > 0, p
    assert ctx.api("GET", f"users/{ctx.account.name}/projectsV2/{p['number']}")["title"] == title
    title2 = ctx.name("jq")
    r = expect_ok(ctx.gh("project", "create", "--owner", ctx.account.name, "--title", title2,
                         "--format", "json", "--jq", ".title"))
    assert r.stdout.strip() == title2, r
    org = ctx.org()
    title3 = ctx.name("org")
    r = expect_ok(ctx.gh("project", "create", "--owner", org, "--title", title3,
                         "--format", "json", "--template", "{{.title}}"))
    assert r.stdout == title3, r
    assert any(p["title"] == title3 for p in ctx.api("GET", f"orgs/{org}/projectsV2"))


@case(command="project create", variant="missing owner, forbidden owner, invalid title")
def create_errors(ctx):
    _failure(ctx.gh("project", "create", "--owner", ctx.name("absent"), "--title", ctx.name(), ok=None),
             "Could not resolve")
    _failure(ctx.gh("project", "create", "--owner", _other(ctx).name, "--title", ctx.name(), ok=None),
             "Project write denied")
    _failure(ctx.gh("project", "create", "--owner", "@me", "--title", "x" * 256, ok=None),
             "Invalid project field")


@case(command="project view", variant="JSON, jq, template and plain output")
def view(ctx):
    p = _project(ctx)
    expect_json(ctx.gh("project", "view", p["_number"], "--owner", "@me", "--format", "json"),
                {"id": p["node_id"], "title": p["title"], "number": p["number"], "public": False, "closed": False})
    r = expect_ok(ctx.gh("project", "view", p["_number"], "--owner", ctx.account.name,
                         "--format", "json", "--jq", ".title"))
    assert r.stdout.strip() == p["title"], r
    r = expect_ok(ctx.gh("project", "view", p["_number"], "--owner", "@me",
                         "--format", "json", "--template", "{{.number}}:{{.title}}"))
    assert r.stdout == f"{p['number']}:{p['title']}", r
    r = expect_ok(ctx.gh("project", "view", p["_number"], "--owner", "@me"))
    assert p["title"] in r.stdout, r


@case(command="project view", variant="missing project, private project and public reader")
def view_errors(ctx):
    p = _project(ctx)
    _failure(ctx.gh("project", "view", "2147483647", "--owner", "@me", ok=None), "Could not resolve")
    other = _other(ctx)
    _failure(ctx.sweep.gh(other, "project", "view", p["_number"], "--owner", ctx.account.name,
                          "--format", "json", ok=None), "Could not resolve")
    public = _project(ctx, public=True)
    expect_json(ctx.sweep.gh(other, "project", "view", public["_number"], "--owner", ctx.account.name,
                             "--format", "json"), {"id": public["node_id"], "public": True})
    _failure(ctx.gh("project", "view", p["_number"], "--owner", "@me", "--format", "yaml", ok=None), "format")


@case(command="project list", variant="open/closed, limits, JSON, jq, template and plain output")
def list_projects(ctx):
    # An isolated organization avoids dirty-account pagination and other lanes' projects.
    org = ctx.org()
    opened = _project(ctx, org=org)
    closed = _project(ctx, org=org)
    expect_ok(ctx.gh("project", "close", closed["_number"], "--owner", org))
    rows = expect_json(ctx.gh("project", "list", "--owner", org, "--limit", "100", "--format", "json"),
                       {"projects": [{"id": str, "title": str}], "totalCount": int})
    assert {p["id"] for p in rows["projects"]} == {opened["node_id"]}, rows
    rows = expect_json(ctx.gh("project", "list", "--owner", org, "--closed", "--limit", "100",
                              "--format", "json"), {"projects": [{"id": str}]})
    assert {p["id"] for p in rows["projects"]} == {opened["node_id"], closed["node_id"]}, rows
    r = expect_ok(ctx.gh("project", "list", "--owner", org, "--closed", "--limit", "1",
                         "--format", "json", "--jq", ".projects | length"))
    assert r.stdout.strip() == "1", r
    r = expect_ok(ctx.gh("project", "list", "--owner", org, "--format", "json",
                         "--template", "{{range .projects}}{{.title}}{{end}}"))
    assert r.stdout == opened["title"], r
    r = expect_ok(ctx.gh("project", "list", "--owner", org))
    assert opened["title"] in r.stdout and closed["title"] not in r.stdout, r


@case(command="project list", variant="missing owner, empty organization, private visibility")
def list_errors(ctx):
    _failure(ctx.gh("project", "list", "--owner", ctx.name("absent"), ok=None), "Could not resolve")
    org = ctx.org()
    expect_json(ctx.gh("project", "list", "--owner", org, "--format", "json"),
                {"projects": [], "totalCount": 0})
    p = _project(ctx, org=org)
    expect_json(ctx.sweep.gh(_other(ctx), "project", "list", "--owner", org, "--format", "json"),
                {"projects": [], "totalCount": 0})
    assert ctx.api("GET", p["_rest"])["title"] == p["title"]


@case(command="project close", variant="close, reopen and all output modes")
def close(ctx):
    p = _project(ctx)
    expect_json(ctx.gh("project", "close", p["_number"], "--owner", "@me", "--format", "json"),
                {"id": p["node_id"], "closed": True})
    assert ctx.api("GET", p["_rest"])["state"] == "closed"
    r = expect_ok(ctx.gh("project", "close", p["_number"], "--owner", ctx.account.name, "--undo",
                         "--format", "json", "--jq", ".closed"))
    assert r.stdout.strip() == "false", r
    assert ctx.api("GET", p["_rest"])["state"] == "open"
    r = expect_ok(ctx.gh("project", "close", p["_number"], "--owner", "@me",
                         "--format", "json", "--template", "{{.closed}}"))
    assert r.stdout == "true", r
    expect_ok(ctx.gh("project", "close", p["_number"], "--owner", "@me"))
    assert ctx.api("GET", p["_rest"])["state"] == "closed"


@case(command="project close", variant="missing project and non-owner close/reopen")
def close_errors(ctx):
    _failure(ctx.gh("project", "close", "2147483647", "--owner", "@me", ok=None), "Could not resolve")
    p = _project(ctx, public=True)
    other = _other(ctx)
    _failure(ctx.sweep.gh(other, "project", "close", p["_number"], "--owner", ctx.account.name, ok=None),
             "Project write denied")
    expect_ok(ctx.gh("project", "close", p["_number"], "--owner", "@me"))
    _failure(ctx.sweep.gh(other, "project", "close", p["_number"], "--owner", ctx.account.name,
                          "--undo", ok=None), "Project write denied")
    assert ctx.api("GET", p["_rest"])["state"] == "closed"


@case(command="project edit", variant="metadata, both visibility values and output formats")
def edit(ctx):
    p = _project(ctx)
    title, description, readme = ctx.name("title"), ctx.name("description"), ctx.name("readme")
    expect_json(ctx.gh("project", "edit", p["_number"], "--owner", "@me", "--title", title,
                       "--description", description, "--readme", readme, "--visibility", "PUBLIC",
                       "--format", "json"), {"id": p["node_id"], "title": title, "public": True})
    expect_json(ctx.gh("project", "view", p["_number"], "--owner", "@me", "--format", "json"),
                {"title": title, "shortDescription": description, "readme": readme, "public": True})
    r = expect_ok(ctx.gh("project", "edit", p["_number"], "--owner", ctx.account.name,
                         "--visibility", "PRIVATE", "--format", "json", "--jq", ".public"))
    assert r.stdout.strip() == "false", r
    title2 = ctx.name("rename")
    r = expect_ok(ctx.gh("project", "edit", p["_number"], "--owner", "@me", "--title", title2,
                         "--format", "json", "--template", "{{.title}}"))
    assert r.stdout == title2, r
    # A partial edit must preserve metadata not passed in the second invocation.
    expect_json(ctx.gh("project", "view", p["_number"], "--owner", "@me", "--format", "json"),
                {"title": title2, "shortDescription": description, "readme": readme, "public": False})


@case(command="project edit", variant="missing, forbidden, invalid visibility and oversized title")
def edit_errors(ctx):
    _failure(ctx.gh("project", "edit", "2147483647", "--owner", "@me", "--title", ctx.name(), ok=None),
             "Could not resolve")
    p = _project(ctx, public=True)
    _failure(ctx.sweep.gh(_other(ctx), "project", "edit", p["_number"], "--owner", ctx.account.name,
                          "--title", ctx.name(), ok=None), "Project write denied")
    _failure(ctx.gh("project", "edit", p["_number"], "--owner", "@me", "--visibility", "INTERNAL", ok=None),
             "visibility")
    _failure(ctx.gh("project", "edit", p["_number"], "--owner", "@me", "--title", "x" * 256, ok=None),
             "Invalid project field")
    assert ctx.api("GET", p["_rest"])["title"] == p["title"]


@case(command="project copy", variant="include/exclude drafts, target org and output formats")
def copy(ctx):
    p = _project(ctx)
    draft = _draft(ctx, p)
    field = _field(ctx, p)
    title = ctx.name("copy")
    copied = expect_json(ctx.gh("project", "copy", p["_number"], "--source-owner", "@me",
                                "--target-owner", "@me", "--title", title, "--format", "json"),
                         {"id": str, "number": int, "title": title})
    assert copied["id"] != p["node_id"], copied
    prefix = f"users/{ctx.account.name}/projectsV2/{copied['number']}"
    assert ctx.api("GET", prefix + "/items") == []
    assert any(f["name"] == field["name"] for f in ctx.api("GET", prefix + "/fields"))
    org = ctx.org()
    title2 = ctx.name("with-drafts")
    r = expect_ok(ctx.gh("project", "copy", p["_number"], "--source-owner", ctx.account.name,
                         "--target-owner", org, "--title", title2, "--drafts", "--format", "json",
                         "--jq", ".number"))
    number = int(r.stdout.strip())
    items = ctx.api("GET", f"orgs/{org}/projectsV2/{number}/items")
    assert len(items) == 1 and items[0]["content"]["title"] == draft["content"]["title"], items
    assert items[0]["node_id"] != draft["node_id"], items
    title3 = ctx.name("template-copy")
    r = expect_ok(ctx.gh("project", "copy", p["_number"], "--source-owner", "@me",
                         "--target-owner", "@me", "--title", title3, "--format", "json",
                         "--template", "{{.title}}"))
    assert r.stdout == title3, r


@case(command="project copy", variant="linked issue retained, draft omitted and field value copied", needs=("issue",))
def copy_linked_item(ctx):
    p = _project(ctx)
    field = _field(ctx, p)
    _draft(ctx, p)
    added = expect_json(ctx.gh("project", "item-add", p["_number"], "--owner", "@me",
                               "--url", ctx.issue["html_url"], "--format", "json"), {"id": str, "type": "Issue"})
    expect_ok(ctx.gh("project", "item-edit", "--id", added["id"], "--project-id", p["node_id"],
                     "--field-id", field["node_id"], "--text", "copied-value"))
    title = ctx.name("linked-copy")
    copied = expect_json(ctx.gh("project", "copy", p["_number"], "--source-owner", "@me",
                                "--target-owner", "@me", "--title", title, "--format", "json"),
                         {"id": str, "number": int, "title": title})
    assert copied["id"] != p["node_id"], copied
    rest = f"users/{ctx.account.name}/projectsV2/{copied['number']}"
    items = ctx.api("GET", rest + "/items")
    assert len(items) == 1 and items[0]["content"]["node_id"] == ctx.issue["node_id"], items
    assert items[0]["node_id"] != added["id"], items
    new_field = next(f for f in ctx.api("GET", rest + "/fields") if f["name"] == field["name"])
    assert new_field["node_id"] != field["node_id"], new_field
    assert _value(ctx, {"_rest": rest}, items[0], new_field) == "copied-value"


@case(command="project copy", variant="missing source, unreadable source and forbidden target")
def copy_errors(ctx):
    _failure(ctx.gh("project", "copy", "2147483647", "--source-owner", "@me", "--target-owner", "@me",
                    "--title", ctx.name(), ok=None), "Could not resolve")
    p = _project(ctx)
    other = _other(ctx)
    _failure(ctx.sweep.gh(other, "project", "copy", p["_number"], "--source-owner", ctx.account.name,
                          "--target-owner", "@me", "--title", ctx.name(), ok=None), "Could not resolve")
    _failure(ctx.gh("project", "copy", p["_number"], "--source-owner", "@me", "--target-owner", other.name,
                    "--title", ctx.name(), ok=None), "Project write denied")
    _failure(ctx.gh("project", "copy", p["_number"], "--source-owner", "@me", "--target-owner", "@me",
                    "--title", "x" * 256, ok=None), "Invalid project field")


@case(command="project delete", variant="JSON, jq, template and deletion persistence")
def delete(ctx):
    p = _project(ctx)
    field = _field(ctx, p)
    _draft(ctx, p)
    expect_json(ctx.gh("project", "delete", p["_number"], "--owner", "@me", "--format", "json"),
                {"id": p["node_id"]})
    _failure(ctx.gh("project", "view", p["_number"], "--owner", "@me", ok=None), "Could not resolve")
    _failure(ctx.gh("project", "field-delete", "--id", field["node_id"], ok=None), "Could not resolve")
    p = _project(ctx)
    r = expect_ok(ctx.gh("project", "delete", p["_number"], "--owner", ctx.account.name,
                         "--format", "json", "--jq", ".id"))
    assert r.stdout.strip() == p["node_id"], r
    p = _project(ctx)
    r = expect_ok(ctx.gh("project", "delete", p["_number"], "--owner", "@me",
                         "--format", "json", "--template", "{{.id}}"))
    assert r.stdout == p["node_id"], r


@case(command="project delete", variant="missing project and non-owner")
def delete_errors(ctx):
    _failure(ctx.gh("project", "delete", "2147483647", "--owner", "@me", ok=None), "Could not resolve")
    p = _project(ctx, public=True)
    _failure(ctx.sweep.gh(_other(ctx), "project", "delete", p["_number"], "--owner", ctx.account.name,
                          ok=None), "Project write denied")
    assert ctx.api("GET", p["_rest"])["title"] == p["title"]


@case(command="project mark-template", variant="org mark/unmark and output modes")
def mark_template(ctx):
    org = ctx.org()
    p = _project(ctx, org=org)
    expect_json(ctx.gh("project", "mark-template", p["_number"], "--owner", org, "--format", "json"),
                {"id": p["node_id"]})
    assert ctx.api("GET", p["_rest"])["is_template"] is True
    r = expect_ok(ctx.gh("project", "mark-template", p["_number"], "--owner", org, "--undo",
                         "--format", "json", "--jq", ".id"))
    assert r.stdout.strip() == p["node_id"], r
    assert ctx.api("GET", p["_rest"])["is_template"] is False
    r = expect_ok(ctx.gh("project", "mark-template", p["_number"], "--owner", org,
                         "--format", "json", "--template", "{{.id}}"))
    assert r.stdout == p["node_id"], r
    assert ctx.api("GET", p["_rest"])["is_template"] is True


@case(command="project mark-template", variant="missing org project and non-owner mark/unmark")
def mark_template_errors(ctx):
    org = ctx.org()
    p = _project(ctx, org=org, public=True)
    _failure(ctx.gh("project", "mark-template", "2147483647", "--owner", org, ok=None), "Could not resolve")
    other = _other(ctx)
    _failure(ctx.sweep.gh(other, "project", "mark-template", p["_number"], "--owner", org, ok=None),
             "Project write denied")
    expect_ok(ctx.gh("project", "mark-template", p["_number"], "--owner", org))
    _failure(ctx.sweep.gh(other, "project", "mark-template", p["_number"], "--owner", org, "--undo", ok=None),
             "Project write denied")
    assert ctx.api("GET", p["_rest"])["is_template"] is True


@case(command="project field-create", variant="TEXT, NUMBER, DATE, SINGLE_SELECT and output modes")
def field_create(ctx):
    p = _project(ctx)
    for kind in ("TEXT", "NUMBER", "DATE"):
        name = ctx.name(kind)
        f = expect_json(ctx.gh("project", "field-create", p["_number"], "--owner", "@me", "--name", name,
                               "--data-type", kind, "--format", "json"), {"id": str, "name": name})
        stored = next(x for x in ctx.api("GET", p["_rest"] + "/fields") if x["node_id"] == f["id"])
        assert stored["data_type"] == kind.lower(), stored
    name = ctx.name("select")
    r = expect_ok(ctx.gh("project", "field-create", p["_number"], "--owner", ctx.account.name,
                         "--name", name, "--data-type", "SINGLE_SELECT", "--single-select-options", "Todo,Done",
                         "--format", "json", "--jq", ".name"))
    assert r.stdout.strip() == name, r
    stored = next(x for x in ctx.api("GET", p["_rest"] + "/fields") if x["name"] == name)
    assert [x["name"]["raw"] for x in stored["options"]] == ["Todo", "Done"], stored
    name2 = ctx.name("template")
    r = expect_ok(ctx.gh("project", "field-create", p["_number"], "--owner", "@me", "--name", name2,
                         "--data-type", "TEXT", "--format", "json", "--template", "{{.name}}"))
    assert r.stdout == name2, r


@case(command="project field-create", variant="missing, forbidden, invalid type and duplicate options")
def field_create_errors(ctx):
    _failure(ctx.gh("project", "field-create", "2147483647", "--owner", "@me", "--name", ctx.name(),
                    "--data-type", "TEXT", ok=None), "Could not resolve")
    p = _project(ctx, public=True)
    _failure(ctx.sweep.gh(_other(ctx), "project", "field-create", p["_number"], "--owner", ctx.account.name,
                          "--name", ctx.name(), "--data-type", "TEXT", ok=None), "Project write denied")
    _failure(ctx.gh("project", "field-create", p["_number"], "--owner", "@me", "--name", ctx.name(),
                    "--data-type", "BOGUS", ok=None), "data-type")
    _failure(ctx.gh("project", "field-create", p["_number"], "--owner", "@me", "--name", ctx.name(),
                    "--data-type", "SINGLE_SELECT", "--single-select-options", "same,same", ok=None),
             "Invalid project field")


@case(command="project field-list", variant="built-ins, pagination, JSON, jq, template and table")
def field_list(ctx):
    p = _project(ctx)
    fields = [_field(ctx, p) for _ in range(3)]
    rows = expect_json(ctx.gh("project", "field-list", p["_number"], "--owner", "@me", "--limit", "100",
                              "--format", "json"), {"fields": [{"id": str, "name": str}], "totalCount": int})
    assert {f["node_id"] for f in fields} <= {f["id"] for f in rows["fields"]}, rows
    assert "Title" in {f["name"] for f in rows["fields"]}, rows
    r = expect_ok(ctx.gh("project", "field-list", p["_number"], "--owner", ctx.account.name, "--limit", "1",
                         "--format", "json", "--jq", ".fields | length"))
    assert r.stdout.strip() == "1", r
    r = expect_ok(ctx.gh("project", "field-list", p["_number"], "--owner", "@me", "--limit", "100",
                         "--format", "json", "--template", "{{range .fields}}{{.name}}\n{{end}}"))
    assert all(f["name"] in r.stdout.splitlines() for f in fields), r
    r = expect_ok(ctx.gh("project", "field-list", p["_number"], "--owner", "@me"))
    assert all(f["name"] in r.stdout for f in fields), r


@case(command="project field-list", variant="missing and private project")
def field_list_errors(ctx):
    _failure(ctx.gh("project", "field-list", "2147483647", "--owner", "@me", ok=None), "Could not resolve")
    p = _project(ctx)
    _failure(ctx.sweep.gh(_other(ctx), "project", "field-list", p["_number"], "--owner", ctx.account.name,
                          ok=None), "Could not resolve")


@case(command="project field-delete", variant="JSON, jq, template and absence after deletion")
def field_delete(ctx):
    p = _project(ctx)
    f = _field(ctx, p)
    expect_json(ctx.gh("project", "field-delete", "--id", f["node_id"], "--format", "json"), {"id": f["node_id"]})
    assert f["id"] not in {x["id"] for x in ctx.api("GET", p["_rest"] + "/fields")}
    f = _field(ctx, p)
    r = expect_ok(ctx.gh("project", "field-delete", "--id", f["node_id"], "--format", "json", "--jq", ".id"))
    assert r.stdout.strip() == f["node_id"], r
    f = _field(ctx, p)
    r = expect_ok(ctx.gh("project", "field-delete", "--id", f["node_id"], "--format", "json",
                         "--template", "{{.id}}"))
    assert r.stdout == f["node_id"], r
    _failure(ctx.gh("project", "field-delete", "--id", f["node_id"], ok=None), "Could not resolve")


@case(command="project field-delete", variant="invalid ID, built-in field and non-owner")
def field_delete_errors(ctx):
    _failure(ctx.gh("project", "field-delete", "--id", ctx.name("invalid-id"), ok=None), "Could not resolve")
    p = _project(ctx, public=True)
    f = _field(ctx, p)
    _failure(ctx.sweep.gh(_other(ctx), "project", "field-delete", "--id", f["node_id"], ok=None),
             "Project write denied")
    title = next(x for x in ctx.api("GET", p["_rest"] + "/fields") if x["data_type"] == "title")
    _failure(ctx.gh("project", "field-delete", "--id", title["node_id"], ok=None), "Invalid project field")
    assert f["id"] in {x["id"] for x in ctx.api("GET", p["_rest"] + "/fields")}


@case(command="project item-create", variant="title/body, JSON, jq and template")
def item_create(ctx):
    p = _project(ctx)
    title, body = ctx.name("draft"), ctx.name("body") + "\nsecond line"
    item = expect_json(ctx.gh("project", "item-create", p["_number"], "--owner", "@me", "--title", title,
                              "--body", body, "--format", "json"), {"id": str, "title": title})
    stored = next(x for x in ctx.api("GET", p["_rest"] + "/items") if x["node_id"] == item["id"])
    assert stored["content"]["title"] == title and stored["content"]["body"] == body, stored
    title2 = ctx.name("bodyless")
    r = expect_ok(ctx.gh("project", "item-create", p["_number"], "--owner", ctx.account.name, "--title", title2,
                         "--format", "json", "--jq", ".title"))
    assert r.stdout.strip() == title2, r
    title3 = ctx.name("template")
    r = expect_ok(ctx.gh("project", "item-create", p["_number"], "--owner", "@me", "--title", title3,
                         "--body", ctx.name("body"), "--format", "json", "--template", "{{.title}}"))
    assert r.stdout == title3, r


@case(command="project item-create", variant="missing, forbidden and oversized title")
def item_create_errors(ctx):
    _failure(ctx.gh("project", "item-create", "2147483647", "--owner", "@me", "--title", ctx.name(), ok=None),
             "Could not resolve")
    p = _project(ctx, public=True)
    _failure(ctx.sweep.gh(_other(ctx), "project", "item-create", p["_number"], "--owner", ctx.account.name,
                          "--title", ctx.name(), ok=None), "Project write denied")
    _failure(ctx.gh("project", "item-create", p["_number"], "--owner", "@me", "--title", "x" * 256, ok=None),
             "Invalid project field")
    assert ctx.api("GET", p["_rest"] + "/items") == []


@case(command="project item-add", variant="issue, PR, idempotence and output modes", needs=("issue", "pr"))
def item_add(ctx):
    p = _project(ctx)
    issue_url = ctx.issue["html_url"]
    pr_url = ctx.pr["html_url"]
    item = expect_json(ctx.gh("project", "item-add", p["_number"], "--owner", "@me", "--url", issue_url,
                              "--format", "json"), {"id": str})
    stored = ctx.api("GET", p["_rest"] + "/items")
    assert len(stored) == 1 and stored[0]["node_id"] == item["id"], stored
    assert stored[0]["content"]["number"] == ctx.issue["number"], stored
    r = expect_ok(ctx.gh("project", "item-add", p["_number"], "--owner", ctx.account.name,
                         "--url", issue_url, "--format", "json", "--jq", ".id"))
    assert r.stdout.strip() == item["id"], r
    assert len(ctx.api("GET", p["_rest"] + "/items")) == 1
    r = expect_ok(ctx.gh("project", "item-add", p["_number"], "--owner", "@me", "--url", pr_url,
                         "--format", "json", "--template", "{{.id}}"))
    stored = ctx.api("GET", p["_rest"] + "/items")
    assert len(stored) == 2 and r.stdout in {x["node_id"] for x in stored}, (r, stored)
    assert any(x["content"].get("pull_request") or x["content"].get("merged") is not None
               for x in stored), stored


@case(command="project item-add", variant="missing repo/project, malformed URL and non-owner", needs=("issue",))
def item_add_errors(ctx):
    p = _project(ctx, public=True)
    url = ctx.issue["html_url"]
    _failure(ctx.gh("project", "item-add", "2147483647", "--owner", "@me", "--url", url, ok=None),
             "Could not resolve")
    origin = urlsplit(url)
    missing = f"{origin.scheme}://{origin.netloc}/{ctx.account.name}/{ctx.name('missing')}/issues/1"
    _failure(ctx.gh("project", "item-add", p["_number"], "--owner", "@me", "--url", missing, ok=None),
             "Could not resolve")
    _failure(ctx.gh("project", "item-add", p["_number"], "--owner", "@me", "--url", "not-a-url", ok=None), "Could not resolve")
    _failure(ctx.sweep.gh(_other(ctx), "project", "item-add", p["_number"], "--owner", ctx.account.name,
                          "--url", url, ok=None), "Project write denied")
    assert ctx.api("GET", p["_rest"] + "/items") == []


@case(command="project item-archive", variant="archive/unarchive, idempotence and output modes")
def item_archive(ctx):
    p = _project(ctx)
    item = _draft(ctx, p)
    expect_json(ctx.gh("project", "item-archive", p["_number"], "--owner", "@me", "--id", item["node_id"],
                       "--format", "json"), {"id": item["node_id"]})
    assert _item(ctx, p, item)["is_archived"] is True
    expect_json(ctx.gh("project", "item-list", p["_number"], "--owner", "@me", "--format", "json"),
                {"items": [], "totalCount": 0})
    r = expect_ok(ctx.gh("project", "item-archive", p["_number"], "--owner", ctx.account.name,
                         "--id", item["node_id"], "--undo", "--format", "json", "--jq", ".id"))
    assert r.stdout.strip() == item["node_id"], r
    assert _item(ctx, p, item)["is_archived"] is False
    expect_json(ctx.gh("project", "item-list", p["_number"], "--owner", "@me", "--format", "json"),
                {"items": [{"id": item["node_id"]}], "totalCount": 1})
    r = expect_ok(ctx.gh("project", "item-archive", p["_number"], "--owner", "@me", "--id", item["node_id"],
                         "--format", "json", "--template", "{{.id}}"))
    assert r.stdout == item["node_id"], r
    expect_ok(ctx.gh("project", "item-archive", p["_number"], "--owner", "@me", "--id", item["node_id"]))
    assert _item(ctx, p, item)["is_archived"] is True


@case(command="project item-archive", variant="missing item, cross-project item and non-owner undo")
def item_archive_errors(ctx):
    p = _project(ctx, public=True)
    item = _draft(ctx, p)
    _failure(ctx.gh("project", "item-archive", p["_number"], "--owner", "@me", "--id", ctx.name(), ok=None),
             "Could not resolve")
    other_project = _project(ctx)
    _failure(ctx.gh("project", "item-archive", other_project["_number"], "--owner", "@me",
                    "--id", item["node_id"], ok=None), "Could not resolve")
    other = _other(ctx)
    _failure(ctx.sweep.gh(other, "project", "item-archive", p["_number"], "--owner", ctx.account.name,
                          "--id", item["node_id"], ok=None), "Project write denied")
    expect_ok(ctx.gh("project", "item-archive", p["_number"], "--owner", "@me", "--id", item["node_id"]))
    _failure(ctx.sweep.gh(other, "project", "item-archive", p["_number"], "--owner", ctx.account.name,
                          "--id", item["node_id"], "--undo", ok=None), "Project write denied")
    assert _item(ctx, p, item)["is_archived"] is True


@case(command="project item-delete", variant="JSON, jq, template and removal persistence")
def item_delete(ctx):
    p = _project(ctx)
    item = _draft(ctx, p)
    expect_json(ctx.gh("project", "item-delete", p["_number"], "--owner", "@me", "--id", item["node_id"],
                       "--format", "json"), {"id": item["node_id"]})
    assert ctx.api("GET", p["_rest"] + "/items") == []
    item = _draft(ctx, p)
    r = expect_ok(ctx.gh("project", "item-delete", p["_number"], "--owner", ctx.account.name,
                         "--id", item["node_id"], "--format", "json", "--jq", ".id"))
    assert r.stdout.strip() == item["node_id"], r
    item = _draft(ctx, p)
    r = expect_ok(ctx.gh("project", "item-delete", p["_number"], "--owner", "@me", "--id", item["node_id"],
                         "--format", "json", "--template", "{{.id}}"))
    assert r.stdout == item["node_id"], r
    _failure(ctx.gh("project", "item-delete", p["_number"], "--owner", "@me", "--id", item["node_id"], ok=None),
             "Could not resolve")


@case(command="project item-delete", variant="invalid ID, cross-project item and non-owner")
def item_delete_errors(ctx):
    p = _project(ctx, public=True)
    item = _draft(ctx, p)
    _failure(ctx.gh("project", "item-delete", p["_number"], "--owner", "@me", "--id", ctx.name(), ok=None),
             "Could not resolve")
    other = _project(ctx)
    _failure(ctx.gh("project", "item-delete", other["_number"], "--owner", "@me", "--id", item["node_id"], ok=None),
             "Could not resolve")
    _failure(ctx.sweep.gh(_other(ctx), "project", "item-delete", p["_number"], "--owner", ctx.account.name,
                          "--id", item["node_id"], ok=None), "Project write denied")
    assert _item(ctx, p, item)["node_id"] == item["node_id"]


@case(command="project item-list", variant="empty/populated, limits, query, custom columns and output modes")
def item_list(ctx):
    p = _project(ctx)
    expect_json(ctx.gh("project", "item-list", p["_number"], "--owner", "@me", "--format", "json"),
                {"items": [], "totalCount": 0})
    drafts = [_draft(ctx, p) for _ in range(3)]
    field = _field(ctx, p)
    rows = expect_json(ctx.gh("project", "item-list", p["_number"], "--owner", "@me", "--limit", "100",
                              "--format", "json"), {"items": [{"id": str, "title": str}], "totalCount": 3})
    assert {x["id"] for x in rows["items"]} == {x["node_id"] for x in drafts}, rows
    r = expect_ok(ctx.gh("project", "item-list", p["_number"], "--owner", ctx.account.name, "--limit", "1",
                         "--format", "json", "--jq", ".items | length"))
    assert r.stdout.strip() == "1", r
    r = expect_ok(ctx.gh("project", "item-list", p["_number"], "--owner", "@me", "--format", "json",
                         "--template", "{{range .items}}{{.title}}\n{{end}}"))
    assert set(r.stdout.splitlines()) == {x["content"]["title"] for x in drafts}, r
    rows = expect_json(ctx.gh("project", "item-list", p["_number"], "--owner", "@me",
                              "--query", drafts[0]["content"]["title"], "--format", "json"), {"items": [{"id": str}]})
    assert [x["id"] for x in rows["items"]] == [drafts[0]["node_id"]], rows
    # --field/--field-id affect the table, so do not hide them behind JSON output.
    expect_json(ctx.gh("project", "item-edit", "--project-id", p["node_id"], "--id", drafts[0]["node_id"],
                       "--field-id", field["node_id"], "--text", "column-marker", "--format", "json"), {"id": str})
    r = expect_ok(ctx.gh("project", "item-list", p["_number"], "--owner", "@me", "--field", field["name"]))
    assert any(line.split("\t")[-1] == "column-marker" for line in r.stdout.splitlines()), r
    r = expect_ok(ctx.gh("project", "item-list", p["_number"], "--owner", "@me", "--field-id", field["node_id"]))
    assert any(line.split("\t")[-1] == "column-marker" for line in r.stdout.splitlines()), r


@case(command="project item-list", variant="missing/private project and unknown column")
def item_list_errors(ctx):
    _failure(ctx.gh("project", "item-list", "2147483647", "--owner", "@me", ok=None), "Could not resolve")
    p = _project(ctx)
    _failure(ctx.sweep.gh(_other(ctx), "project", "item-list", p["_number"], "--owner", ctx.account.name,
                          ok=None), "Could not resolve")
    _failure(ctx.gh("project", "item-list", p["_number"], "--owner", "@me", "--field", ctx.name("missing"),
                    ok=None), "field")


@case(command="project item-edit", variant="draft title/body, JSON, jq and template")
def item_edit_draft(ctx):
    p = _project(ctx)
    item = _draft(ctx, p)
    draft_id = item["content"]["node_id"]
    title, body = ctx.name("renamed"), ctx.name("new-body") + "\nmore"
    expect_json(ctx.gh("project", "item-edit", "--id", draft_id, "--title", title, "--body", body,
                       "--format", "json"), {"id": draft_id, "title": title, "body": body})
    stored = _item(ctx, p, item)["content"]
    assert stored["title"] == title and stored["body"] == body, stored
    title2 = ctx.name("title-only")
    r = expect_ok(ctx.gh("project", "item-edit", "--id", draft_id, "--title", title2,
                         "--format", "json", "--jq", ".title"))
    assert r.stdout.strip() == title2, r
    body2 = ctx.name("body-only")
    r = expect_ok(ctx.gh("project", "item-edit", "--id", draft_id, "--body", body2,
                         "--format", "json", "--template", "{{.body}}"))
    assert r.stdout == body2, r
    assert _item(ctx, p, item)["content"]["title"] == title2
    expect_json(ctx.gh("project", "item-edit", "--id", draft_id, "--body", "", "--format", "json"),
                {"title": title2, "body": ""})
    assert _item(ctx, p, item)["content"]["body"] == ""


@case(command="project item-edit", variant="typed values by IDs: text, number, date, select, iteration, clear")
def item_edit_values(ctx):
    p = _project(ctx)
    item = _draft(ctx, p)
    text = _field(ctx, p)
    number = _field(ctx, p, "number")
    date = _field(ctx, p, "date")
    select = _field(ctx, p, "single_select", single_select_options=[{"name": "Todo"}, {"name": "Done"}])
    iteration = _field(ctx, p, "iteration", iteration_configuration={
        "start_date": "2099-01-05", "duration": 14,
        "iterations": [{"title": ctx.name("sprint"), "start_date": "2099-01-05", "duration": 14}]})
    expect_json(ctx.gh("project", "item-edit", "--id", item["node_id"], "--project-id", p["node_id"],
                       "--field-id", text["node_id"], "--text", "typed text", "--format", "json"), {"id": item["node_id"]})
    assert _value(ctx, p, item, text) == "typed text"
    expect_ok(ctx.gh("project", "item-edit", "--id", item["node_id"], "--project-id", p["node_id"],
                     "--field-id", number["node_id"], "--number", "-12.5"))
    assert _value(ctx, p, item, number) == -12.5
    expect_ok(ctx.gh("project", "item-edit", "--id", item["node_id"], "--project-id", p["node_id"],
                     "--field-id", number["node_id"], "--number", "0"))
    assert _value(ctx, p, item, number) == 0
    expect_ok(ctx.gh("project", "item-edit", "--id", item["node_id"], "--project-id", p["node_id"],
                     "--field-id", date["node_id"], "--date", "2030-01-02"))
    assert _value(ctx, p, item, date) == "2030-01-02"
    option_id = select["options"][1]["id"]
    expect_ok(ctx.gh("project", "item-edit", "--id", item["node_id"], "--project-id", p["node_id"],
                     "--field-id", select["node_id"], "--single-select-option-id", option_id))
    assert _value(ctx, p, item, select) == option_id
    iteration_id = iteration["configuration"]["iterations"][0]["id"]
    expect_ok(ctx.gh("project", "item-edit", "--id", item["node_id"], "--project-id", p["node_id"],
                     "--field-id", iteration["node_id"], "--iteration-id", iteration_id))
    assert _value(ctx, p, item, iteration) == iteration_id
    for field in (text, number, date, select, iteration):
        expect_json(ctx.gh("project", "item-edit", "--id", item["node_id"], "--project-id", p["node_id"],
                           "--field-id", field["node_id"], "--clear", "--format", "json"), {"id": item["node_id"]})
        assert _value(ctx, p, item, field) is None


@case(command="project item-edit", variant="named field/option and issue URL resolution", needs=("issue",))
def item_edit_named(ctx):
    p = _project(ctx)
    url = ctx.issue["html_url"]
    item = expect_json(ctx.gh("project", "item-add", p["_number"], "--owner", "@me", "--url", url,
                              "--format", "json"), {"id": str})
    stored = next(x for x in ctx.api("GET", p["_rest"] + "/items") if x["node_id"] == item["id"])
    field = _field(ctx, p, "single_select", single_select_options=[{"name": "Todo"}, {"name": "In Progress"}])
    expect_json(ctx.gh("project", "item-edit", p["_number"], "--owner", ctx.account.name, "--url", url,
                       "--field", field["name"], "--value", "In Progress", "--format", "json"), {"id": item["id"]})
    assert _value(ctx, p, stored, field) == field["options"][1]["id"]
    expect_ok(ctx.gh("project", "item-edit", p["_number"], "--owner", "@me", "--url", url,
                     "--field", field["name"], "--clear"))
    assert _value(ctx, p, stored, field) is None
    text = _field(ctx, p)
    expect_ok(ctx.gh("project", "item-edit", p["_number"], "--owner", "@me", "--url", url,
                     "--field", text["name"], "--value", "resolved by name"))
    assert _value(ctx, p, stored, text) == "resolved by name"
    _failure(ctx.gh("project", "item-edit", p["_number"], "--owner", "@me", "--url", url,
                    "--field", field["name"], "--value", ctx.name("missing-option"), ok=None), "option")
    _failure(ctx.gh("project", "item-edit", p["_number"], "--owner", "@me", "--url", url,
                    "--field", ctx.name("missing-field"), "--value", "x", ok=None), "field")


@case(command="project item-edit", variant="invalid ID/type/date/option, conflicting values and non-owner")
def item_edit_errors(ctx):
    p = _project(ctx, public=True)
    item = _draft(ctx, p)
    text = _field(ctx, p)
    select = _field(ctx, p, "single_select", single_select_options=[{"name": "Todo"}])
    _failure(ctx.gh("project", "item-edit", "--id", ctx.name("invalid"), "--title", ctx.name(), ok=None),
             "ID must be the ID of the draft issue content")
    _failure(ctx.gh("project", "item-edit", "--id", "DI_" + ctx.name("missing"), "--title", ctx.name(), ok=None),
             "Could not resolve")
    _failure(ctx.sweep.gh(_other(ctx), "project", "item-edit", "--id", item["content"]["node_id"],
                          "--title", ctx.name(), ok=None), "Project write denied")
    _failure(ctx.sweep.gh(_other(ctx), "project", "item-edit", "--id", item["node_id"],
                          "--project-id", p["node_id"], "--field-id", text["node_id"], "--text", "denied", ok=None),
             "Project write denied")
    _failure(ctx.gh("project", "item-edit", "--id", item["node_id"], "--project-id", p["node_id"],
                    "--field-id", text["node_id"], "--number", "4", ok=None), "Invalid project field")
    _failure(ctx.gh("project", "item-edit", "--id", item["node_id"], "--project-id", p["node_id"],
                    "--field-id", select["node_id"], "--single-select-option-id", ctx.name("missing"), ok=None),
             "Invalid project field")
    _failure(ctx.gh("project", "item-edit", "--id", item["node_id"], "--project-id", p["node_id"],
                    "--field-id", text["node_id"], "--date", "not-a-date", ok=None), "date")
    _failure(ctx.gh("project", "item-edit", "--id", item["node_id"], "--project-id", p["node_id"],
                    "--field-id", text["node_id"], "--text", "x", "--number", "1", ok=None), "only one")
    assert _value(ctx, p, item, text) is None


@case(command="project link", variant="repository link, idempotence and team link", needs=("repo",))
def link(ctx):
    p = _project(ctx)
    r = expect_ok(ctx.gh("project", "link", p["_number"], "--owner", ctx.account.name, "--repo", ctx.repo))
    assert r.stdout.strip() == "" and r.stderr.strip() == "", r
    expect_ok(ctx.gh("project", "link", p["_number"], "--owner", ctx.account.name,
                     "--repo", ctx.repo.split("/", 1)[1]))
    owner, name = ctx.repo.split("/")
    query = ('query { repository(owner:"' + owner + '",name:"' + name + '") '
             '{ projectsV2(first:100) { nodes { id } } } }')
    rows = expect_json(ctx.gh("api", "graphql", "-f", "query=" + query),
                       {"data": {"repository": {"projectsV2": {"nodes": [{"id": str}]}}}})
    assert p["node_id"] in {x["id"] for x in rows["data"]["repository"]["projectsV2"]["nodes"]}, rows
    org = ctx.org()
    team = ctx.team(org)
    p = _project(ctx, org=org)
    r = expect_ok(ctx.gh("project", "link", p["_number"], "--owner", org, "--team", team["slug"]))
    assert r.stdout.strip() == "" and r.stderr.strip() == "", r
    expect_ok(ctx.gh("project", "link", p["_number"], "--owner", org, "--team", team["slug"]))


@case(command="project link", variant="missing repo/team, conflicting selectors and non-owner", needs=("repo",))
def link_errors(ctx):
    p = _project(ctx, public=True)
    _failure(ctx.gh("project", "link", p["_number"], "--owner", ctx.account.name,
                    "--repo", f"{ctx.account.name}/{ctx.name('absent')}", ok=None), "Could not resolve")
    _failure(ctx.sweep.gh(_other(ctx), "project", "link", p["_number"], "--owner", ctx.account.name,
                          "--repo", ctx.repo, ok=None), "Project write denied")
    _failure(ctx.gh("project", "link", p["_number"], "--owner", ctx.account.name, "--repo", ctx.repo,
                    "--team", ctx.name(), ok=None), "repo")
    org = ctx.org()
    p = _project(ctx, org=org, public=True)
    _failure(ctx.gh("project", "link", p["_number"], "--owner", org, "--team", ctx.name("missing"), ok=None),
             "Could not resolve")
    team = ctx.team(org)
    _failure(ctx.sweep.gh(_other(ctx), "project", "link", p["_number"], "--owner", org,
                          "--team", team["slug"], ok=None), "Project write denied")


@case(command="project unlink", variant="repository unlink, idempotence and team unlink", needs=("repo",))
def unlink(ctx):
    p = _project(ctx)
    expect_ok(ctx.gh("project", "link", p["_number"], "--owner", ctx.account.name, "--repo", ctx.repo))
    r = expect_ok(ctx.gh("project", "unlink", p["_number"], "--owner", ctx.account.name, "--repo", ctx.repo))
    assert r.stdout.strip() == "" and r.stderr.strip() == "", r
    expect_ok(ctx.gh("project", "unlink", p["_number"], "--owner", ctx.account.name,
                     "--repo", ctx.repo.split("/", 1)[1]))
    owner, name = ctx.repo.split("/")
    query = ('query { repository(owner:"' + owner + '",name:"' + name + '") '
             '{ projectsV2(first:100) { nodes { id } } } }')
    rows = expect_json(ctx.gh("api", "graphql", "-f", "query=" + query),
                       {"data": {"repository": {"projectsV2": {"nodes": []}}}})
    assert rows["data"]["repository"]["projectsV2"]["nodes"] == [], rows
    org = ctx.org()
    team = ctx.team(org)
    p = _project(ctx, org=org)
    expect_ok(ctx.gh("project", "link", p["_number"], "--owner", org, "--team", team["slug"]))
    r = expect_ok(ctx.gh("project", "unlink", p["_number"], "--owner", org, "--team", team["slug"]))
    assert r.stdout.strip() == "" and r.stderr.strip() == "", r
    expect_ok(ctx.gh("project", "unlink", p["_number"], "--owner", org, "--team", team["slug"]))


@case(command="project unlink", variant="missing repo/team, conflicting selectors and non-owner", needs=("repo",))
def unlink_errors(ctx):
    p = _project(ctx, public=True)
    _failure(ctx.gh("project", "unlink", p["_number"], "--owner", ctx.account.name,
                    "--repo", f"{ctx.account.name}/{ctx.name('absent')}", ok=None), "Could not resolve")
    expect_ok(ctx.gh("project", "link", p["_number"], "--owner", ctx.account.name, "--repo", ctx.repo))
    _failure(ctx.sweep.gh(_other(ctx), "project", "unlink", p["_number"], "--owner", ctx.account.name,
                          "--repo", ctx.repo, ok=None), "Project write denied")
    _failure(ctx.gh("project", "unlink", p["_number"], "--owner", ctx.account.name, "--repo", ctx.repo,
                    "--team", ctx.name(), ok=None), "repo")
    org = ctx.org()
    p = _project(ctx, org=org, public=True)
    _failure(ctx.gh("project", "unlink", p["_number"], "--owner", org, "--team", ctx.name("missing"), ok=None),
             "Could not resolve")
    team = ctx.team(org)
    expect_ok(ctx.gh("project", "link", p["_number"], "--owner", org, "--team", team["slug"]))
    _failure(ctx.sweep.gh(_other(ctx), "project", "unlink", p["_number"], "--owner", org,
                          "--team", team["slug"], ok=None), "Project write denied")


@case(command="project item-edit", variant="named number/date, typed URL addressing and PR URL", needs=("pr",))
def item_edit_named_types(ctx):
    p = _project(ctx)
    url = ctx.pr["html_url"]
    added = expect_json(ctx.gh("project", "item-add", p["_number"], "--owner", "@me", "--url", url,
                               "--format", "json"), {"id": str, "type": "PullRequest"})
    item = next(x for x in ctx.api("GET", p["_rest"] + "/items") if x["node_id"] == added["id"])
    number = _field(ctx, p, "number")
    date = _field(ctx, p, "date")
    text = _field(ctx, p)
    expect_json(ctx.gh("project", "item-edit", p["_number"], "--owner", "@me", "--url", url,
                       "--field", number["name"], "--value", "0", "--format", "json"), {"id": added["id"]})
    assert _value(ctx, p, item, number) == 0
    r = expect_ok(ctx.gh("project", "item-edit", p["_number"], "--owner", "@me", "--url", url,
                         "--field", date["name"], "--value", "2031-02-03", "--format", "json", "--jq", ".id"))
    assert r.stdout.strip() == added["id"], r
    assert _value(ctx, p, item, date) == "2031-02-03"
    r = expect_ok(ctx.gh("project", "item-edit", p["_number"], "--owner", "@me", "--url", url,
                         "--field-id", text["node_id"], "--text", "URL with field ID", "--format", "json",
                         "--template", "{{.id}}"))
    assert r.stdout == added["id"], r
    assert _value(ctx, p, item, text) == "URL with field ID"
    _failure(ctx.gh("project", "item-edit", p["_number"], "--owner", "@me", "--url", url,
                    "--field", number["name"], "--value", "not-a-number", ok=None), "invalid number value")
    _failure(ctx.gh("project", "item-edit", p["_number"], "--owner", "@me", "--url", url,
                    "--field", date["name"], "--value", "2031-02-30", ok=None), "day out of range")
    empty = _project(ctx)
    _failure(ctx.gh("project", "item-edit", empty["_number"], "--owner", "@me", "--url", url,
                    "--field-id", text["node_id"], "--text", "not a member", ok=None), "is not an item in project")


@case(command="project item-edit", variant="selector exclusions, required IDs and no changes")
def item_edit_selectors(ctx):
    p = _project(ctx)
    item = _draft(ctx, p)
    field = _field(ctx, p)
    _failure(ctx.gh("project", "item-edit", "--id", item["node_id"], "--field-id", field["node_id"],
                    "--project-id", p["node_id"], "--text", "x", "--clear", ok=None), "in conjunction with `--clear`")
    _failure(ctx.gh("project", "item-edit", p["_number"], "--owner", "@me", "--id", item["node_id"],
                    "--url", "https://localhost.invalid/a/b/issues/1", "--text", "x", ok=None),
             "only one of `--url` or `--id`")
    _failure(ctx.gh("project", "item-edit", "--id", item["node_id"], "--field", field["name"],
                    "--field-id", field["node_id"], "--text", "x", ok=None), "only one of `--field` or `--field-id`")
    _failure(ctx.gh("project", "item-edit", "--id", item["node_id"], "--field-id", field["node_id"],
                    "--value", "x", ok=None), "`--value` cannot be used with `--field-id`")
    _failure(ctx.gh("project", "item-edit", "--id", item["node_id"], "--field", field["name"],
                    "--text", "x", ok=None), "`--field` cannot be used with `--id`")
    _failure(ctx.gh("project", "item-edit", "--id", item["node_id"], "--text", "x", ok=None),
             "field-id and project-id must be provided")
    _failure(ctx.gh("project", "item-edit", "--id", item["node_id"], "--field-id", field["node_id"],
                    "--clear", ok=None), "project-id must be provided")
    _failure(ctx.gh("project", "item-edit", "--id", item["node_id"], "--project-id", p["node_id"],
                    "--clear", ok=None), "field-id must be provided")
    _failure(ctx.gh("project", "item-edit", "--id", item["node_id"], ok=None), "no changes to make")
    assert _value(ctx, p, item, field) is None


@case(command="project item-list", variant="repeated columns, selector exclusions and missing field ID")
def item_list_columns(ctx):
    p = _project(ctx)
    item = _draft(ctx, p)
    first, second = _field(ctx, p), _field(ctx, p)
    expect_ok(ctx.gh("project", "item-edit", "--id", item["node_id"], "--project-id", p["node_id"],
                     "--field-id", first["node_id"], "--text", "first-column"))
    expect_ok(ctx.gh("project", "item-edit", "--id", item["node_id"], "--project-id", p["node_id"],
                     "--field-id", second["node_id"], "--text", "second-column"))
    r = expect_ok(ctx.gh("project", "item-list", p["_number"], "--owner", "@me", "--field", second["name"],
                         "--field", first["name"]))
    assert r.stdout.rstrip("\n").split("\t")[-2:] == ["second-column", "first-column"], r
    r = expect_ok(ctx.gh("project", "item-list", p["_number"], "--owner", "@me", "--field-id", first["node_id"],
                         "--field-id", second["node_id"]))
    assert r.stdout.rstrip("\n").split("\t")[-2:] == ["first-column", "second-column"], r
    _failure(ctx.gh("project", "item-list", p["_number"], "--owner", "@me", "--field", first["name"],
                    "--field-id", second["node_id"], ok=None), "only one of `--field` or `--field-id`")
    _failure(ctx.gh("project", "item-list", p["_number"], "--owner", "@me", "--field", first["name"],
                    "--format", "json", ok=None), "cannot use `--format`")
    _failure(ctx.gh("project", "item-list", p["_number"], "--owner", "@me", "--field-id", ctx.name("invalid"),
                    ok=None), "field")


@case(command="project link", variant="infer repository/owner from cwd and qualified team", needs=("repo",))
def link_inferred(ctx):
    p = _project(ctx)
    expect_ok(ctx.sweep.execute(ctx.account, ["git", "init", "--quiet", str(ctx.directory)]))
    expect_ok(ctx.sweep.execute(ctx.account, ["git", "remote", "add", "origin",
                                             f"https://codeyard-e2e.test/{ctx.repo}.git"]))
    r = expect_ok(ctx.gh("project", "link", p["_number"], cwd=ctx.directory))
    assert r.stdout.strip() == "" and r.stderr.strip() == "", r
    owner, name = ctx.repo.split("/")
    query = ('query { repository(owner:"' + owner + '",name:"' + name + '") '
             '{ projectsV2(first:100) { nodes { id } } } }')
    rows = expect_json(ctx.gh("api", "graphql", "-f", "query=" + query),
                       {"data": {"repository": {"projectsV2": {"nodes": [{"id": p["node_id"]}]}}}})
    assert len(rows["data"]["repository"]["projectsV2"]["nodes"]) == 1, rows
    org = ctx.org()
    team = ctx.team(org)
    p = _project(ctx, org=org)
    r = expect_ok(ctx.gh("project", "link", p["_number"], "--team", f"{org}/{team['slug']}"))
    assert r.stdout.strip() == "" and r.stderr.strip() == "", r


@case(command="project unlink", variant="infer repository/owner from cwd and qualified team", needs=("repo",))
def unlink_inferred(ctx):
    p = _project(ctx)
    expect_ok(ctx.gh("project", "link", p["_number"], "--owner", ctx.account.name, "--repo", ctx.repo))
    expect_ok(ctx.sweep.execute(ctx.account, ["git", "init", "--quiet", str(ctx.directory)]))
    expect_ok(ctx.sweep.execute(ctx.account, ["git", "remote", "add", "origin",
                                             f"https://codeyard-e2e.test/{ctx.repo}.git"]))
    r = expect_ok(ctx.gh("project", "unlink", p["_number"], cwd=ctx.directory))
    assert r.stdout.strip() == "" and r.stderr.strip() == "", r
    owner, name = ctx.repo.split("/")
    query = ('query { repository(owner:"' + owner + '",name:"' + name + '") '
             '{ projectsV2(first:100) { nodes { id } } } }')
    expect_json(ctx.gh("api", "graphql", "-f", "query=" + query),
                {"data": {"repository": {"projectsV2": {"nodes": []}}}})
    org = ctx.org()
    team = ctx.team(org)
    p = _project(ctx, org=org)
    expect_ok(ctx.gh("project", "link", p["_number"], "--owner", org, "--team", team["slug"]))
    r = expect_ok(ctx.gh("project", "unlink", p["_number"], "--team", f"{org}/{team['slug']}"))
    assert r.stdout.strip() == "" and r.stderr.strip() == "", r
