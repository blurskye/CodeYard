"""Cloud-only commands test gh's GHES/auth guards, not github.com services.

In particular, do not install cloud credentials to bypass the Codespaces guard:
that would send these operations to github.com rather than the disposable forge.
The adjacent `api` calls test the actual GHES HTTP boundary as well. Local
extension/skill fixtures never execute downloaded code and use invocation-owned
paths; extension operations share a mutex because gh stores them per account.
"""
import base64
import re
import shutil

from gh_sweep.harness import case, expect_error, expect_json, expect_ok, expect_unsupported


def rejected(result, *messages, rc=1):
    assert not result.timed_out and result.rc == rc, result
    text = result.stderr + result.stdout
    assert any(message in text for message in messages), text
    assert not re.search(r"panic:|panic at|segmentation fault|fatal error:", text, re.I), text
    return result


def agent_guard(result):
    rejected(result, "agent tasks are not supported on this host")


def attestation_guard(result):
    rejected(result, "gh attestation does not currently support GHES")


def codespace_guard(result):
    # With a GHES host gh sends Codespaces calls to its API, which has none: HTTP 404, as on GHES.
    rejected(result, "HTTP 404: Not Found")


def skill_guard(result):
    rejected(result, "GitHub Skills does not currently support GitHub Enterprise Server")


def missing_repo(ctx):
    return ctx.account.name + "/" + ctx.name("missing")


def other_account(ctx):
    return next(a for a in ctx.sweep.accounts if a != ctx.account)


def artifact(ctx):
    path = ctx.directory / ctx.name("artifact")
    path.write_bytes(b"sweep artifact\n")
    return str(path)


def script_extension(ctx):
    name = "gh-" + ctx.name("extension")
    directory = ctx.directory / name
    directory.mkdir()
    executable = directory / name
    executable.write_text("#!/bin/sh\nprintf 'sweep-extension:%s\\n' \"$*\"\n")
    executable.chmod(0o755)
    expect_ok(ctx.sweep.execute(ctx.account, ["git", "init", str(directory)]))
    return name, directory


def skill_tree(ctx, *, hidden=False):
    root = ctx.directory / ctx.name("source")
    name = ctx.name("skill")
    directory = root / (".agents/skills" if hidden else "skills") / name
    directory.mkdir(parents=True)
    text = f"---\nname: {name}\ndescription: Sweep local skill fixture.\n---\n\n# Sweep skill\n\nNever execute this text.\n"
    (directory / "SKILL.md").write_text(text)
    return root, name, text


@case(command="agent-task create", variant="GHES inline, stdin, file, base, agent and follow", needs=("repo",))
def agent_create(ctx):
    agent_guard(ctx.gh("agent-task", "create", ctx.name("task"), "--repo", ctx.repo, ok=None))
    agent_guard(ctx.gh("agent-task", "create", "--from-file", "-", "--repo", ctx.repo,
                       input=ctx.name("task"), ok=None))
    description = ctx.directory / ctx.name("description")
    description.write_text("Implement " + ctx.name("task"))
    agent_guard(ctx.gh("agent-task", "create", "--from-file", str(description), "--repo", ctx.repo, ok=None))
    agent_guard(ctx.gh("agent-task", "create", ctx.name("task"), "--repo", ctx.repo,
                       "--base", ctx.name("branch"), ok=None))
    agent_guard(ctx.gh("agent-task", "create", ctx.name("task"), "--repo", ctx.repo,
                       "--custom-agent", ctx.name("agent"), ok=None))
    agent_guard(ctx.gh("agent-task", "create", ctx.name("task"), "--repo", missing_repo(ctx),
                       "--follow", ok=None, timeout=15))
    private = ctx.sweep.create_repo(other_account(ctx), ctx.name("private"), private=True)["full_name"]
    agent_guard(ctx.gh("agent-task", "create", ctx.name("task"), "--repo", private, ok=None))


@case(command="agent-task list", variant="GHES text and output formats")
def agent_list(ctx):
    agent_guard(ctx.gh("agent-task", "list", ok=None))
    agent_guard(ctx.gh("agent-task", "list", "--limit", "1", ok=None))
    agent_guard(ctx.gh("agent-task", "list", "--json", "id,name,state,repository", ok=None))
    agent_guard(ctx.gh("agent-task", "list", "--json", "id", "--jq", ".[].id", ok=None))
    agent_guard(ctx.gh("agent-task", "list", "--json", "id", "--template", "{{len .}}", ok=None))


@case(command="agent-task view", variant="GHES PR, session, logs and output formats", needs=("pr",))
def agent_view(ctx):
    number = str(ctx.pr["number"])
    agent_guard(ctx.gh("agent-task", "view", number, "--repo", ctx.repo, ok=None))
    agent_guard(ctx.gh("agent-task", "view", "ffffffff-ffff-4fff-8fff-ffffffffffff", "--repo", ctx.repo, ok=None))
    agent_guard(ctx.gh("agent-task", "view", number, "--repo", ctx.repo, "--log", ok=None))
    agent_guard(ctx.gh("agent-task", "view", number, "--repo", ctx.repo, "--follow", ok=None, timeout=15))
    agent_guard(ctx.gh("agent-task", "view", number, "--repo", ctx.repo, "--json", "id,state", ok=None))
    agent_guard(ctx.gh("agent-task", "view", number, "--repo", ctx.repo,
                       "--json", "id", "--jq", ".id", ok=None))
    agent_guard(ctx.gh("agent-task", "view", number, "--repo", missing_repo(ctx),
                       "--json", "id", "--template", "{{.id}}", ok=None))


