"""Real-gh release and gist cases; each invocation owns its repositories and gists.

Fixtures use REST only for arrangement/inspection, never as command coverage.
Unsigned releases deliberately exercise the verifier's no-attestation failure; a
Codeyard node signature is not a GitHub/Sigstore release attestation.
"""
import base64
import hashlib
import re
import tarfile
import zipfile

from gh_sweep.harness import case, expect_error, expect_json, expect_ok


def _failure(result, contains):
    """gh sometimes translates an HTTP failure into a command-specific message."""
    assert not result.timed_out and result.rc == 1, result
    assert contains.lower() in result.stderr.lower(), result.stderr
    assert not re.search(r"panic:|panic at|segmentation fault|fatal error:", result.stderr, re.I), result.stderr
    return result


def _denied(result, message=None):
    """GitHub answers another user's write with 403, or hides it behind 404 (gh may reword that as `message`)."""
    assert not result.timed_out and result.rc == 1, result
    assert re.search(r"\bHTTP 40[34]\b", result.stderr) or (message and message in result.stderr), result.stderr
    assert not re.search(r"panic:|panic at|segmentation fault|fatal error:", result.stderr, re.I), result.stderr
    return result


def _other(ctx):
    return next(account for account in ctx.sweep.accounts if account != ctx.account)


def _missing_repo(ctx):
    return ctx.account.name + "/" + ctx.name("missing-repo")


def _missing_gist(ctx):
    return hashlib.md5(ctx.name("missing-gist").encode()).hexdigest()


def _gist(ctx, *, public=True, files=None):
    return ctx.api("POST", "gists", {
        "description": ctx.name("gist"), "public": public,
        "files": files or {"note.txt": {"content": "sweep gist\n"}},
    })


def _release(ctx, **fields):
    return ctx.api("POST", f"repos/{ctx.repo}/releases", {
        "tag_name": ctx.name("tag"), "name": ctx.name("release"),
        "body": "sweep release notes", **fields,
    })


def _tag(ctx, tag=None):
    tag = tag or ctx.name("tag")
    branch = ctx.api("GET", f"repos/{ctx.repo}")["default_branch"]
    sha = ctx.api("GET", f"repos/{ctx.repo}/git/ref/heads/{branch}")["object"]["sha"]
    ctx.api("POST", f"repos/{ctx.repo}/git/refs", {"ref": "refs/tags/" + tag, "sha": sha})
    return tag, sha


def _release_fields(ctx, tag, **fields):
    row = ctx.api("GET", f"repos/{ctx.repo}/releases/tags/{tag}")
    for key, value in fields.items():
        assert row[key] == value, (key, row[key], value)
    return row


@case(command="gist create", variant="public stdin filename and description", serial_group="g5-gist-writes")
def gist_create_public(ctx):
    description = ctx.name("description")
    filename = ctx.name("note") + ".txt"
    result = expect_ok(ctx.gh("gist", "create", "-", "--public", "--desc", description,
                              "--filename", filename, input="public gist contents\n"))
    gist_id = result.stdout.strip().rstrip("/").rsplit("/", 1)[-1]
    row = ctx.api("GET", "gists/" + gist_id)
    assert row["description"] == description and row["public"] is True, row
    assert row["files"][filename]["content"] == "public gist contents\n", row
    assert result.stdout.strip() == row["html_url"], result.stdout


@case(command="gist create", variant="secret multiple files and empty input", serial_group="g5-gist-writes")
def gist_create_secret(ctx):
    first, second = ctx.directory / (ctx.name("one") + ".txt"), ctx.directory / (ctx.name("two") + ".txt")
    first.write_text("one\n")
    second.write_text("two\n")
    result = expect_ok(ctx.gh("gist", "create", str(first), str(second)))
    row = ctx.api("GET", "gists/" + result.stdout.strip().rstrip("/").rsplit("/", 1)[-1])
    assert row["public"] is False, row
    assert set(row["files"]) == {first.name, second.name}, row
    assert row["files"][first.name]["content"] == "one\n", row
    # GitHub refuses a blank file with 422, which gh reports in its own words.
    _failure(ctx.gh("gist", "create", "-", "--filename", ctx.name("empty"), input="", ok=None),
             "a gist file cannot be blank")


@case(command="gist create", variant="missing local file", serial_group="g5-gist-writes")
def gist_create_missing_file(ctx):
    path = ctx.directory / ctx.name("absent")
    _failure(ctx.gh("gist", "create", str(path), ok=None), "no matches found")


@case(command="gist clone", variant="id explicit destination and URL default destination", serial_group="g5-gist-writes")
def gist_clone(ctx):
    row = _gist(ctx)
    destination = ctx.directory / ctx.name("clone")
    expect_ok(ctx.gh("gist", "clone", row["id"], str(destination)))
    assert (destination / "note.txt").read_text() == "sweep gist\n"
    expect_ok(ctx.gh("gist", "clone", row["html_url"], "--", "--depth", "1"))
    assert (ctx.directory / row["id"] / "note.txt").read_text() == "sweep gist\n"


@case(command="gist clone", variant="missing gist and existing destination", serial_group="g5-gist-writes")
def gist_clone_failures(ctx):
    row = _gist(ctx)
    destination = ctx.directory / ctx.name("occupied")
    destination.mkdir()
    (destination / "keep").write_text("keep")
    _failure(ctx.gh("gist", "clone", row["id"], str(destination), ok=None), "already exists")
    # Gist IDs are hexadecimal, not arbitrary fixture labels.
    missing = _missing_gist(ctx)
    _failure(ctx.gh("gist", "clone", missing, str(ctx.directory / ctx.name("clone")), ok=None), "not found")


