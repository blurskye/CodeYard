"""Three small examples; group lanes supply the exhaustive command variants."""
from gh_sweep.harness import case, expect_json, expect_ok, expect_unsupported


@case(command="repo view", variant="--json nameWithOwner,isPrivate", needs=("repo",))
def repo_view(ctx):
    expect_json(ctx.gh("repo", "view", ctx.repo, "--json", "nameWithOwner,isPrivate"),
                {"nameWithOwner": ctx.repo, "isPrivate": False})


@case(command="issue create", variant="--title --body and issue list --json", needs=("repo",))
def issue_create_and_list(ctx):
    title = ctx.name("issue")
    created = expect_ok(ctx.gh("issue", "create", "--repo", ctx.repo, "--title", title, "--body", "Sweep sample"))
    number = int(created.stdout.strip().rstrip("/").rsplit("/", 1)[1])
    issues = expect_json(ctx.gh("issue", "list", "--repo", ctx.repo, "--json", "number,title,state"),
                         [{"number": int, "title": str, "state": str}])
    assert {"number": number, "title": title, "state": "OPEN"} in issues, issues


@case(command="api", variant="GHES unsupported user/codespaces")
def unsupported_codespaces_endpoint(ctx):
    # Codespaces is a cloud service: the GHES REST route does not exist.
    # Exercise the HTTP failure, not codespace list's client-side GHES rejection.
    expect_unsupported(ctx.gh("api", "user/codespaces", ok=None))