@case(command="attestation download", variant="GHES host guard and live repository access", needs=("repo",))
def attestation_download(ctx):
    file = artifact(ctx)
    attestation_guard(ctx.gh("attestation", "download", file, "--repo", ctx.repo, ok=None))
    attestation_guard(ctx.gh("attestation", "download", file, "--owner", ctx.account.name, ok=None))
    attestation_guard(ctx.gh("attestation", "download", file, "--repo", ctx.repo,
                             "--hostname", "codeyard-e2e.test", ok=None))
    attestation_guard(ctx.gh("attestation", "download", file, "--repo", ctx.repo,
                             "--digest-alg", "sha512", ok=None))
    attestation_guard(ctx.gh("attestation", "download", file, "--repo", ctx.repo, "--limit", "1", ok=None))
    attestation_guard(ctx.gh("attestation", "download", file, "--repo", ctx.repo,
                             "--predicate-type", "https://slsa.dev/provenance/v1", ok=None))
    # Codeyard does implement repository attestation reads, even though gh refuses
    # to use them on GHES. Do not misclassify these implemented routes as 404.
    digest = "sha256:" + "0" * 64
    expect_json(ctx.gh("api", f"repos/{ctx.repo}/attestations/{digest}"), {"attestations": []})
    expect_unsupported(ctx.gh("api", f"repos/{missing_repo(ctx)}/attestations/{digest}", ok=None))
    private = ctx.sweep.create_repo(other_account(ctx), ctx.name("private"), private=True)["full_name"]
    expect_unsupported(ctx.gh("api", f"repos/{private}/attestations/{digest}", ok=None))
    expect_error(ctx.gh("api", f"repos/{ctx.repo}/attestations/invalid", ok=None), status=422)


@case(command="attestation trusted-root", variant="GHES default and custom TUF guards")
def trusted_root(ctx):
    root = ctx.directory / ctx.name("root")
    root.write_text("{}\n")
    attestation_guard(ctx.gh("attestation", "trusted-root", ok=None))
    attestation_guard(ctx.gh("attestation", "trusted-root", "--hostname", "codeyard-e2e.test", ok=None))
    attestation_guard(ctx.gh("attestation", "trusted-root", "--verify-only", ok=None))
    # Use only the test host, never a public Sigstore/TUF endpoint.
    attestation_guard(ctx.gh("attestation", "trusted-root", "--tuf-root", str(root),
                             "--tuf-url", "https://codeyard-e2e.test/tuf", ok=None))
    attestation_guard(ctx.gh("attestation", "trusted-root", "--tuf-root", str(root),
                             "--tuf-url", "https://codeyard-e2e.test/tuf", "--verify-only", ok=None))


@case(command="attestation verify", variant="GHES policy and output guards", needs=("repo",))
def attestation_verify(ctx):
    file = artifact(ctx)
    attestation_guard(ctx.gh("attestation", "verify", file, "--repo", ctx.repo, ok=None))
    attestation_guard(ctx.gh("attestation", "verify", file, "--owner", ctx.account.name, ok=None))
    # Separate invocations keep mutually exclusive policy/output modes apart.
    for flags in (
        ("--hostname", "codeyard-e2e.test"), ("--digest-alg", "sha512"),
        ("--limit", "1"), ("--predicate-type", "https://slsa.dev/provenance/v1"),
        ("--cert-identity", "https://codeyard-e2e.test/owner/repo/workflow"),
        ("--cert-identity-regex", "^https://codeyard-e2e[.]test/.*$"),
        ("--cert-oidc-issuer", "https://codeyard-e2e.test"),
        ("--deny-self-hosted-runners",), ("--no-public-good",),
        ("--signer-digest", "0" * 40), ("--signer-repo", ctx.repo),
        ("--signer-workflow", ctx.repo + "/.github/workflows/build.yml"),
        ("--source-digest", "0" * 40), ("--source-ref", "refs/heads/main"),
        ("--format", "json"), ("--format", "json", "--jq", ".[0].verificationResult"),
        ("--format", "json", "--template", "{{len .}}"),
    ):
        attestation_guard(ctx.gh("attestation", "verify", file, "--repo", ctx.repo, *flags, ok=None))
    bundle = ctx.directory / ctx.name("bundle")
    bundle.write_text("{}\n")
    root = ctx.directory / ctx.name("trusted-root")
    root.write_text("{}\n")
    attestation_guard(ctx.gh("attestation", "verify", file, "--repo", ctx.repo, "--bundle", str(bundle), ok=None))
    attestation_guard(ctx.gh("attestation", "verify", file, "--repo", ctx.repo,
                             "--custom-trusted-root", str(root), ok=None))
    # Invalid local/OCI combination must fail before making a registry request.
    rejected(ctx.gh("attestation", "verify", file, "--repo", ctx.repo, "--bundle-from-oci", ok=None),
             "bundle-from-oci flag can only be used with OCI artifact paths",
             "gh attestation does not currently support GHES")