@case(command="gist delete", variant="id URL repeat and nonowner", serial_group="g5-gist-writes")
def gist_delete(ctx):
    row = _gist(ctx)
    _denied(ctx.sweep.gh(_other(ctx), "gist", "delete", row["id"], "--yes", ok=None), "not owned by you")
    expect_ok(ctx.gh("gist", "delete", row["id"], "--yes"))
    # gh turns a gist 404 into its own "not found" message.
    _failure(ctx.gh("gist", "view", row["id"], "--raw", ok=None), "not found")
    _failure(ctx.gh("gist", "delete", row["id"], "--yes", ok=None), "not found")
    second = _gist(ctx, public=False)
    expect_ok(ctx.gh("gist", "delete", second["html_url"], "--yes"))
    _failure(ctx.gh("gist", "view", second["id"], "--raw", ok=None), "not found")


@case(command="gist edit", variant="replace file add remove and description", serial_group="g5-gist-writes")
def gist_edit(ctx):
    row = _gist(ctx)
    replacement = ctx.directory / ctx.name("replacement")
    replacement.write_text("replacement contents\n")
    description = ctx.name("edited-description")
    expect_ok(ctx.gh("gist", "edit", row["id"], str(replacement), "--filename", "note.txt", "--desc", description))
    changed = ctx.api("GET", "gists/" + row["id"])
    assert changed["description"] == description, changed
    assert changed["files"]["note.txt"]["content"] == "replacement contents\n", changed
    extra = ctx.directory / (ctx.name("extra") + ".txt")
    extra.write_text("extra contents\n")
    expect_ok(ctx.gh("gist", "edit", row["html_url"], "--add", str(extra)))
    assert ctx.api("GET", "gists/" + row["id"])["files"][extra.name]["content"] == "extra contents\n"
    expect_ok(ctx.gh("gist", "edit", row["id"], "--remove", extra.name))
    assert set(ctx.api("GET", "gists/" + row["id"])["files"]) == {"note.txt"}
    # Without a source file gh opens the editor, so pass the unchanged content to change only the description.
    description = ctx.name("description-only")
    expect_ok(ctx.gh("gist", "edit", row["id"], str(replacement), "--filename", "note.txt", "--desc", description))
    assert ctx.api("GET", "gists/" + row["id"])["description"] == description


@case(command="gist edit", variant="nonowner missing gist missing file and last file", serial_group="g5-gist-writes")
def gist_edit_failures(ctx):
    row = _gist(ctx)
    # gh checks the owner itself before it sends the update.
    _failure(ctx.sweep.gh(_other(ctx), "gist", "edit", row["id"], "--desc", ctx.name("denied"), ok=None),
             "you do not own this gist")
    _failure(ctx.gh("gist", "edit", _missing_gist(ctx), "--desc", "absent", ok=None), "gist not found")
    replacement = ctx.directory / ctx.name("replacement")
    replacement.write_text("replacement\n")
    _failure(ctx.gh("gist", "edit", row["id"], str(replacement), "--filename", ctx.name("absent"), ok=None), "file")
    # A gist may not be left with zero files.
    expect_error(ctx.gh("gist", "edit", row["id"], "--remove", "note.txt", ok=None), status=422)
    assert ctx.api("GET", "gists/" + row["id"])["files"]["note.txt"]["content"] == "sweep gist\n"


@case(command="gist list", variant="public secret filter and content search", serial_group="g5-gist-writes")
def gist_list(ctx):
    marker = ctx.name("search")
    public = _gist(ctx, files={marker + ".txt": {"content": "public body\n"}})
    secret = _gist(ctx, public=False, files={"secret.txt": {"content": marker + " hidden content\n"}})
    # The account is shared with other cases: never assume its entire gist list is ours.
    listed = expect_ok(ctx.gh("gist", "list", "--limit", "1000", "--filter", marker))
    assert public["id"] in listed.stdout and secret["id"] not in listed.stdout, listed.stdout
    public_only = expect_ok(ctx.gh("gist", "list", "--public", "--limit", "1000", "--filter", marker))
    assert public["id"] in public_only.stdout and secret["id"] not in public_only.stdout, public_only.stdout
    secret_only = expect_ok(ctx.gh("gist", "list", "--secret", "--limit", "1000", "--filter", marker, "--include-content"))
    assert secret["id"] in secret_only.stdout and public["id"] not in secret_only.stdout, secret_only.stdout
    assert marker + " hidden content" in secret_only.stdout, secret_only.stdout
    both = expect_ok(ctx.gh("gist", "list", "--limit", "1000", "--filter", marker, "--include-content"))
    assert public["id"] in both.stdout and secret["id"] in both.stdout, both.stdout
    limited = expect_ok(ctx.gh("gist", "list", "--limit", "1"))
    assert len(limited.stdout.strip().splitlines()) == 1, limited.stdout


@case(command="gist list", variant="invalid regex public with secret and limit", serial_group="g5-gist-writes")
def gist_list_bad_input(ctx):
    _failure(ctx.gh("gist", "list", "--filter", "[", ok=None), "regexp")
    # gh accepts both visibility flags and lets --secret win.
    expect_ok(ctx.gh("gist", "list", "--public", "--secret", "--limit", "1"))
    _failure(ctx.gh("gist", "list", "--limit", "0", ok=None), "limit")


@case(command="gist rename", variant="success URL missing source collision and nonowner", serial_group="g5-gist-writes")
def gist_rename(ctx):
    row = _gist(ctx, files={"old.txt": {"content": "old\n"}, "other.txt": {"content": "other\n"}})
    new_name = ctx.name("renamed") + ".txt"
    expect_ok(ctx.gh("gist", "rename", row["html_url"], "old.txt", new_name))
    changed = ctx.api("GET", "gists/" + row["id"])
    assert "old.txt" not in changed["files"] and changed["files"][new_name]["content"] == "old\n", changed
    _failure(ctx.gh("gist", "rename", row["id"], ctx.name("absent"), "new.txt", ok=None), "file")
    _failure(ctx.gh("gist", "rename", row["id"], new_name, "other.txt", ok=None), "already exists")
    _failure(ctx.sweep.gh(_other(ctx), "gist", "rename", row["id"], new_name, "denied.txt", ok=None),
             "you do not own this gist")
    _failure(ctx.gh("gist", "rename", _missing_gist(ctx), "old.txt", "new.txt", ok=None), "gist not found")