@case(command="codespace code", variant="cloud authentication guard and HTTP 404", needs=("repo",))
def codespace_code(ctx):
    name = ctx.name("codespace")
    codespace_guard(ctx.gh("codespace", "code", "--codespace", name, ok=None))
    codespace_guard(ctx.gh("codespace", "code", "--codespace", name, "--insiders", ok=None))
    codespace_guard(ctx.gh("codespace", "code", "--repo", ctx.repo, ok=None))
    codespace_guard(ctx.gh("codespace", "code", "--repo-owner", ctx.account.name, ok=None))
    expect_unsupported(ctx.gh("api", f"user/codespaces/{name}", ok=None))


@case(command="codespace cp", variant="copy, expand, recursive and selection guards", needs=("repo",))
def codespace_cp(ctx):
    file = artifact(ctx)
    name = ctx.name("codespace")
    codespace_guard(ctx.gh("codespace", "cp", file, "remote:/tmp/sweep", "--codespace", name, ok=None))
    codespace_guard(ctx.gh("codespace", "cp", "remote:/tmp/sweep*", str(ctx.directory),
                          "--codespace", name, "--expand", ok=None))
    codespace_guard(ctx.gh("codespace", "cp", str(ctx.directory), "remote:/tmp/sweep",
                          "--codespace", name, "--recursive", "--profile", "default", ok=None))
    codespace_guard(ctx.gh("codespace", "cp", file, "remote:/tmp/sweep", "--repo", ctx.repo, ok=None))
    codespace_guard(ctx.gh("codespace", "cp", file, "remote:/tmp/sweep", "--repo-owner", ctx.account.name, ok=None))
    expect_unsupported(ctx.gh("api", f"user/codespaces/{name}", ok=None))


@case(command="codespace create", variant="creation flags stop at cloud auth", needs=("repo",))
def codespace_create(ctx):
    codespace_guard(ctx.gh("codespace", "create", "--repo", ctx.repo, ok=None))
    for flags in (("--branch", ctx.name("branch")), ("--default-permissions",),
                  ("--devcontainer-path", ".devcontainer/devcontainer.json"),
                  ("--display-name", ctx.name("display")[:48]), ("--idle-timeout", "10m"),
                  ("--location", "EastUs"), ("--machine", "basicLinux32gb"),
                  ("--retention-period", "24h"), ("--status",)):
        codespace_guard(ctx.gh("codespace", "create", "--repo", ctx.repo, *flags, ok=None))
    codespace_guard(ctx.gh("codespace", "create", "--repo", missing_repo(ctx), ok=None))
    expect_unsupported(ctx.gh("api", f"repos/{ctx.repo}/codespaces", "--method", "POST", ok=None))


@case(command="codespace delete", variant="named, bulk, age, org and selection guards", needs=("repo",))
def codespace_delete(ctx):
    name = ctx.name("codespace")
    codespace_guard(ctx.gh("codespace", "delete", "--codespace", name, "--force", ok=None))
    codespace_guard(ctx.gh("codespace", "delete", "--all", "--force", ok=None))
    codespace_guard(ctx.gh("codespace", "delete", "--days", "7", "--force", ok=None))
    codespace_guard(ctx.gh("codespace", "delete", "--repo", ctx.repo, "--force", ok=None))
    codespace_guard(ctx.gh("codespace", "delete", "--repo-owner", ctx.account.name, "--force", ok=None))
    org = ctx.org()
    codespace_guard(ctx.gh("codespace", "delete", "--org", org, "--user", other_account(ctx).name,
                          "--all", "--force", ok=None))
    expect_unsupported(ctx.gh("api", f"user/codespaces/{name}", "--method", "DELETE", ok=None))


@case(command="codespace edit", variant="rename, machine and selection guards", needs=("repo",))
def codespace_edit(ctx):
    name = ctx.name("codespace")
    codespace_guard(ctx.gh("codespace", "edit", "--codespace", name, "--display-name", ctx.name("display")[:48], ok=None))
    codespace_guard(ctx.gh("codespace", "edit", "--codespace", name, "--machine", "basicLinux32gb", ok=None))
    codespace_guard(ctx.gh("codespace", "edit", "--repo", ctx.repo, "--display-name", "sweep", ok=None))
    codespace_guard(ctx.gh("codespace", "edit", "--repo-owner", ctx.account.name, "--display-name", "sweep", ok=None))
    expect_unsupported(ctx.gh("api", f"user/codespaces/{name}", "--method", "PATCH", ok=None))


@case(command="codespace jupyter", variant="named and filtered cloud guards", needs=("repo",))
def codespace_jupyter(ctx):
    name = ctx.name("codespace")
    codespace_guard(ctx.gh("codespace", "jupyter", "--codespace", name, ok=None))
    codespace_guard(ctx.gh("codespace", "jupyter", "--repo", ctx.repo, ok=None))
    codespace_guard(ctx.gh("codespace", "jupyter", "--repo-owner", ctx.account.name, ok=None))
    expect_unsupported(ctx.gh("api", f"user/codespaces/{name}", ok=None))


@case(command="codespace list", variant="formats, limit, repo and organization guards", needs=("repo",))
def codespace_list(ctx):
    codespace_guard(ctx.gh("codespace", "list", ok=None))
    codespace_guard(ctx.gh("codespace", "list", "--repo", ctx.repo, "--limit", "1", ok=None))
    codespace_guard(ctx.gh("codespace", "list", "--json", "name,state,repository", ok=None))
    codespace_guard(ctx.gh("codespace", "list", "--json", "name", "--jq", ".[].name", ok=None))
    codespace_guard(ctx.gh("codespace", "list", "--json", "name", "--template", "{{len .}}", ok=None))
    org = ctx.org()
    codespace_guard(ctx.gh("codespace", "list", "--org", org, ok=None))
    codespace_guard(ctx.gh("codespace", "list", "--org", org, "--user", other_account(ctx).name, ok=None))
    expect_unsupported(ctx.gh("api", "user/codespaces", ok=None))
    expect_unsupported(ctx.gh("api", f"orgs/{org}/codespaces", ok=None))


@case(command="codespace logs", variant="snapshot, follow and selection guards", needs=("repo",))
def codespace_logs(ctx):
    name = ctx.name("codespace")
    codespace_guard(ctx.gh("codespace", "logs", "--codespace", name, ok=None))
    codespace_guard(ctx.gh("codespace", "logs", "--codespace", name, "--follow", ok=None, timeout=15))
    codespace_guard(ctx.gh("codespace", "logs", "--repo", ctx.repo, ok=None))
    codespace_guard(ctx.gh("codespace", "logs", "--repo-owner", ctx.account.name, ok=None))
    expect_unsupported(ctx.gh("api", f"user/codespaces/{name}", ok=None))


@case(command="codespace ports forward", variant="loopback, all interfaces and selection guards", needs=("repo",))
def codespace_forward(ctx):
    name = ctx.name("codespace")
    codespace_guard(ctx.gh("codespace", "ports", "forward", "3000:0", "--codespace", name, ok=None, timeout=15))
    codespace_guard(ctx.gh("codespace", "ports", "forward", "3000:0", "--codespace", name,
                          "--all-interfaces", ok=None, timeout=15))
    codespace_guard(ctx.gh("codespace", "ports", "forward", "3000:0", "--repo", ctx.repo, ok=None, timeout=15))
    codespace_guard(ctx.gh("codespace", "ports", "forward", "3000:0", "--repo-owner", ctx.account.name, ok=None, timeout=15))
    expect_unsupported(ctx.gh("api", f"user/codespaces/{name}", ok=None))


@case(command="codespace ports visibility", variant="public, private, org and selection guards", needs=("repo",))
def codespace_visibility(ctx):
    name = ctx.name("codespace")
    codespace_guard(ctx.gh("codespace", "ports", "visibility", "3000:private", "--codespace", name, ok=None))
    codespace_guard(ctx.gh("codespace", "ports", "visibility", "3000:public", "--codespace", name, ok=None))
    codespace_guard(ctx.gh("codespace", "ports", "visibility", "3000:org", "--codespace", name, ok=None))
    codespace_guard(ctx.gh("codespace", "ports", "visibility", "3000:private", "--repo", ctx.repo, ok=None))
    codespace_guard(ctx.gh("codespace", "ports", "visibility", "3000:private", "--repo-owner", ctx.account.name, ok=None))
    expect_unsupported(ctx.gh("api", f"user/codespaces/{name}", ok=None))


@case(command="codespace rebuild", variant="incremental, full and selection guards", needs=("repo",))
def codespace_rebuild(ctx):
    name = ctx.name("codespace")
    codespace_guard(ctx.gh("codespace", "rebuild", "--codespace", name, ok=None))
    codespace_guard(ctx.gh("codespace", "rebuild", "--codespace", name, "--full", ok=None))
    codespace_guard(ctx.gh("codespace", "rebuild", "--repo", ctx.repo, ok=None))
    codespace_guard(ctx.gh("codespace", "rebuild", "--repo-owner", ctx.account.name, ok=None))
    expect_unsupported(ctx.gh("api", f"user/codespaces/{name}", ok=None))


@case(command="codespace ssh", variant="connection, config, debug and selection guards", needs=("repo",))
def codespace_ssh(ctx):
    name = ctx.name("codespace")
    codespace_guard(ctx.gh("codespace", "ssh", "--codespace", name, ok=None, timeout=15))
    codespace_guard(ctx.gh("codespace", "ssh", "--config", ok=None))
    codespace_guard(ctx.gh("codespace", "ssh", "--codespace", name, "--debug",
                          "--debug-file", str(ctx.directory / ctx.name("ssh-log")), ok=None, timeout=15))
    codespace_guard(ctx.gh("codespace", "ssh", "--codespace", name, "--profile", "default",
                          "--server-port", "2222", ok=None, timeout=15))
    codespace_guard(ctx.gh("codespace", "ssh", "--repo", ctx.repo, ok=None, timeout=15))
    codespace_guard(ctx.gh("codespace", "ssh", "--repo-owner", ctx.account.name, ok=None, timeout=15))
    expect_unsupported(ctx.gh("api", f"user/codespaces/{name}", ok=None))