@case(command="gist view", variant="default raw filename and files", serial_group="g5-gist-writes")
def gist_view(ctx):
    row = _gist(ctx, files={"note.txt": {"content": "sweep gist\n"}, "second.txt": {"content": "second body\n"}})
    plain = expect_ok(ctx.gh("gist", "view", row["id"]))
    assert "sweep gist" in plain.stdout and "second body" in plain.stdout, plain.stdout
    raw = expect_ok(ctx.gh("gist", "view", row["html_url"], "--raw", "--filename", "note.txt"))
    assert raw.stdout == "sweep gist\n", raw.stdout
    files = expect_ok(ctx.gh("gist", "view", row["id"], "--files"))
    assert set(files.stdout.splitlines()) == {"note.txt", "second.txt"}, files.stdout
    all_raw = expect_ok(ctx.gh("gist", "view", row["id"], "--raw"))
    assert "sweep gist" in all_raw.stdout and "second body" in all_raw.stdout, all_raw.stdout
    # Secret gists are unlisted, not private: another account can read a known ID.
    secret = _gist(ctx, public=False)
    shared = expect_ok(ctx.sweep.gh(_other(ctx), "gist", "view", secret["id"], "--raw", "--filename", "note.txt"))
    assert shared.stdout == "sweep gist\n", shared.stdout


@case(command="gist view", variant="terminal escape sanitization", serial_group="g5-gist-writes")
def gist_view_escapes(ctx):
    content = "\x1b[31msweep red\x1b[0m\n"
    row = _gist(ctx, files={"escape.txt": {"content": content}})
    safe = expect_ok(ctx.gh("gist", "view", row["id"], "--raw", "--filename", "escape.txt"))
    assert "sweep red" in safe.stdout and "\x1b" not in safe.stdout, safe.stdout
    raw = expect_ok(ctx.gh("gist", "view", row["id"], "--raw", "--filename", "escape.txt", "--allow-escape-sequences"))
    # gh's HTTP client rewrites control characters in JSON responses to caret notation, so the
    # escape byte never reaches the command, even with --allow-escape-sequences.
    assert raw.stdout == content.replace("\x1b", "^["), repr(raw.stdout)


@case(command="gist view", variant="missing gist and filename", serial_group="g5-gist-writes")
def gist_view_failures(ctx):
    row = _gist(ctx)
    _failure(ctx.gh("gist", "view", _missing_gist(ctx), "--raw", ok=None), "not found")
    _failure(ctx.gh("gist", "view", row["id"], "--filename", ctx.name("absent"), ok=None), "file")


@case(command="release create", variant="published title notes target latest and initial asset", needs=("repo",))
def release_create(ctx):
    tag, title, notes = ctx.name("tag"), ctx.name("title"), ctx.name("notes")
    branch = ctx.api("GET", f"repos/{ctx.repo}")["default_branch"]
    asset = ctx.directory / (ctx.name("asset") + ".txt")
    asset.write_text("initial asset\n")
    created = expect_ok(ctx.gh("release", "create", tag, str(asset) + "#Display label", "--repo", ctx.repo,
                               "--title", title, "--notes", notes, "--target", branch, "--latest"))
    row = _release_fields(ctx, tag, name=title, body=notes, draft=False, prerelease=False)
    assert created.stdout.strip() == row["html_url"], created.stdout
    assert [(a["name"], a["label"], a["size"]) for a in row["assets"]] == [(asset.name, "Display label", 14)], row
    assert ctx.api("GET", f"repos/{ctx.repo}/releases/latest")["tag_name"] == tag
    assert ctx.api("GET", f"repos/{ctx.repo}/git/ref/tags/{tag}")["object"]["sha"] == ctx.api(
        "GET", f"repos/{ctx.repo}/git/ref/heads/{branch}")["object"]["sha"]


@case(command="release create", variant="draft prerelease notes file and latest false", needs=("repo",))
def release_create_draft(ctx):
    tag = ctx.name("draft")
    notes = ctx.directory / ctx.name("notes")
    notes.write_text("file notes\nsecond line\n")
    expect_ok(ctx.gh("release", "create", tag, "--repo", ctx.repo, "--draft", "--prerelease",
                     "--notes-file", str(notes), "--latest=false", "--title", tag))
    _release_fields(ctx, tag, draft=True, prerelease=True, body=notes.read_text())
    assert ctx.api("GET", f"repos/{ctx.repo}/git/matching-refs/tags/{tag}") == []
    stdin_tag = ctx.name("stdin")
    expect_ok(ctx.gh("release", "create", stdin_tag, "--repo", ctx.repo, "--notes-file", "-", input="stdin notes\n"))
    _release_fields(ctx, stdin_tag, body="stdin notes\n", draft=False)


@case(command="release create", variant="generated notes start tag and preface", needs=("repo",))
def release_create_generate(ctx):
    previous = _release(ctx)
    tag = ctx.name("generated")
    preface = ctx.name("preface")
    expect_ok(ctx.gh("release", "create", tag, "--repo", ctx.repo, "--generate-notes",
                     "--notes-start-tag", previous["tag_name"], "--notes", preface, "--latest=false"))
    row = _release_fields(ctx, tag, name=tag, draft=False)
    assert row["body"].startswith(preface), row
    assert "Full Changelog" in row["body"] and previous["tag_name"] in row["body"] and tag in row["body"], row
    assert ctx.api("GET", f"repos/{ctx.repo}/releases/latest")["id"] == previous["id"]