@case(command="codespace stop", variant="named, repository, owner and org guards", needs=("repo",))
def codespace_stop(ctx):
    name = ctx.name("codespace")
    codespace_guard(ctx.gh("codespace", "stop", "--codespace", name, ok=None))
    codespace_guard(ctx.gh("codespace", "stop", "--repo", ctx.repo, ok=None))
    codespace_guard(ctx.gh("codespace", "stop", "--repo-owner", ctx.account.name, ok=None))
    org = ctx.org()
    codespace_guard(ctx.gh("codespace", "stop", "--codespace", name, "--org", org,
                          "--user", other_account(ctx).name, ok=None))
    expect_unsupported(ctx.gh("api", f"user/codespaces/{name}/stop", "--method", "POST", ok=None))


@case(command="codespace view", variant="details, formatting and selection guards", needs=("repo",))
def codespace_view(ctx):
    name = ctx.name("codespace")
    codespace_guard(ctx.gh("codespace", "view", "--codespace", name, ok=None))
    codespace_guard(ctx.gh("codespace", "view", "--codespace", name, "--json", "name,state", ok=None))
    codespace_guard(ctx.gh("codespace", "view", "--codespace", name, "--json", "name", "--jq", ".name", ok=None))
    codespace_guard(ctx.gh("codespace", "view", "--codespace", name, "--json", "name", "--template", "{{.name}}", ok=None))
    codespace_guard(ctx.gh("codespace", "view", "--repo", ctx.repo, ok=None))
    codespace_guard(ctx.gh("codespace", "view", "--repo-owner", ctx.account.name, ok=None))
    expect_unsupported(ctx.gh("api", f"user/codespaces/{name}", ok=None))


@case(command="copilot", variant="remove absent locally managed binary", serial_group="g8-copilot")
def copilot_remove(ctx):
    # Never launch Copilot: that can execute a PATH binary or download from GitHub.
    result = ctx.gh("copilot", "--remove", ok=None)
    assert not result.timed_out and result.rc in (0, 1), result
    assert "Copilot CLI not installed" in result.stderr + result.stdout, result


@case(command="extension browse", variant="non-TTY refusal", serial_group="g8-extensions")
def extension_browse(ctx):
    rejected(ctx.gh("extension", "browse", ok=None, timeout=15), "TTY", "terminal")
    rejected(ctx.gh("extension", "browse", "--single-column", ok=None, timeout=15), "TTY", "terminal")
    rejected(ctx.gh("extension", "browse", "--debug", "--single-column", ok=None, timeout=15), "TTY", "terminal")


@case(command="extension create", variant="script and precompiled scaffolds", serial_group="g8-extensions")
def extension_create(ctx):
    name = ctx.name("scaffold")
    result = expect_ok(ctx.gh("extension", "create", name))
    directory = ctx.directory / ("gh-" + name)
    assert (directory / ("gh-" + name)).is_file(), result
    # A Go scaffold runs `go mod init`; without Go, `other` still covers --precompiled.
    for language in ("go", "other") if shutil.which("go") else ("other",):
        name = ctx.name("scaffold")
        result = expect_ok(ctx.gh("extension", "create", name, "--precompiled", language))
        directory = ctx.directory / ("gh-" + name)
        assert (directory / ".github/workflows").is_dir(), result
        assert any(directory.iterdir()), result
    rejected(ctx.gh("extension", "create", ctx.name("invalid"), "--precompiled", "invalid", ok=None),
             "precompiled", "invalid")


@case(command="extension exec", variant="local script argument forwarding and absent extension", serial_group="g8-extensions")
def extension_exec(ctx):
    name, directory = script_extension(ctx)
    expect_ok(ctx.gh("extension", "install", ".", cwd=directory))
    try:
        result = expect_ok(ctx.gh("extension", "exec", name[3:], "one", "two words"))
        assert result.stdout == "sweep-extension:one two words\n", result
        rejected(ctx.gh("extension", "exec", ctx.name("absent"), ok=None), "not found")
    finally:
        expect_ok(ctx.gh("extension", "remove", name[3:]))


@case(command="extension install", variant="local, duplicate, force and remote 404", serial_group="g8-extensions")
def extension_install(ctx):
    name, directory = script_extension(ctx)
    expect_ok(ctx.gh("extension", "install", ".", cwd=directory))
    try:
        rejected(ctx.gh("extension", "install", ".", cwd=directory, ok=None),
                 "already an installed extension that provides")
        # --force upgrades remote extensions only; a local one is still refused.
        rejected(ctx.gh("extension", "install", ".", "--force", cwd=directory, ok=None),
                 "already an installed extension that provides")
        # gh lists a local extension by name only, with empty repository and version columns.
        listed = expect_ok(ctx.gh("extension", "list"))
        assert name[3:] in listed.stdout, listed
    finally:
        expect_ok(ctx.gh("extension", "remove", name[3:]))
    url = "https://codeyard-e2e.test/" + ctx.account.name + "/gh-" + ctx.name("absent")
    # gh turns the 404 for a missing extension repository into its own message.
    for flags in ((), ("--pin", "v0.0.1"), ("--force",)):
        rejected(ctx.gh("extension", "install", url, *flags, ok=None),
                 f"Could not find extension '{url}' on host codeyard-e2e.test")
    rejected(ctx.gh("extension", "install", ".", "--pin", "v0.0.1", cwd=directory, ok=None),
             "local extensions cannot be pinned")


@case(command="extension list", variant="installed link appears and removal disappears", serial_group="g8-extensions")
def extension_list(ctx):
    name, directory = script_extension(ctx)
    before = expect_ok(ctx.gh("extension", "list"))
    assert name not in before.stdout, before
    expect_ok(ctx.gh("extension", "install", ".", cwd=directory))
    try:
        listed = expect_ok(ctx.gh("extension", "list"))
        assert name[3:] in listed.stdout, listed
    finally:
        expect_ok(ctx.gh("extension", "remove", name[3:]))
    assert name not in expect_ok(ctx.gh("extension", "list")).stdout


@case(command="extension remove", variant="remove local link without deleting source", serial_group="g8-extensions")
def extension_remove(ctx):
    name, directory = script_extension(ctx)
    expect_ok(ctx.gh("extension", "install", ".", cwd=directory))
    expect_ok(ctx.gh("extension", "remove", name[3:]))
    assert (directory / name).is_file()
    assert name not in expect_ok(ctx.gh("extension", "list")).stdout
    rejected(ctx.gh("extension", "remove", name[3:], ok=None), "no extension", "not installed", "not found")


@case(command="extension search", variant="topic discovery, filters, ordering and formats", needs=("repo",))
def extension_search(ctx):
    ctx.api("PUT", f"repos/{ctx.repo}/topics", {"names": ["gh-extension"]})
    rows = expect_json(ctx.gh("extension", "search", "--owner", ctx.account.name,
                              "--limit", "100", "--json", "fullName,isPrivate"),
                       [{"fullName": str, "isPrivate": bool}])
    assert {"fullName": ctx.repo, "isPrivate": False} in rows, rows
    result = expect_ok(ctx.gh("extension", "search", "--owner", ctx.account.name, "--json", "fullName",
                              "--jq", ".[].fullName"))
    assert ctx.repo in result.stdout.splitlines(), result
    result = expect_ok(ctx.gh("extension", "search", "--owner", ctx.account.name, "--json", "fullName",
                              "--template", "{{range .}}{{.fullName}}{{\"\\n\"}}{{end}}"))
    assert ctx.repo in result.stdout.splitlines(), result
    for sort in ("stars", "forks", "updated", "help-wanted-issues"):
        expect_json(ctx.gh("extension", "search", ctx.name("absent"), "--owner", ctx.account.name,
                           "--sort", sort, "--order", "asc", "--limit", "1", "--json", "fullName"), [])
    expect_json(ctx.gh("extension", "search", ctx.name("absent"), "--license", "MIT",
                       "--sort", "stars", "--order", "desc", "--json", "fullName"), [])
    rejected(ctx.gh("extension", "search", "--sort", "invalid", ok=None), "invalid")


@case(command="extension upgrade", variant="local links cannot upgrade and unknown name", serial_group="g8-extensions")
def extension_upgrade(ctx):
    name, directory = script_extension(ctx)
    expect_ok(ctx.gh("extension", "install", ".", cwd=directory))
    try:
        for flags in (("--dry-run",), ("--force",), ()):
            result = ctx.gh("extension", "upgrade", name[3:], *flags, ok=None)
            assert not result.timed_out and result.rc in (0, 1), result
            assert "local" in (result.stderr + result.stdout).lower(), result
        result = ctx.gh("extension", "upgrade", "--all", "--dry-run", "--force", ok=None)
        assert not result.timed_out and result.rc in (0, 1), result
        assert "local" in (result.stderr + result.stdout).lower(), result
    finally:
        expect_ok(ctx.gh("extension", "remove", name[3:]))
    rejected(ctx.gh("extension", "upgrade", ctx.name("absent"), ok=None), "no extension", "not installed", "not found")


@case(command="preview prompter", variant="non-TTY prompt EOF")
def preview_prompter(ctx):
    rejected(ctx.gh("preview", "prompter", "input", input="", ok=None, timeout=10),
             "EOF", "terminal", "prompt", "canceled")