@case(command="release create", variant="notes from annotated local tag", needs=("repo",))
def release_create_tag_notes(ctx):
    tag, sha = _tag(ctx)
    checkout = ctx.directory / ctx.name("checkout")
    git = ctx.sweep.execute
    expect_ok(git(ctx.account, ["git", "clone", f"https://codeyard-e2e.test/{ctx.repo}.git", str(checkout)]))
    expect_ok(git(ctx.account, ["git", "-c", "user.name=Sweep", "-c", "user.email=sweep@example.test",
                               "tag", "-f", "-a", tag, sha, "-m", "Annotated sweep notes"], cwd=checkout))
    # gh forbids --notes-from-tag with --repo: infer the repository from this remote.
    expect_ok(ctx.gh("release", "create", tag, "--notes-from-tag", "--verify-tag", cwd=checkout))
    row = _release_fields(ctx, tag, draft=False)
    assert "Annotated sweep notes" in row["body"], row


@case(command="release create", variant="lightweight tag notes and incompatible repo flag", needs=("repo",))
def release_create_lightweight_notes(ctx):
    tag, sha = _tag(ctx)
    checkout = ctx.directory / ctx.name("checkout")
    expect_ok(ctx.sweep.execute(ctx.account, ["git", "clone", f"https://codeyard-e2e.test/{ctx.repo}.git", str(checkout)]))
    commit = ctx.api("GET", f"repos/{ctx.repo}/commits/{sha}")
    expect_ok(ctx.gh("release", "create", tag, "--notes-from-tag", cwd=checkout))
    row = _release_fields(ctx, tag)
    assert row["body"].strip() == commit["commit"]["message"].strip(), row
    _failure(ctx.gh("release", "create", ctx.name("tag"), "--notes-from-tag", "--repo", ctx.repo, ok=None),
             "using `--notes-from-tag` with `--repo` is not supported")


@case(command="release create", variant="verify existing and missing remote tag", needs=("repo",))
def release_create_verify(ctx):
    tag, _ = _tag(ctx)
    expect_ok(ctx.gh("release", "create", tag, "--repo", ctx.repo, "--verify-tag", "--notes", "existing tag"))
    _release_fields(ctx, tag, body="existing tag")
    missing = ctx.name("missing-tag")
    _failure(ctx.gh("release", "create", missing, "--repo", ctx.repo, "--verify-tag", "--notes", "absent", ok=None), "--verify-tag")
    assert ctx.api("GET", f"repos/{ctx.repo}/git/matching-refs/tags/{missing}") == []


@case(command="release create", variant="fail on no commits first release and unchanged release", needs=("repo",))
def release_create_no_commits(ctx):
    tag = ctx.name("first")
    expect_ok(ctx.gh("release", "create", tag, "--repo", ctx.repo, "--fail-on-no-commits", "--notes", "first"))
    _release_fields(ctx, tag, body="first")
    second = ctx.name("no-commits")
    _failure(ctx.gh("release", "create", second, "--repo", ctx.repo, "--fail-on-no-commits", "--notes", "second", ok=None),
             "no new commits since the last release")
    assert all(r["tag_name"] != second for r in ctx.api("GET", f"repos/{ctx.repo}/releases"))
    ctx.api("PUT", f"repos/{ctx.repo}/contents/{ctx.name('change')}.txt",
            {"message": "New release commit", "content": base64.b64encode(b"new commit\n").decode()})
    third = ctx.name("new-commits")
    expect_ok(ctx.gh("release", "create", third, "--repo", ctx.repo, "--fail-on-no-commits", "--notes", "new commits"))
    _release_fields(ctx, third, body="new commits")


@case(command="release create", variant="bad discussion category", needs=("repo",))
def release_create_discussion(ctx):
    # GHES must reject an unknown category rather than silently discard the flag.
    ctx.api("PATCH", f"repos/{ctx.repo}", {"has_discussions": True})
    expect_error(ctx.gh("release", "create", ctx.name("discussion"), "--repo", ctx.repo,
                        "--discussion-category", ctx.name("absent-category"), "--notes", "discussion", ok=None), status=422)


@case(command="release create", variant="start discussion in existing category", needs=("repo",))
def release_create_discussion_success(ctx):
    ctx.api("PATCH", f"repos/{ctx.repo}", {"has_discussions": True})
    tag = ctx.name("discussion")
    expect_ok(ctx.gh("release", "create", tag, "--repo", ctx.repo, "--discussion-category", "General", "--notes", "Discuss this release"))
    row = _release_fields(ctx, tag)
    assert row.get("discussion_url", "").startswith("http"), row
    assert "/discussions/" in row["discussion_url"], row


@case(command="release create", variant="duplicate bad tag missing repo and denied writer", needs=("release",))
def release_create_failures(ctx):
    _failure(ctx.gh("release", "create", ctx.release["tag_name"], "--repo", ctx.repo, "--notes", "duplicate", ok=None), "already exists")
    expect_error(ctx.gh("release", "create", ctx.name("invalid") + "..bad", "--repo", ctx.repo, "--notes", "bad", ok=None), status=422)
    expect_error(ctx.gh("release", "create", ctx.name("tag"), "--repo", _missing_repo(ctx), "--notes", "missing", ok=None), contains="Not Found")
    _denied(ctx.sweep.gh(_other(ctx), "release", "create", ctx.name("tag"), "--repo", ctx.repo,
                         "--notes", "denied", ok=None))


@case(command="release delete", variant="keep tag cleanup tag and draft", needs=("repo",))
def release_delete(ctx):
    row = _release(ctx)
    expect_ok(ctx.gh("release", "delete", row["tag_name"], "--repo", ctx.repo, "--yes"))
    assert all(r["id"] != row["id"] for r in ctx.api("GET", f"repos/{ctx.repo}/releases"))
    assert ctx.api("GET", f"repos/{ctx.repo}/git/ref/tags/{row['tag_name']}")["ref"] == "refs/tags/" + row["tag_name"]
    second = _release(ctx)
    expect_ok(ctx.gh("release", "delete", second["tag_name"], "--repo", ctx.repo, "--yes", "--cleanup-tag"))
    assert ctx.api("GET", f"repos/{ctx.repo}/git/matching-refs/tags/{second['tag_name']}") == []
    draft = _release(ctx, draft=True)
    expect_ok(ctx.gh("release", "delete", draft["tag_name"], "--repo", ctx.repo, "--yes"))
    assert all(r["id"] != draft["id"] for r in ctx.api("GET", f"repos/{ctx.repo}/releases"))