@case(command="skill install", variant="remote GHES guard and local copying", needs=("repo",))
def skill_install(ctx):
    skill_guard(ctx.gh("skill", "install", ctx.repo, ctx.name("skill"), ok=None))
    for flags in (("--all",), ("--allow-hidden-dirs",), ("--force",), ("--upstream",),
                  ("--pin", "v0.0.1"), ("--agent", "claude-code", "--scope", "user")):
        skill_guard(ctx.gh("skill", "install", ctx.repo, *flags, ok=None))
    skill_guard(ctx.gh("skill", "install", missing_repo(ctx), "--all", ok=None))
    private = ctx.sweep.create_repo(other_account(ctx), ctx.name("private"), private=True)["full_name"]
    skill_guard(ctx.gh("skill", "install", private, "--all", ok=None))
    root, name, text = skill_tree(ctx)
    dest = ctx.directory / ctx.name("installed")
    # --dir wins over agent/scope, so even user scope must stay in this case's
    # private destination, never the shared account home.
    result = expect_ok(ctx.gh("skill", "install", str(root), name, "--from-local",
                              "--agent", "claude-code", "--scope", "user", "--dir", str(dest)))
    installed = dest / name / "SKILL.md"
    assert installed.is_file() and "Never execute this text." in installed.read_text(), result
    assert not installed.is_symlink()
    installed.write_text("local change\n")
    expect_ok(ctx.gh("skill", "install", str(root), "--all", "--from-local", "--dir", str(dest), "--force"))
    assert "Never execute this text." in installed.read_text()
    hidden, hidden_name, _ = skill_tree(ctx, hidden=True)
    expect_ok(ctx.gh("skill", "install", str(hidden), "--all", "--from-local", "--allow-hidden-dirs", "--dir", str(dest)))
    assert (dest / hidden_name / "SKILL.md").is_file()
    assert (root / "skills" / name / "SKILL.md").read_text() == text


@case(command="skill list", variant="custom directory fields, jq, template and empty host")
def skill_list(ctx):
    root, name, _ = skill_tree(ctx)
    directory = root / "skills"
    rows = expect_json(ctx.gh("skill", "list", "--dir", str(directory), "--json", "skillName,path,pinned"),
                       [{"skillName": str, "path": str, "pinned": bool}])
    assert len(rows) == 1 and rows[0]["skillName"] == name and rows[0]["pinned"] is False, rows
    result = expect_ok(ctx.gh("skill", "list", "--dir", str(directory), "--json", "skillName", "--jq", ".[0].skillName"))
    assert result.stdout.strip() == name, result
    result = expect_ok(ctx.gh("skill", "list", "--dir", str(directory), "--json", "skillName", "--template", "{{(index . 0).skillName}}"))
    assert result.stdout.strip() == name, result
    expect_json(ctx.gh("skill", "list", "--agent", "claude-code", "--scope", "user", "--json", "skillName"), [])
    rejected(ctx.gh("skill", "list", "--scope", "invalid", ok=None), "invalid")


@case(command="skill preview", variant="remote GHES guard with hidden-directory mode", needs=("repo",))
def skill_preview(ctx):
    skill_guard(ctx.gh("skill", "preview", ctx.repo, ctx.name("skill"), ok=None))
    skill_guard(ctx.gh("skill", "preview", ctx.repo, "--allow-hidden-dirs", ok=None))
    skill_guard(ctx.gh("skill", "preview", missing_repo(ctx), ctx.name("skill"), ok=None))


@case(command="skill publish", variant="local validation, fix, tag and invalid skill", needs=("repo",))
def skill_publish(ctx):
    _, name, text = skill_tree(ctx)
    ctx.api("PUT", f"repos/{ctx.repo}/contents/skills/{name}/SKILL.md",
            {"message": "Sweep skill fixture", "content": base64.b64encode(text.encode()).decode()})
    ctx.api("PUT", f"repos/{ctx.repo}/topics", {"names": ["agent-skills"]})
    # A real clean checkout avoids accidentally testing an unborn HEAD or
    # untracked-file policy instead of publish validation. REST made the commit.
    root = ctx.directory / ctx.name("checkout")
    expect_ok(ctx.sweep.execute(ctx.account, ["git", "clone", ctx.sweep.base + "/" + ctx.repo + ".git", str(root)]))
    result = expect_ok(ctx.gh("skill", "publish", str(root), "--dry-run"))
    assert "Dry run complete" in result.stdout + result.stderr, result
    result = expect_ok(ctx.gh("skill", "publish", str(root), "--dry-run", "--tag", ctx.name("v1")))
    assert "Dry run complete" in result.stdout + result.stderr, result
    result = expect_ok(ctx.gh("skill", "publish", str(root), "--fix"))
    assert (root / "skills" / name / "SKILL.md").read_text() == text, result
    # gh publishes only from github.com or ghe.com remotes; with a GHES remote it stops after validating.
    result = expect_ok(ctx.gh("skill", "publish", str(root), "--tag", ctx.name("v1")))
    assert "is not a GitHub repository" in result.stdout, result
    assert "Set up a GitHub remote to publish" in result.stderr, result
    (root / "skills" / name / "SKILL.md").write_text("---\nname: INVALID NAME\n---\n")
    rejected(ctx.gh("skill", "publish", str(root), "--dry-run", ok=None), "validation", "Validation", "invalid", "description")


@case(command="skill search", variant="remote GHES guard for paging and output")
def skill_search(ctx):
    query = ctx.name("query")
    skill_guard(ctx.gh("skill", "search", query, ok=None))
    skill_guard(ctx.gh("skill", "search", query, "--owner", ctx.account.name, "--page", "2", "--limit", "1", ok=None))
    skill_guard(ctx.gh("skill", "search", query, "--json", "skillName,repo,path", ok=None))
    skill_guard(ctx.gh("skill", "search", query, "--json", "skillName", "--jq", ".[].skillName", ok=None))
    skill_guard(ctx.gh("skill", "search", query, "--json", "skillName", "--template", "{{len .}}", ok=None))