@case(command="release delete", variant="missing tag repo and denied writer", needs=("release",))
def release_delete_failures(ctx):
    _failure(ctx.gh("release", "delete", ctx.name("missing-tag"), "--repo", ctx.repo, "--yes", ok=None), "release not found")
    _failure(ctx.gh("release", "delete", ctx.name("tag"), "--repo", _missing_repo(ctx), "--yes", ok=None), "release not found")
    _denied(ctx.sweep.gh(_other(ctx), "release", "delete", ctx.release["tag_name"], "--repo", ctx.repo, "--yes", ok=None))
    _release_fields(ctx, ctx.release["tag_name"], id=ctx.release["id"])


@case(command="release upload", variant="new asset label duplicate and clobber", needs=("release",))
def release_upload(ctx):
    tag = ctx.release["tag_name"]
    asset = ctx.directory / (ctx.name("asset") + ".txt")
    asset.write_text("original\n")
    expect_ok(ctx.gh("release", "upload", tag, str(asset) + "#Sweep label", "--repo", ctx.repo))
    row = _release_fields(ctx, tag)
    assert [(a["name"], a["label"], a["size"]) for a in row["assets"]] == [(asset.name, "Sweep label", 9)], row
    _failure(ctx.gh("release", "upload", tag, str(asset), "--repo", ctx.repo, ok=None), "already exists")
    asset.write_text("replacement contents\n")
    expect_ok(ctx.gh("release", "upload", tag, str(asset), "--repo", ctx.repo, "--clobber"))
    row = _release_fields(ctx, tag)
    assert len(row["assets"]) == 1 and row["assets"][0]["size"] == len(asset.read_bytes()), row
    result = expect_ok(ctx.gh("release", "download", tag, "--repo", ctx.repo, "--pattern", asset.name, "--output", "-"))
    assert result.stdout == asset.read_text(), result.stdout


@case(command="release upload", variant="missing release repo local file and denied writer", needs=("release",))
def release_upload_failures(ctx):
    asset = ctx.directory / (ctx.name("asset") + ".txt")
    asset.write_text("asset\n")
    _failure(ctx.gh("release", "upload", ctx.name("missing"), str(asset), "--repo", ctx.repo, ok=None), "release not found")
    _failure(ctx.gh("release", "upload", ctx.name("missing"), str(asset), "--repo", _missing_repo(ctx), ok=None), "release not found")
    _failure(ctx.gh("release", "upload", ctx.release["tag_name"], str(ctx.directory / ctx.name("absent")), "--repo", ctx.repo, ok=None), "no matches found")
    _denied(ctx.sweep.gh(_other(ctx), "release", "upload", ctx.release["tag_name"], str(asset), "--repo", ctx.repo, ok=None))
    assert _release_fields(ctx, ctx.release["tag_name"])["assets"] == []


@case(command="release delete-asset", variant="success missing asset release repo and nonowner", needs=("release",))
def release_delete_asset(ctx):
    tag = ctx.release["tag_name"]
    asset = ctx.directory / (ctx.name("asset") + ".txt")
    asset.write_text("asset to delete\n")
    expect_ok(ctx.gh("release", "upload", tag, str(asset), "--repo", ctx.repo))
    _denied(ctx.sweep.gh(_other(ctx), "release", "delete-asset", tag, asset.name, "--repo", ctx.repo, "--yes", ok=None))
    expect_ok(ctx.gh("release", "delete-asset", tag, asset.name, "--repo", ctx.repo, "--yes"))
    assert _release_fields(ctx, tag)["assets"] == []
    _failure(ctx.gh("release", "delete-asset", tag, asset.name, "--repo", ctx.repo, "--yes", ok=None), "not found")
    _failure(ctx.gh("release", "delete-asset", ctx.name("missing"), asset.name, "--repo", ctx.repo, "--yes", ok=None), "release not found")
    _failure(ctx.gh("release", "delete-asset", tag, asset.name, "--repo", _missing_repo(ctx), "--yes", ok=None), "release not found")


@case(command="release download", variant="all assets patterns latest directory output and overwrite policies", needs=("release",))
def release_download(ctx):
    tag = ctx.release["tag_name"]
    first, second = ctx.directory / (ctx.name("one") + ".txt"), ctx.directory / (ctx.name("two") + ".bin")
    first.write_text("first asset\n")
    second.write_text("second asset\n")
    expect_ok(ctx.gh("release", "upload", tag, str(first), str(second), "--repo", ctx.repo))
    target = ctx.directory / ctx.name("downloads")
    expect_ok(ctx.gh("release", "download", tag, "--repo", ctx.repo, "--dir", str(target)))
    assert (target / first.name).read_text() == first.read_text()
    assert (target / second.name).read_text() == second.read_text()
    (target / first.name).write_text("local sentinel\n")
    _failure(ctx.gh("release", "download", tag, "--repo", ctx.repo, "--pattern", first.name, "--dir", str(target), ok=None), "already exists")
    expect_ok(ctx.gh("release", "download", tag, "--repo", ctx.repo, "--pattern", first.name, "--dir", str(target), "--skip-existing"))
    assert (target / first.name).read_text() == "local sentinel\n"
    expect_ok(ctx.gh("release", "download", tag, "--repo", ctx.repo, "--pattern", first.name, "--dir", str(target), "--clobber"))
    assert (target / first.name).read_text() == first.read_text()
    output = ctx.directory / ctx.name("output")
    expect_ok(ctx.gh("release", "download", "--repo", ctx.repo, "--pattern", "*.txt", "--output", str(output)))
    assert output.read_text() == first.read_text()
    patterns = ctx.directory / ctx.name("patterns")
    expect_ok(ctx.gh("release", "download", tag, "--repo", ctx.repo, "--pattern", "*.txt", "--pattern", "*.bin", "--dir", str(patterns)))
    assert {p.name for p in patterns.iterdir()} == {first.name, second.name}


@case(command="release download", variant="stdout terminal escape handling", needs=("release",))
def release_download_escapes(ctx):
    asset = ctx.directory / (ctx.name("escape") + ".txt")
    content = "\x1b[31msweep asset\x1b[0m\n"
    asset.write_text(content)
    tag = ctx.release["tag_name"]
    expect_ok(ctx.gh("release", "upload", tag, str(asset), "--repo", ctx.repo))
    # Piped standard output refuses escape sequences instead of rewriting them.
    _failure(ctx.gh("release", "download", tag, "--repo", ctx.repo, "--pattern", asset.name, "--output", "-", ok=None),
             "terminal escape sequences")
    raw = expect_ok(ctx.gh("release", "download", tag, "--repo", ctx.repo, "--pattern", asset.name,
                          "--output", "-", "--allow-escape-sequences"))
    assert raw.stdout == content, repr(raw.stdout)


@case(command="release download", variant="zip and tar source archives", needs=("release",))
def release_download_archives(ctx):
    tag = ctx.release["tag_name"]
    zip_path = ctx.directory / (ctx.name("source") + ".zip")
    tar_path = ctx.directory / (ctx.name("source") + ".tar.gz")
    expect_ok(ctx.gh("release", "download", tag, "--repo", ctx.repo, "--archive", "zip", "--output", str(zip_path)))
    with zipfile.ZipFile(zip_path) as archive:
        assert any(name.endswith("README.md") for name in archive.namelist()), archive.namelist()
    expect_ok(ctx.gh("release", "download", tag, "--repo", ctx.repo, "--archive", "tar.gz", "--output", str(tar_path)))
    with tarfile.open(tar_path, "r:gz") as archive:
        assert any(name.endswith("README.md") for name in archive.getnames()), archive.getnames()


@case(command="release download", variant="missing release repo asset and incompatible flags", needs=("release",))
def release_download_failures(ctx):
    _failure(ctx.gh("release", "download", ctx.name("missing"), "--repo", ctx.repo, ok=None), "release not found")
    _failure(ctx.gh("release", "download", ctx.name("missing"), "--repo", _missing_repo(ctx), ok=None), "release not found")
    _failure(ctx.gh("release", "download", ctx.release["tag_name"], "--repo", ctx.repo, "--pattern", ctx.name("absent"), ok=None), "no assets")
    _failure(ctx.gh("release", "download", ctx.release["tag_name"], "--repo", ctx.repo, "--clobber", "--skip-existing", ok=None), "specify only one")
    _failure(ctx.gh("release", "download", ctx.release["tag_name"], "--repo", ctx.repo, "--archive", "rar", ok=None), "must be one of")


@case(command="release edit", variant="draft metadata retag target verify publish prerelease and latest", needs=("repo",))
def release_edit(ctx):
    row = _release(ctx, draft=True)
    tag = row["tag_name"]
    new_tag, sha = _tag(ctx)
    title, notes = ctx.name("edited-title"), ctx.name("edited-notes")
    expect_ok(ctx.gh("release", "edit", tag, "--repo", ctx.repo, "--draft", "--prerelease", "--title", title,
                     "--notes", notes, "--tag", new_tag, "--target", sha, "--verify-tag", "--latest=false"))
    _release_fields(ctx, new_tag, name=title, body=notes, draft=True, prerelease=True, target_commitish=sha)
    file = ctx.directory / ctx.name("notes")
    file.write_text("edited notes from file\n")
    expect_ok(ctx.gh("release", "edit", new_tag, "--repo", ctx.repo, "--notes-file", str(file), "--draft=false",
                     "--prerelease=false", "--latest"))
    _release_fields(ctx, new_tag, body=file.read_text(), draft=False, prerelease=False)
    assert ctx.api("GET", f"repos/{ctx.repo}/releases/latest")["tag_name"] == new_tag
    expect_ok(ctx.gh("release", "edit", new_tag, "--repo", ctx.repo, "--notes-file", "-", input="stdin edited notes\n"))
    _release_fields(ctx, new_tag, body="stdin edited notes\n")


@case(command="release edit", variant="discussion category when publishing draft", needs=("repo",))
def release_edit_discussion(ctx):
    ctx.api("PATCH", f"repos/{ctx.repo}", {"has_discussions": True})
    row = _release(ctx, draft=True)
    expect_error(ctx.gh("release", "edit", row["tag_name"], "--repo", ctx.repo, "--draft=false",
                        "--discussion-category", ctx.name("absent-category"), ok=None), status=422)
    _release_fields(ctx, row["tag_name"], draft=True)


@case(command="release edit", variant="publish draft with discussion in existing category", needs=("repo",))
def release_edit_discussion_success(ctx):
    ctx.api("PATCH", f"repos/{ctx.repo}", {"has_discussions": True})
    row = _release(ctx, draft=True)
    expect_ok(ctx.gh("release", "edit", row["tag_name"], "--repo", ctx.repo, "--draft=false", "--discussion-category", "General"))
    changed = _release_fields(ctx, row["tag_name"], draft=False)
    assert "/discussions/" in changed.get("discussion_url", ""), changed