@case(command="skill update", variant="empty local directory and all update modes")
def skill_update(ctx):
    directory = ctx.directory / ctx.name("empty")
    directory.mkdir()
    for flags in ((), ("--all",), ("--dry-run",), ("--force", "--all"), ("--unpin", "--all"),
                  ("--all", "--force", "--unpin", "--dry-run")):
        result = expect_ok(ctx.gh("skill", "update", "--dir", str(directory), *flags))
        assert "No installed skills found" in result.stdout + result.stderr, result
        assert list(directory.iterdir()) == []
    root, name, text = skill_tree(ctx)
    result = expect_ok(ctx.gh("skill", "update", "--dir", str(root / "skills"), "--all", "--dry-run"))
    assert name + " has no GitHub metadata" in result.stdout + result.stderr, result
    assert (root / "skills" / name / "SKILL.md").read_text() == text


@case(command="agent-task list", variant="malformed numeric limit")
def agent_list_bad_limit(ctx):
    rejected(ctx.gh("agent-task", "list", "--limit", "not-an-integer", ok=None), "invalid argument")


@case(command="attestation download", variant="malformed numeric limit")
def attestation_bad_limit(ctx):
    rejected(ctx.gh("attestation", "download", artifact(ctx), "--repo", missing_repo(ctx),
                    "--limit", "not-an-integer", ok=None), "invalid argument")


@case(command="codespace create", variant="malformed duration before cloud authentication")
def codespace_bad_duration(ctx):
    rejected(ctx.gh("codespace", "create", "--repo", missing_repo(ctx),
                    "--idle-timeout", "not-a-duration", ok=None), "invalid argument")


@case(command="codespace delete", variant="malformed age before cloud authentication")
def codespace_bad_days(ctx):
    rejected(ctx.gh("codespace", "delete", "--days", "not-an-integer", "--force", ok=None), "invalid argument")


@case(command="codespace ssh", variant="malformed server port before cloud authentication")
def codespace_bad_port(ctx):
    rejected(ctx.gh("codespace", "ssh", "--codespace", ctx.name("codespace"),
                    "--server-port", "not-an-integer", ok=None), "invalid argument")


@case(command="extension search", variant="malformed limit")
def extension_search_bad_limit(ctx):
    rejected(ctx.gh("extension", "search", ctx.name("query"), "--limit", "not-an-integer", ok=None),
             "invalid argument")


@case(command="skill install", variant="local source conflicts with pin and upstream")
def skill_install_conflicts(ctx):
    root, name, _ = skill_tree(ctx)
    dest = ctx.directory / ctx.name("destination")
    rejected(ctx.gh("skill", "install", str(root), name, "--from-local", "--dir", str(dest),
                    "--pin", "v1.0.0", ok=None), "--from-local and --pin cannot be used together")
    rejected(ctx.gh("skill", "install", str(root), name, "--from-local", "--dir", str(dest),
                    "--upstream", ok=None), "--from-local and --upstream cannot be used together")
    assert not dest.exists() or not list(dest.iterdir())


@case(command="skill publish", variant="mutually exclusive validation and fix modes")
def skill_publish_conflict(ctx):
    root, _, _ = skill_tree(ctx)
    rejected(ctx.gh("skill", "publish", str(root), "--dry-run", "--fix", ok=None),
             "specify only one of `--fix` or `--dry-run`")


@case(command="skill publish", variant="fix strips committed install metadata", needs=("repo",))
def skill_publish_fix_metadata(ctx):
    _, name, text = skill_tree(ctx)
    marked = text.replace("description: Sweep local skill fixture.\n",
                          "description: Sweep local skill fixture.\nmetadata:\n  github-owner: "
                          + ctx.account.name + "\n  github-repo: " + ctx.repo.split("/", 1)[1] + "\n")
    ctx.api("PUT", f"repos/{ctx.repo}/contents/skills/{name}/SKILL.md",
            {"message": "Sweep skill metadata fixture", "content": base64.b64encode(marked.encode()).decode()})
    ctx.api("PUT", f"repos/{ctx.repo}/topics", {"names": ["agent-skills"]})
    root = ctx.directory / ctx.name("checkout")
    expect_ok(ctx.sweep.execute(ctx.account, ["git", "clone", ctx.sweep.base + "/" + ctx.repo + ".git", str(root)]))
    rejected(ctx.gh("skill", "publish", str(root), "--dry-run", ok=None), "metadata", "must be stripped")
    expect_ok(ctx.gh("skill", "publish", str(root), "--fix"))
    fixed = (root / "skills" / name / "SKILL.md").read_text()
    assert "github-owner" not in fixed and "github-repo" not in fixed, fixed
    assert "Never execute this text." in fixed and name in fixed, fixed


@case(command="skill search", variant="malformed page")
def skill_search_bad_page(ctx):
    rejected(ctx.gh("skill", "search", ctx.name("query"), "--page", "not-an-integer", ok=None), "invalid argument")