@case(command="release edit", variant="missing release repo tag invalid retag and denied writer", needs=("release",))
def release_edit_failures(ctx):
    tag = ctx.release["tag_name"]
    _failure(ctx.gh("release", "edit", ctx.name("missing"), "--repo", ctx.repo, "--title", "absent", ok=None), "release not found")
    _failure(ctx.gh("release", "edit", tag, "--repo", _missing_repo(ctx), "--title", "absent", ok=None), "release not found")
    _failure(ctx.gh("release", "edit", tag, "--repo", ctx.repo, "--tag", ctx.name("missing-tag"), "--verify-tag", ok=None), "--verify-tag")
    expect_error(ctx.gh("release", "edit", tag, "--repo", ctx.repo, "--tag", ctx.name("invalid") + "..bad", ok=None), status=422)
    _denied(ctx.sweep.gh(_other(ctx), "release", "edit", tag, "--repo", ctx.repo, "--notes", "denied", ok=None))
    _release_fields(ctx, tag, body=ctx.release["body"])


@case(command="release list", variant="JSON table order pagination and both exclusions", needs=("repo",))
def release_list(ctx):
    published = _release(ctx)
    draft = _release(ctx, draft=True)
    prerelease = _release(ctx, prerelease=True)
    rows = expect_json(ctx.gh("release", "list", "--repo", ctx.repo, "--json", "tagName,name,isDraft,isPrerelease,isLatest",
                              "--limit", "100", "--order", "asc"),
                       [{"tagName": str, "name": str, "isDraft": bool, "isPrerelease": bool, "isLatest": bool}])
    assert {r["tagName"] for r in rows} == {published["tag_name"], draft["tag_name"], prerelease["tag_name"]}, rows
    assert next(r for r in rows if r["tagName"] == draft["tag_name"])["isDraft"] is True
    descending = expect_json(ctx.gh("release", "list", "--repo", ctx.repo, "--json", "tagName", "--order", "desc"), [{"tagName": str}])
    assert [r["tagName"] for r in descending] == [r["tagName"] for r in reversed(rows)], (rows, descending)
    no_drafts = expect_json(ctx.gh("release", "list", "--repo", ctx.repo, "--exclude-drafts", "--json", "tagName"), [{"tagName": str}])
    assert {r["tagName"] for r in no_drafts} == {published["tag_name"], prerelease["tag_name"]}, no_drafts
    no_pre = expect_json(ctx.gh("release", "list", "--repo", ctx.repo, "--exclude-pre-releases", "--json", "tagName"), [{"tagName": str}])
    assert {r["tagName"] for r in no_pre} == {published["tag_name"], draft["tag_name"]}, no_pre
    stable = expect_json(ctx.gh("release", "list", "--repo", ctx.repo, "--exclude-drafts", "--exclude-pre-releases", "--json", "tagName"), [{"tagName": str}])
    assert stable == [{"tagName": published["tag_name"]}], stable
    limited = expect_json(ctx.gh("release", "list", "--repo", ctx.repo, "--limit", "1", "--json", "tagName"), [{"tagName": str}])
    assert len(limited) == 1 and limited == descending[:1], limited
    table = expect_ok(ctx.gh("release", "list", "--repo", ctx.repo))
    assert all(row["tag_name"] in table.stdout for row in (published, draft, prerelease)), table.stdout
    reader = expect_json(ctx.sweep.gh(_other(ctx), "release", "list", "--repo", ctx.repo, "--json", "tagName"), [{"tagName": str}])
    assert {r["tagName"] for r in reader} == {published["tag_name"], prerelease["tag_name"]}, reader


@case(command="release list", variant="jq template empty and bad input", needs=("repo",))
def release_list_formatting(ctx):
    assert expect_json(ctx.gh("release", "list", "--repo", ctx.repo, "--json", "tagName"), []) == []
    row = _release(ctx)
    jq = expect_ok(ctx.gh("release", "list", "--repo", ctx.repo, "--json", "tagName", "--jq", ".[0].tagName"))
    assert jq.stdout.strip() == row["tag_name"], jq.stdout
    template = expect_ok(ctx.gh("release", "list", "--repo", ctx.repo, "--json", "tagName", "--template", "{{range .}}{{.tagName}}{{end}}"))
    assert template.stdout.strip() == row["tag_name"], template.stdout
    _failure(ctx.gh("release", "list", "--repo", _missing_repo(ctx), "--json", "tagName", ok=None), "Could not resolve to a Repository")
    _failure(ctx.gh("release", "list", "--repo", ctx.repo, "--order", "sideways", ok=None), "invalid")
    _failure(ctx.gh("release", "list", "--repo", ctx.repo, "--limit", "0", ok=None), "limit")


@case(command="release list", variant="private repository hidden from second account")
def release_list_private(ctx):
    private = ctx.sweep.create_repo(ctx.account, ctx.name("private"), private=True)["full_name"]
    ctx.api("POST", f"repos/{private}/releases", {"tag_name": ctx.name("tag"), "name": "Private release"})
    _failure(ctx.sweep.gh(_other(ctx), "release", "list", "--repo", private, "--json", "tagName", ok=None),
             "Could not resolve to a Repository")


@case(command="release view", variant="tag latest JSON jq template and rendered text", needs=("release",))
def release_view(ctx):
    tag = ctx.release["tag_name"]
    # The fixture has no notes: the API returns a null body and gh prints it as "".
    body = ctx.release["body"] or ""
    row = expect_json(ctx.gh("release", "view", tag, "--repo", ctx.repo, "--json", "tagName,name,body,isDraft,isPrerelease,assets"),
                      {"tagName": tag, "name": ctx.release["name"], "body": body,
                       "isDraft": False, "isPrerelease": False, "assets": []})
    assert row["tagName"] == tag
    latest = expect_json(ctx.gh("release", "view", "--repo", ctx.repo, "--json", "tagName"), {"tagName": tag})
    assert latest == {"tagName": tag}
    jq = expect_ok(ctx.gh("release", "view", tag, "--repo", ctx.repo, "--json", "tagName", "--jq", ".tagName"))
    assert jq.stdout.strip() == tag, jq.stdout
    template = expect_ok(ctx.gh("release", "view", tag, "--repo", ctx.repo, "--json", "name,tagName", "--template", "{{.name}}:{{.tagName}}"))
    assert template.stdout.strip() == ctx.release["name"] + ":" + tag, template.stdout
    text = expect_ok(ctx.gh("release", "view", tag, "--repo", ctx.repo))
    assert ctx.release["name"] in text.stdout and body in text.stdout, text.stdout


@case(command="release view", variant="missing repo tag and draft visibility", needs=("repo",))
def release_view_failures(ctx):
    _failure(ctx.gh("release", "view", ctx.name("missing"), "--repo", ctx.repo, ok=None), "release not found")
    _failure(ctx.gh("release", "view", ctx.name("missing"), "--repo", _missing_repo(ctx), ok=None), "release not found")
    draft = _release(ctx, draft=True)
    expect_json(ctx.gh("release", "view", draft["tag_name"], "--repo", ctx.repo, "--json", "isDraft"), {"isDraft": True})
    _failure(ctx.sweep.gh(_other(ctx), "release", "view", draft["tag_name"], "--repo", ctx.repo, ok=None), "release not found")


@case(command="release view", variant="private repository visibility")
def release_view_private(ctx):
    private = ctx.sweep.create_repo(ctx.account, ctx.name("private"), private=True)["full_name"]
    tag = ctx.name("tag")
    ctx.api("POST", f"repos/{private}/releases", {"tag_name": tag, "name": "Private release"})
    expect_json(ctx.gh("release", "view", tag, "--repo", private, "--json", "tagName"), {"tagName": tag})
    _failure(ctx.sweep.gh(_other(ctx), "release", "view", tag, "--repo", private, ok=None), "release not found")


@case(command="release download", variant="private repository visibility")
def release_download_private(ctx):
    private = ctx.sweep.create_repo(ctx.account, ctx.name("private"), private=True)["full_name"]
    tag = ctx.name("tag")
    ctx.api("POST", f"repos/{private}/releases", {"tag_name": tag, "name": "Private release"})
    asset = ctx.directory / (ctx.name("private-asset") + ".txt")
    asset.write_text("private bytes\n")
    expect_ok(ctx.gh("release", "upload", tag, str(asset), "--repo", private))
    owner = expect_ok(ctx.gh("release", "download", tag, "--repo", private, "--pattern", asset.name, "--output", "-"))
    assert owner.stdout == "private bytes\n", owner.stdout
    _failure(ctx.sweep.gh(_other(ctx), "release", "download", tag, "--repo", private, "--pattern", asset.name, "--output", "-", ok=None),
             "release not found")


@case(command="release verify", variant="unsigned release default JSON jq template and latest", needs=("release",))
def release_verify(ctx):
    tag = ctx.release["tag_name"]
    _failure(ctx.gh("release", "verify", tag, "--repo", ctx.repo, ok=None), "no attestations")
    _failure(ctx.gh("release", "verify", tag, "--repo", ctx.repo, "--format", "json", ok=None), "no attestations")
    _failure(ctx.gh("release", "verify", tag, "--repo", ctx.repo, "--format", "json", "--jq", ".", ok=None), "no attestations")
    _failure(ctx.gh("release", "verify", tag, "--repo", ctx.repo, "--format", "json", "--template", "{{.}}", ok=None), "no attestations")
    _failure(ctx.gh("release", "verify", "--repo", ctx.repo, ok=None), "no attestations")


@case(command="release verify", variant="missing tag repo and invalid format", needs=("repo",))
def release_verify_failures(ctx):
    _failure(ctx.gh("release", "verify", ctx.name("missing"), "--repo", ctx.repo, ok=None), "release not found")
    _failure(ctx.gh("release", "verify", ctx.name("missing"), "--repo", _missing_repo(ctx), ok=None), "release not found")
    _failure(ctx.gh("release", "verify", ctx.name("tag"), "--repo", ctx.repo, "--format", "yaml", ok=None), "format")


@case(command="release verify-asset", variant="unsigned asset default JSON jq template and latest", needs=("release",))
def release_verify_asset(ctx):
    tag = ctx.release["tag_name"]
    asset = ctx.directory / (ctx.name("asset") + ".txt")
    asset.write_text("unsigned asset\n")
    expect_ok(ctx.gh("release", "upload", tag, str(asset), "--repo", ctx.repo))
    _failure(ctx.gh("release", "verify-asset", tag, str(asset), "--repo", ctx.repo, ok=None), "no attestations")
    _failure(ctx.gh("release", "verify-asset", tag, str(asset), "--repo", ctx.repo, "--format", "json", ok=None), "no attestations")
    _failure(ctx.gh("release", "verify-asset", tag, str(asset), "--repo", ctx.repo, "--format", "json", "--jq", ".", ok=None), "no attestations")
    _failure(ctx.gh("release", "verify-asset", tag, str(asset), "--repo", ctx.repo, "--format", "json", "--template", "{{.}}", ok=None), "no attestations")
    _failure(ctx.gh("release", "verify-asset", str(asset), "--repo", ctx.repo, ok=None), "no attestations")


@case(command="release verify-asset", variant="missing local file tag repo and invalid format", needs=("release",))
def release_verify_asset_failures(ctx):
    asset = ctx.directory / (ctx.name("asset") + ".txt")
    asset.write_text("unsigned\n")
    _failure(ctx.gh("release", "verify-asset", ctx.release["tag_name"], str(ctx.directory / ctx.name("absent")), "--repo", ctx.repo, ok=None), "no such file")
    _failure(ctx.gh("release", "verify-asset", ctx.name("missing"), str(asset), "--repo", ctx.repo, ok=None), "release not found")
    _failure(ctx.gh("release", "verify-asset", ctx.name("missing"), str(asset), "--repo", _missing_repo(ctx), ok=None), "release not found")
    _failure(ctx.gh("release", "verify-asset", ctx.release["tag_name"], str(asset), "--repo", ctx.repo, "--format", "yaml", ok=None), "format")
