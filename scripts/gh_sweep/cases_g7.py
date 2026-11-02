"""Real-gh g7 contracts. All mutable CLI configuration is private to a case.

Local commands deliberately use disposable Account directories, never the shared
provisioned accounts' config. REST fixtures are not counted as CLI coverage.
"""
import base64
import hashlib
import json
import math
import secrets
import shutil
import struct
import time
from urllib.parse import urlsplit

from gh_sweep.harness import (
    Account, HOST, case, expect_error, expect_json, expect_ok, expect_unsupported,
)


def isolated(ctx, authenticated=False):
    account = Account(ctx.account.name, ctx.directory / ctx.name("cli"), ctx.account.token)
    for folder in ("home", "config", "cache", "data", "state", "runtime"):
        (account.directory / folder).mkdir(parents=True, mode=0o700)
    if authenticated:
        for name in ("hosts.yml", "config.yml"):
            source = ctx.account.directory / "config" / name
            if source.exists():
                shutil.copyfile(source, account.directory / "config" / name)
    return account


def local_hosts(account, users, host=HOST):
    """Write gh's multi-account config, without invoking OAuth."""
    active = next(iter(users))
    lines = [host + ":", "    user: " + active,
             "    oauth_token: " + users[active], "    git_protocol: https", "    users:"]
    for name, token in users.items():
        lines += ["        " + name + ":", "            oauth_token: " + token]
    (account.directory / "config/hosts.yml").write_text("\n".join(lines) + "\n")


def client_error(result, *fragments):
    assert not result.timed_out and result.rc > 0, result
    text = result.stderr.lower()
    assert text and not any(s in text for s in ("panic:", "panic at", "segmentation fault", "fatal error:")), text
    for fragment in fragments:
        assert fragment.lower() in text, result.stderr
    return result


def numbers(result, expected):
    rows = expect_json(result, [{"number": int}])
    assert sorted(row["number"] for row in rows) == sorted(expected), rows


def paths(result, expected):
    rows = expect_json(result, [{"path": str}])
    assert sorted(row["path"] for row in rows) == sorted(expected), rows


def other_account(ctx):
    return next(a for a in ctx.sweep.accounts if a.name != ctx.account.name)


def prime(ctx, bits=1024):
    # Test-owned RSA material: no gpg/ssh-keygen dependency and no shared key IDs.
    small = (3, 5, 7, 11, 13, 17, 19, 23, 29, 31, 37, 41, 43, 47)
    while True:
        ctx.check_deadline()
        n = secrets.randbits(bits) | (3 << (bits - 2)) | 1
        if math.gcd(n - 1, 65537) != 1 or any(n % p == 0 for p in small):
            continue
        d, s = n - 1, 0
        while d % 2 == 0:
            d //= 2
            s += 1
        for _ in range(32):
            x = pow(secrets.randbelow(n - 3) + 2, d, n)
            if x in (1, n - 1):
                continue
            for _ in range(s - 1):
                x = pow(x, 2, n)
                if x == n - 1:
                    break
            else:
                break
        else:
            return n


def rsa(ctx):
    p, q = prime(ctx), prime(ctx)
    while q == p:
        q = prime(ctx)
    n = p * q
    return n, pow(65537, -1, (p - 1) * (q - 1))


def ssh_material(ctx):
    n, _ = rsa(ctx)
    def string(value):
        return struct.pack(">I", len(value)) + value
    blob = string(b"ssh-rsa") + string(b"\x01\x00\x01") + string(b"\x00" + n.to_bytes(256, "big"))
    text = "ssh-rsa " + base64.b64encode(blob).decode() + " " + ctx.name("key") + "\n"
    file = ctx.directory / ctx.name("public")
    file.write_text(text)
    return file, text


def gpg_material(ctx):
    """Minimal RFC 4880 RSA/SHA256 certificate with a real UID self-signature."""
    n, d = rsa(ctx)
    def mpi(value):
        return struct.pack(">H", value.bit_length()) + value.to_bytes((value.bit_length() + 7) // 8, "big")
    def packet(tag, body):
        return bytes([0xc0 | tag, 255]) + struct.pack(">I", len(body)) + body
    stamp = struct.pack(">I", int(time.time()) - 60)
    public = b"\x04" + stamp + b"\x01" + mpi(n) + mpi(65537)
    prefixed = b"\x99" + struct.pack(">H", len(public)) + public
    fingerprint = hashlib.sha1(prefixed).digest()
    uid = (ctx.name("identity") + " <" + ctx.account.name + "@example.test>").encode()
    hashed = b"\x05\x02" + stamp + b"\x02\x1b\x03"
    signature = b"\x04\x13\x01\x08" + struct.pack(">H", len(hashed)) + hashed
    digest = hashlib.sha256(prefixed + b"\xb4" + struct.pack(">I", len(uid)) + uid
                            + signature + b"\x04\xff" + struct.pack(">I", len(signature))).digest()
    der = bytes.fromhex("3031300d060960864801650304020105000420") + digest
    encoded = b"\x00\x01" + b"\xff" * (256 - len(der) - 3) + b"\x00" + der
    unhashed = b"\x09\x10" + fingerprint[-8:]
    signature += struct.pack(">H", len(unhashed)) + unhashed + digest[:2] + mpi(pow(int.from_bytes(encoded, "big"), d, n))
    raw = packet(6, public) + packet(13, uid) + packet(2, signature)
    crc = 0xb704ce
    for byte in raw:
        crc ^= byte << 16
        for _ in range(8):
            crc <<= 1
            if crc & 0x1000000:
                crc ^= 0x1864cfb
    data = base64.b64encode(raw).decode()
    armor = "-----BEGIN PGP PUBLIC KEY BLOCK-----\n\n" + "\n".join(data[i:i+64] for i in range(0, len(data), 64))
    armor += "\n=" + base64.b64encode((crc & 0xffffff).to_bytes(3, "big")).decode() + "\n-----END PGP PUBLIC KEY BLOCK-----\n"
    file = ctx.directory / ctx.name("gpg")
    file.write_text(armor)
    return file, fingerprint[-8:].hex().upper()


@case(command="alias set", variant="expansion, stdin, shell, collision and clobber")
def alias_set(ctx):
    a = isolated(ctx)
    name = ctx.name("alias")
    expect_ok(ctx.sweep.gh(a, "alias", "set", name, "api user"))
    assert "api user" in expect_ok(ctx.sweep.gh(a, "alias", "list")).stdout
    client_error(ctx.sweep.gh(a, "alias", "set", name, "api rate_limit", ok=None), "already")
    expect_ok(ctx.sweep.gh(a, "alias", "set", name, "api rate_limit", "--clobber"))
    assert "api rate_limit" in expect_ok(ctx.sweep.gh(a, "alias", "list")).stdout
    shell = ctx.name("shell")
    expect_ok(ctx.sweep.gh(a, "alias", "set", shell, "printf '%s\\n' \"$1\"", "--shell"))
    assert expect_ok(ctx.sweep.gh(a, shell, name)).stdout.strip() == name
    stdin = ctx.name("stdin")
    expect_ok(ctx.sweep.gh(a, "alias", "set", stdin, "-", input="api user\n"))
    assert stdin in expect_ok(ctx.sweep.gh(a, "alias", "list")).stdout
    client_error(ctx.sweep.gh(a, "alias", "set", "api", "api user", ok=None), "command")


@case(command="alias import", variant="file/stdin, duplicate/clobber, malformed YAML and unknown flag")
def alias_import(ctx):
    a = isolated(ctx)
    name = ctx.name("alias")
    file = ctx.directory / ctx.name("aliases")
    file.write_text(name + ": api user\n")
    expect_ok(ctx.sweep.gh(a, "alias", "import", str(file)))
    assert name in expect_ok(ctx.sweep.gh(a, "alias", "list")).stdout
    # Import deliberately skips duplicates unless --clobber is given.
    expect_ok(ctx.sweep.gh(a, "alias", "import", "-", input=name + ": api rate_limit\n"))
    assert "api user" in expect_ok(ctx.sweep.gh(a, "alias", "list")).stdout
    expect_ok(ctx.sweep.gh(a, "alias", "import", "-", "--clobber", input=name + ": api rate_limit\n"))
    assert "api rate_limit" in expect_ok(ctx.sweep.gh(a, "alias", "list")).stdout
    client_error(ctx.sweep.gh(a, "alias", "import", "-", input="[not: a: map", ok=None), "yaml")
    # Inventory includes --label, but gh 2.102.0 does not implement this option.
    client_error(ctx.sweep.gh(a, "alias", "import", "-", "--label", "bug", input="{}\n", ok=None), "unknown flag", "--label")


@case(command="alias list", variant="empty and populated YAML")
def alias_list(ctx):
    a = isolated(ctx)
    assert not expect_ok(ctx.sweep.gh(a, "alias", "list")).stdout.strip()
    name = ctx.name("alias")
    expect_ok(ctx.sweep.gh(a, "alias", "set", name, "api user"))
    assert name + ": api user" in expect_ok(ctx.sweep.gh(a, "alias", "list")).stdout


@case(command="alias delete", variant="one, missing, all, empty all")
def alias_delete(ctx):
    a = isolated(ctx)
    first, second = ctx.name("first"), ctx.name("second")
    expect_ok(ctx.sweep.gh(a, "alias", "set", first, "api user"))
    expect_ok(ctx.sweep.gh(a, "alias", "set", second, "api user"))
    expect_ok(ctx.sweep.gh(a, "alias", "delete", first))
    output = expect_ok(ctx.sweep.gh(a, "alias", "list")).stdout
    assert first not in output and second in output, output
    client_error(ctx.sweep.gh(a, "alias", "delete", first, ok=None), first)
    expect_ok(ctx.sweep.gh(a, "alias", "delete", "--all"))
    assert not expect_ok(ctx.sweep.gh(a, "alias", "list")).stdout.strip()
    expect_ok(ctx.sweep.gh(a, "alias", "delete", "--all"))


@case(command="config get", variant="default/global/host and missing key")
def config_get(ctx):
    a = isolated(ctx)
    assert expect_ok(ctx.sweep.gh(a, "config", "get", "git_protocol")).stdout.strip() == "https"
    expect_ok(ctx.sweep.gh(a, "config", "set", "git_protocol", "ssh", "--host", HOST))
    assert expect_ok(ctx.sweep.gh(a, "config", "get", "git_protocol", "--host", HOST)).stdout.strip() == "ssh"
    assert expect_ok(ctx.sweep.gh(a, "config", "get", "git_protocol")).stdout.strip() == "https"
    client_error(ctx.sweep.gh(a, "config", "get", ctx.name("absent"), ok=None), "not found")


@case(command="config set", variant="global and host isolation; invalid enum")
def config_set(ctx):
    a = isolated(ctx)
    editor = ctx.name("editor")
    expect_ok(ctx.sweep.gh(a, "config", "set", "editor", editor))
    assert expect_ok(ctx.sweep.gh(a, "config", "get", "editor")).stdout.strip() == editor
    expect_ok(ctx.sweep.gh(a, "config", "set", "git_protocol", "ssh", "--host", HOST))
    assert expect_ok(ctx.sweep.gh(a, "config", "get", "git_protocol", "--host", HOST)).stdout.strip() == "ssh"
    client_error(ctx.sweep.gh(a, "config", "set", "git_protocol", "invalid-protocol", ok=None), "git_protocol")


@case(command="config list", variant="defaults and host overrides")
def config_list(ctx):
    a = isolated(ctx)
    assert "git_protocol=https" in expect_ok(ctx.sweep.gh(a, "config", "list")).stdout
    expect_ok(ctx.sweep.gh(a, "config", "set", "git_protocol", "ssh", "--host", HOST))
    assert "git_protocol=ssh" in expect_ok(ctx.sweep.gh(a, "config", "list", "--host", HOST)).stdout
    assert "git_protocol=https" in expect_ok(ctx.sweep.gh(a, "config", "list")).stdout


@case(command="config clear-cache", variant="existing cache and idempotence")
def config_clear_cache(ctx):
    a = isolated(ctx)
    cache = a.directory / "cache/gh"
    cache.mkdir()
    sentinel = cache / ctx.name("cache")
    sentinel.write_text("disposable cache")
    expect_ok(ctx.sweep.gh(a, "config", "clear-cache"))
    assert not sentinel.exists()
    expect_ok(ctx.sweep.gh(a, "config", "clear-cache"))
    assert not sentinel.exists()


@case(command="completion", variant="all shells and unsupported shell")
def completion(ctx):
    a = isolated(ctx)
    for shell, marker in (("bash", "__start_gh"), ("zsh", "#compdef gh"),
                          ("fish", "complete -c gh"), ("powershell", "Register-ArgumentCompleter")):
        output = expect_ok(ctx.sweep.gh(a, "completion", "--shell", shell)).stdout
        assert marker in output and len(output) > 100, output[:200]
    client_error(ctx.sweep.gh(a, "completion", "--shell", ctx.name("unknown"), ok=None), "shell")


@case(command="auth login", variant="PAT, HTTPS/SSH, skip key, invalid PAT and incompatible OAuth flags")
def auth_login(ctx):
    a = isolated(ctx)
    expect_ok(ctx.sweep.gh(a, "auth", "login", "--hostname", HOST, "--with-token",
                           "--git-protocol", "https", "--insecure-storage", input=ctx.account.token + "\n"))
    assert expect_ok(ctx.sweep.gh(a, "auth", "token", "--hostname", HOST)).stdout.strip() == ctx.account.token
    expect_ok(ctx.sweep.gh(a, "auth", "login", "--hostname", HOST, "--with-token",
                           "--git-protocol", "ssh", "--skip-ssh-key", "--insecure-storage", input=ctx.account.token + "\n"))
    assert expect_ok(ctx.sweep.gh(a, "config", "get", "git_protocol", "--host", HOST)).stdout.strip() == "ssh"
    bad = isolated(ctx)
    expect_error(ctx.sweep.gh(bad, "auth", "login", "--hostname", HOST, "--with-token",
                              "--insecure-storage", input=ctx.name("invalid-token"), ok=None), status=401)
    client_error(ctx.sweep.gh(bad, "auth", "login", "--hostname", HOST, "--with-token", "--web",
                              input=ctx.account.token, ok=None), "--web", "--with-token")
    client_error(ctx.sweep.gh(bad, "auth", "login", "--hostname", HOST, "--with-token", "--scopes", "repo",
                              input=ctx.account.token, ok=None), "--scopes", "--with-token")


@case(command="auth refresh", variant="noninteractive missing login")
def auth_refresh(ctx):
    # Successful refresh requires a human-approved OAuth device/browser flow.
    a = isolated(ctx)
    client_error(ctx.sweep.gh(a, "auth", "refresh", "--hostname", HOST, ok=None), "not logged")


@case(command="auth logout", variant="selected user, remaining user and unknown host")
def auth_logout(ctx):
    a = isolated(ctx)
    first, second = ctx.name("first"), ctx.name("second")
    local_hosts(a, {first: "local-first-token", second: "local-second-token"})
    expect_ok(ctx.sweep.gh(a, "auth", "logout", "--hostname", HOST, "--user", first))
    assert expect_ok(ctx.sweep.gh(a, "auth", "token", "--hostname", HOST, "--user", second)).stdout.strip() == "local-second-token"
    client_error(ctx.sweep.gh(a, "auth", "token", "--hostname", HOST, "--user", first, ok=None), "token")
    expect_ok(ctx.sweep.gh(a, "auth", "logout", "--hostname", HOST))
    client_error(ctx.sweep.gh(a, "auth", "logout", "--hostname", "missing.invalid", ok=None), "not logged")


@case(command="auth switch", variant="named, automatic two-account toggle, unknown user")
def auth_switch(ctx):
    a = isolated(ctx)
    first, second = ctx.name("first"), ctx.name("second")
    local_hosts(a, {first: "local-first-token", second: "local-second-token"})
    expect_ok(ctx.sweep.gh(a, "auth", "switch", "--hostname", HOST, "--user", second))
    assert expect_ok(ctx.sweep.gh(a, "auth", "token", "--hostname", HOST)).stdout.strip() == "local-second-token"
    expect_ok(ctx.sweep.gh(a, "auth", "switch", "--hostname", HOST))
    assert expect_ok(ctx.sweep.gh(a, "auth", "token", "--hostname", HOST)).stdout.strip() == "local-first-token"
    client_error(ctx.sweep.gh(a, "auth", "switch", "--hostname", HOST, "--user", ctx.name("missing"), ok=None), "account")


@case(command="auth token", variant="default, host/user, missing host/user")
def auth_token(ctx):
    a = isolated(ctx)
    name = ctx.name("user")
    local_hosts(a, {name: "local-test-token"})
    default = isolated(ctx)
    local_hosts(default, {name: "local-default-token"}, host="localhost.invalid")
    assert expect_ok(ctx.sweep.gh(default, "auth", "token")).stdout.strip() == "local-default-token"
    assert expect_ok(ctx.sweep.gh(a, "auth", "token", "--hostname", HOST)).stdout.strip() == "local-test-token"
    assert expect_ok(ctx.sweep.gh(a, "auth", "token", "--hostname", HOST, "--user", name)).stdout.strip() == "local-test-token"
    client_error(ctx.sweep.gh(a, "auth", "token", "--hostname", HOST, "--user", ctx.name("missing"), ok=None), "token")
    client_error(ctx.sweep.gh(a, "auth", "token", "--hostname", "missing.invalid", ok=None), "token")


@case(command="auth setup-git", variant="all hosts, selected host, forced unknown host, bad force")
def auth_setup_git(ctx):
    a = isolated(ctx)
    local_hosts(a, {ctx.name("user"): "local-test-token"})
    expect_ok(ctx.sweep.gh(a, "auth", "setup-git"))
    config = a.directory / "gitconfig"
    assert HOST in config.read_text() and "auth git-credential" in config.read_text()
    expect_ok(ctx.sweep.gh(a, "auth", "setup-git", "--hostname", HOST))
    assert HOST in config.read_text()
    client_error(ctx.sweep.gh(a, "auth", "setup-git", "--hostname", "missing.invalid", ok=None), "not logged")
    expect_ok(ctx.sweep.gh(a, "auth", "setup-git", "--hostname", "missing.invalid", "--force"))
    assert "missing.invalid" in config.read_text()
    client_error(ctx.sweep.gh(a, "auth", "setup-git", "--force", ok=None), "--hostname")


@case(command="auth status", variant="live account, active, text/JSON/token/jq/template")
def auth_status(ctx):
    result = expect_ok(ctx.gh("auth", "status", "--hostname", HOST, "--active"))
    assert ctx.account.name in result.stdout + result.stderr
    value = expect_json(ctx.gh("auth", "status", "--hostname", HOST, "--json", "hosts"), {"hosts": dict})
    rows = value["hosts"][HOST]
    assert any(row["login"] == ctx.account.name and row["state"] == "success" and row["active"] for row in rows), rows
    assert ctx.account.token not in json.dumps(value)
    shown = expect_json(ctx.gh("auth", "status", "--hostname", HOST, "--active", "--json", "hosts", "--show-token"), {"hosts": dict})
    assert shown["hosts"][HOST][0]["token"] == ctx.account.token
    output = expect_ok(ctx.gh("auth", "status", "--hostname", HOST, "--json", "hosts", "--jq", ".hosts[][] | .login")).stdout
    assert ctx.account.name in output.splitlines()
    output = expect_ok(ctx.gh("auth", "status", "--hostname", HOST, "--json", "hosts", "--template", "{{range .hosts}}{{range .}}{{.login}}{{\"\\n\"}}{{end}}{{end}}")).stdout
    assert ctx.account.name in output.splitlines()
    result = expect_ok(ctx.gh("auth", "status", "--hostname", HOST, "--show-token"))
    assert ctx.account.token in result.stdout + result.stderr


@case(command="auth status", variant="invalid PAT gives text error but JSON exit zero")
def auth_status_invalid(ctx):
    a = isolated(ctx)
    local_hosts(a, {ctx.name("user"): ctx.name("bad-token")})
    result = ctx.sweep.gh(a, "auth", "status", "--hostname", HOST, ok=None)
    client_error(result, "token")
    value = expect_json(ctx.sweep.gh(a, "auth", "status", "--hostname", HOST, "--json", "hosts"), {"hosts": dict})
    assert value["hosts"][HOST][0]["state"] == "error", value


@case(command="auth login", variant="second real identity; switch/token/logout remain config-local")
def auth_second_identity(ctx):
    a = isolated(ctx, authenticated=True)
    other = other_account(ctx)
    expect_ok(ctx.sweep.gh(a, "auth", "login", "--hostname", HOST, "--with-token", "--insecure-storage",
                           "--git-protocol", "https", input=other.token + "\n"))
    assert expect_json(ctx.sweep.gh(a, "api", "user"), {"login": str})["login"] == other.name
    expect_ok(ctx.sweep.gh(a, "auth", "switch", "--hostname", HOST, "--user", ctx.account.name))
    assert expect_json(ctx.sweep.gh(a, "api", "user"), {"login": str})["login"] == ctx.account.name
    expect_ok(ctx.sweep.gh(a, "auth", "logout", "--hostname", HOST, "--user", other.name))
    assert expect_ok(ctx.sweep.gh(a, "auth", "token", "--hostname", HOST)).stdout.strip() == ctx.account.token


@case(command="api", variant="GET hostname/header/preview, formatting, include, verbose and silent")
def api_output(ctx):
    expect_json(ctx.gh("api", "user", "--hostname", HOST, "--header", "Accept: application/vnd.github+json", "--preview", "mercy"), {"login": ctx.account.name, "id": int})
    assert expect_ok(ctx.gh("api", "user", "--jq", ".login")).stdout.strip() == ctx.account.name
    assert expect_ok(ctx.gh("api", "user", "--template", "{{.login}}\n")).stdout.strip() == ctx.account.name
    result = expect_ok(ctx.gh("api", "user", "--include"))
    assert "200" in result.stdout.splitlines()[0] and '"login"' in result.stdout, result.stdout
    result = expect_ok(ctx.gh("api", "user", "--verbose"))
    assert ctx.account.name in result.stdout and "GET /api/v3/user" in result.stderr and "200" in result.stderr, result
    assert expect_ok(ctx.gh("api", "user", "--silent")).stdout == ""
    result = expect_ok(ctx.gh("api", "user", "--silent", "--include"))
    assert "200" in result.stdout and '"login"' not in result.stdout, result.stdout


@case(command="api", variant="typed/raw/nested fields, method override and file/stdin input", needs=("repo",))
def api_payload(ctx):
    title = ctx.name("issue")
    created = expect_json(ctx.gh("api", f"repos/{ctx.repo}/issues", "--raw-field", "title=" + title,
                                 "--field", "body=@-", input="body from stdin"), {"title": title, "body": "body from stdin", "number": int})
    endpoint = f"repos/{ctx.repo}/issues/{created['number']}"
    expect_json(ctx.gh("api", endpoint, "--method", "PATCH", "--raw-field", "state=closed"), {"state": "closed"})
    name = ctx.name("typed")
    repo = expect_json(ctx.gh("api", "user/repos", "--raw-field", "name=" + name,
                              "--field", "private=true", "--field", "auto_init=true"), {"name": name, "private": True})
    expect_json(ctx.gh("api", "repos/" + repo["full_name"], "--method", "PATCH", "--field", "private=false"), {"private": False})
    payload = ctx.directory / ctx.name("json")
    payload.write_text(json.dumps({"title": ctx.name("file-issue"), "body": "file payload"}))
    expect_json(ctx.gh("api", f"repos/{ctx.repo}/issues", "--input", str(payload)), {"body": "file payload"})
    expect_json(ctx.gh("api", endpoint, "--method", "PATCH", "--input", "-", input='{"body":"stdin JSON"}'), {"body": "stdin JSON"})
    rows = expect_json(ctx.gh("api", f"repos/{ctx.repo}/issues", "--method", "GET", "--raw-field", "state=closed", "--field", "per_page=1"), [{"number": int, "state": "closed"}])
    assert [r["number"] for r in rows] == [created["number"]], rows


@case(command="api", variant="REST pagination/slurp and jq", needs=("repo",))
def api_pagination(ctx):
    ids = []
    for _ in range(3):
        ids.append(ctx.api("POST", f"repos/{ctx.repo}/issues", {"title": ctx.name("page")})["number"])
    endpoint = f"repos/{ctx.repo}/issues?per_page=1&state=all"
    pages = expect_json(ctx.gh("api", endpoint, "--paginate", "--slurp"), [[{"number": int}]])
    assert len(pages) == 3 and all(len(page) == 1 for page in pages), pages
    assert sorted(row["number"] for page in pages for row in page) == sorted(ids), pages
    result = expect_ok(ctx.gh("api", endpoint, "--paginate", "--jq", ".[].number"))
    assert sorted(map(int, result.stdout.splitlines())) == sorted(ids), result.stdout


@case(command="api", variant="GraphQL fields, variables and cursor pagination")
def api_graphql(ctx):
    expect_json(ctx.gh("api", "graphql", "--raw-field", "query=query { viewer { login } }"), {"data": {"viewer": {"login": ctx.account.name}}})
    org = ctx.org()
    names = [ctx.name("repo"), ctx.name("repo")]
    for name in names:
        ctx.sweep.create_repo(ctx.account, name, org=org)
    query = "query($owner:String!,$endCursor:String){organization(login:$owner){repositories(first:1,after:$endCursor){nodes{name} pageInfo{hasNextPage endCursor}}}}"
    pages = expect_json(ctx.gh("api", "graphql", "--raw-field", "query=" + query, "--field", "owner=" + org,
                               "--paginate", "--slurp"), [{"data": {"organization": {"repositories": dict}}}])
    actual = [r["name"] for p in pages for r in p["data"]["organization"]["repositories"]["nodes"]]
    assert sorted(actual) == sorted(names), pages


@case(command="api", variant="cache hit and terminal escape opt-in", needs=("repo",))
def api_cache_and_escape(ctx):
    endpoint = f"repos/{ctx.repo}"
    original = expect_json(ctx.gh("api", endpoint, "--cache", "1h"), {"id": int})
    description = ctx.name("description")
    ctx.api("PATCH", endpoint, {"description": description})
    assert expect_json(ctx.gh("api", endpoint, "--cache", "1h"), {"id": int})["description"] == original["description"]
    assert expect_json(ctx.gh("api", endpoint), {"id": int})["description"] == description
    ctx.api("PATCH", endpoint, {"description": "\x1b[31m" + description + "\x1b[0m"})
    safe = expect_ok(ctx.gh("api", endpoint, "--jq", ".description")).stdout
    allowed = expect_ok(ctx.gh("api", endpoint, "--jq", ".description", "--allow-escape-sequences")).stdout
    assert description in safe and "\x1b" not in safe, repr(safe)
    assert "\x1b[31m" in allowed and description in allowed, repr(allowed)


@case(command="api", variant="404, validation, invalid auth, private repo isolation and GHES cloud rejection", needs=("repo",))
def api_errors(ctx):
    expect_error(ctx.gh("api", f"repos/{ctx.account.name}/{ctx.name('missing')}", ok=None), contains="Not Found")
    expect_error(ctx.gh("api", f"repos/{ctx.repo}/issues", "--method", "POST", "--raw-field", "body=no title", ok=None), status=422)
    expect_error(ctx.gh("api", "user", "--header", "Authorization: Bearer " + ctx.name("bad-token"), ok=None), status=401)
    private = ctx.sweep.create_repo(ctx.account, ctx.name("private"), private=True)
    expect_error(ctx.sweep.gh(other_account(ctx), "api", "repos/" + private["full_name"], ok=None), status=404)
    expect_unsupported(ctx.gh("api", "user/codespaces", ok=None))
    client_error(ctx.gh("api", "user", "--jq", ".login", "--template", "{{.login}}", ok=None), "--jq", "--template")


@case(command="browse", variant="repository destinations and no-browser", needs=("repo",))
def browse_destinations(ctx):
    root = "/" + ctx.repo
    assert urlsplit(expect_ok(ctx.gh("browse", "--repo", ctx.repo, "--no-browser")).stdout.strip()).path.rstrip("/") == root
    for flag, suffix in (("--actions", "/actions"), ("--projects", "/projects"), ("--releases", "/releases"),
                         ("--settings", "/settings"), ("--wiki", "/wiki")):
        result = expect_ok(ctx.gh("browse", "--repo", ctx.repo, "--no-browser", flag))
        assert urlsplit(result.stdout.strip()).path.rstrip("/") == root + suffix, result.stdout
    missing = ctx.account.name + "/" + ctx.name("missing")
    # gh browse constructs a root URL locally; it does not assert the repo exists.
    assert urlsplit(expect_ok(ctx.gh("browse", "--repo", missing, "--no-browser")).stdout.strip()).path == "/" + missing


@case(command="browse", variant="issue, file, branch, blame, commit and invalid combinations", needs=("issue",))
def browse_content(ctx):
    root = "/" + ctx.repo
    repo = ctx.api("GET", f"repos/{ctx.repo}")
    branch = repo["default_branch"]
    sha = ctx.api("GET", f"repos/{ctx.repo}/git/ref/heads/{branch}")["object"]["sha"]
    result = expect_ok(ctx.gh("browse", str(ctx.issue["number"]), "--repo", ctx.repo, "--no-browser"))
    assert urlsplit(result.stdout.strip()).path == root + "/issues/" + str(ctx.issue["number"]), result.stdout
    result = expect_ok(ctx.gh("browse", "README.md", "--branch", branch, "--repo", ctx.repo, "--no-browser"))
    assert urlsplit(result.stdout.strip()).path == root + "/blob/" + branch + "/README.md", result.stdout
    result = expect_ok(ctx.gh("browse", "README.md:1", "--blame", "--branch", branch, "--repo", ctx.repo, "--no-browser"))
    assert urlsplit(result.stdout.strip()).path == root + "/blame/" + branch + "/README.md" and "L1" in result.stdout, result.stdout
    result = expect_ok(ctx.gh("browse", "--commit", sha, "--repo", ctx.repo, "--no-browser"))
    assert urlsplit(result.stdout.strip()).path == root + "/commit/" + sha, result.stdout
    result = expect_ok(ctx.gh("browse", "README.md", "--commit", sha, "--repo", ctx.repo, "--no-browser"))
    assert urlsplit(result.stdout.strip()).path == root + "/blob/" + sha + "/README.md", result.stdout
    client_error(ctx.gh("browse", "--branch", branch, "--commit", sha, "--repo", ctx.repo, "--no-browser", ok=None), "branch", "commit")


def read_only_account(ctx):
    token = ctx.native("form", "/settings?tab=tokens", "--action", "/settings/tokens",
                       "--has", "name", "--set", "name=" + ctx.name("read-token"),
                       "--set", "scope=read", "--set", "expiry=never", "--status", "200",
                       "--print", r"re:(?s)Copy this token now.*?(cy_pat_[0-9a-f]{64})")
    account = isolated(ctx)
    local_hosts(account, {ctx.account.name: token})
    return account


@case(command="ssh-key add", variant="authentication default/explicit and signing, file/stdin, duplicate and invalid input", timeout=180)
def ssh_key_add(ctx):
    file, text = ssh_material(ctx)
    title = ctx.name("authentication")
    expect_ok(ctx.gh("ssh-key", "add", str(file), "--title", title))
    row = next(r for r in ctx.api("GET", "user/keys") if r["title"] == title)
    assert row["key"] == " ".join(text.split()[:2]), row
    # gh treats uploading an identical existing key as an idempotent operation.
    expect_ok(ctx.gh("ssh-key", "add", str(file), "--title", title, "--type", "authentication"))
    assert len([r for r in ctx.api("GET", "user/keys") if r["key"] == row["key"]]) == 1
    signfile, signtext = ssh_material(ctx)
    signing_title = ctx.name("signing")
    expect_ok(ctx.gh("ssh-key", "add", "--title", signing_title, "--type", "signing", input=signtext))
    signing = next(r for r in ctx.api("GET", "user/ssh_signing_keys") if r["title"] == signing_title)
    assert signing["key"] == " ".join(signtext.split()[:2]), signing
    bad = ctx.directory / ctx.name("invalid-key")
    bad.write_text("not an SSH public key\n")
    expect_error(ctx.gh("ssh-key", "add", str(bad), "--title", ctx.name("bad"), ok=None), status=422)
    client_error(ctx.gh("ssh-key", "add", str(file), "--type", "unknown", ok=None), "type")
    # Clean up only this invocation's keys.
    ctx.api("DELETE", "user/keys/" + str(row["id"]))
    ctx.api("DELETE", "user/ssh_signing_keys/" + str(signing["id"]))


@case(command="ssh-key add", variant="read-only credential and another account's duplicate key", timeout=180)
def ssh_key_add_permissions(ctx):
    file, text = ssh_material(ctx)
    title = ctx.name("owned-key")
    row = ctx.api("POST", "user/keys", {"key": text, "title": title})
    expect_error(ctx.sweep.gh(other_account(ctx), "ssh-key", "add", str(file), "--title", ctx.name("foreign"), ok=None), status=422)
    file2, _ = ssh_material(ctx)
    expect_error(ctx.sweep.gh(read_only_account(ctx), "ssh-key", "add", str(file2), "--title", ctx.name("denied"), ok=None), status=403)
    ctx.api("DELETE", "user/keys/" + str(row["id"]))


@case(command="ssh-key list", variant="both authentication and signing keys with unique titles", timeout=180)
def ssh_key_list(ctx):
    _, key = ssh_material(ctx)
    title = ctx.name("auth")
    row = ctx.api("POST", "user/keys", {"key": key, "title": title})
    _, key2 = ssh_material(ctx)
    signing_title = ctx.name("sign")
    sign = ctx.api("POST", "user/ssh_signing_keys", {"key": key2, "title": signing_title})
    output = expect_ok(ctx.gh("ssh-key", "list")).stdout
    assert title in output and signing_title in output, output
    assert str(row["id"]) in output and str(sign["id"]) in output, output
    foreign = expect_ok(ctx.sweep.gh(other_account(ctx), "ssh-key", "list")).stdout
    assert title not in foreign and signing_title not in foreign, foreign
    ctx.api("DELETE", "user/keys/" + str(row["id"]))
    ctx.api("DELETE", "user/ssh_signing_keys/" + str(sign["id"]))
    output = expect_ok(ctx.gh("ssh-key", "list")).stdout
    assert title not in output and signing_title not in output, output


@case(command="ssh-key delete", variant="yes, repeat 404 and foreign ownership", timeout=180)
def ssh_key_delete(ctx):
    _, key = ssh_material(ctx)
    title = ctx.name("delete")
    row = ctx.api("POST", "user/keys", {"key": key, "title": title})
    key_id = str(row["id"])
    # Explicit endpoint avoids ambiguity between authentication/signing ID spaces
    # in another account; never risk deleting an unrelated lane's signing key.
    expect_error(ctx.sweep.gh(other_account(ctx), "api", "user/keys/" + key_id, "--method", "DELETE", ok=None), status=404)
    assert any(r["id"] == row["id"] for r in ctx.api("GET", "user/keys"))
    client_error(ctx.gh("ssh-key", "delete", key_id, ok=None), "--yes")
    expect_ok(ctx.gh("ssh-key", "delete", key_id, "--yes"))
    assert title not in expect_ok(ctx.gh("ssh-key", "list")).stdout
    expect_error(ctx.gh("ssh-key", "delete", key_id, "--yes", ok=None), status=404)


@case(command="ssh-key delete", variant="read-only credential cannot delete an owned key", timeout=180)
def ssh_key_delete_read_only(ctx):
    _, key = ssh_material(ctx)
    title = ctx.name("denied-delete")
    row = ctx.api("POST", "user/keys", {"key": key, "title": title})
    expect_error(ctx.sweep.gh(read_only_account(ctx), "ssh-key", "delete", str(row["id"]), "--yes", ok=None), status=403)
    assert any(r["id"] == row["id"] for r in ctx.api("GET", "user/keys"))
    expect_ok(ctx.gh("ssh-key", "delete", str(row["id"]), "--yes"))
    assert title not in expect_ok(ctx.gh("ssh-key", "list")).stdout


@case(command="gpg-key add", variant="valid self-signed unique certificate, file/stdin, title and duplicate", timeout=180)
def gpg_key_add(ctx):
    file, key_id = gpg_material(ctx)
    title = ctx.name("gpg-title")
    expect_ok(ctx.gh("gpg-key", "add", str(file), "--title", title))
    row = next(r for r in ctx.api("GET", "user/gpg_keys") if r["key_id"] == key_id)
    assert row["name"] == title and row["can_sign"] is True, row
    assert key_id in expect_ok(ctx.gh("gpg-key", "list")).stdout
    expect_error(ctx.gh("gpg-key", "add", "--title", ctx.name("duplicate"), input=file.read_text(), ok=None), status=422)
    ctx.api("DELETE", "user/gpg_keys/" + str(row["id"]))
    file2, key_id2 = gpg_material(ctx)
    expect_ok(ctx.gh("gpg-key", "add", input=file2.read_text()))
    row2 = next(r for r in ctx.api("GET", "user/gpg_keys") if r["key_id"] == key_id2)
    assert row2["key_id"] in expect_ok(ctx.gh("gpg-key", "list")).stdout
    ctx.api("DELETE", "user/gpg_keys/" + str(row2["id"]))


@case(command="gpg-key add", variant="bad armor and read-only credential", timeout=180)
def gpg_key_add_errors(ctx):
    bad = ctx.directory / ctx.name("invalid-gpg")
    bad.write_text("-----BEGIN PGP PUBLIC KEY BLOCK-----\n\ninvalid\n-----END PGP PUBLIC KEY BLOCK-----\n")
    expect_error(ctx.gh("gpg-key", "add", str(bad), "--title", ctx.name("bad"), ok=None), status=422)
    file, _ = gpg_material(ctx)
    expect_error(ctx.sweep.gh(read_only_account(ctx), "gpg-key", "add", str(file), "--title", ctx.name("denied"), ok=None), status=403)


@case(command="gpg-key list", variant="enrolled key visible only to owner; removal", timeout=180)
def gpg_key_list(ctx):
    file, key_id = gpg_material(ctx)
    row = ctx.api("POST", "user/gpg_keys", {"armored_public_key": file.read_text(), "name": ctx.name("gpg")})
    assert key_id in expect_ok(ctx.gh("gpg-key", "list")).stdout
    assert key_id not in expect_ok(ctx.sweep.gh(other_account(ctx), "gpg-key", "list")).stdout
    ctx.api("DELETE", "user/gpg_keys/" + str(row["id"]))
    assert key_id not in expect_ok(ctx.gh("gpg-key", "list")).stdout


@case(command="gpg-key delete", variant="key ID resolution, yes, read-only denial and missing key", timeout=180)
def gpg_key_delete(ctx):
    file, key_id = gpg_material(ctx)
    row = ctx.api("POST", "user/gpg_keys", {"armored_public_key": file.read_text(), "name": ctx.name("delete-gpg")})
    expect_error(ctx.sweep.gh(read_only_account(ctx), "gpg-key", "delete", key_id, "--yes", ok=None), status=403)
    # gh resolves a fingerprint-style key ID from the authenticated user's list.
    client_error(ctx.sweep.gh(other_account(ctx), "gpg-key", "delete", key_id, "--yes", ok=None), "key")
    assert any(r["id"] == row["id"] for r in ctx.api("GET", "user/gpg_keys"))
    client_error(ctx.gh("gpg-key", "delete", key_id, ok=None), "--yes")
    expect_ok(ctx.gh("gpg-key", "delete", key_id, "--yes"))
    assert key_id not in expect_ok(ctx.gh("gpg-key", "list")).stdout
    client_error(ctx.gh("gpg-key", "delete", key_id, "--yes", ok=None), "key")


def code_fixture(ctx):
    token = ctx.name("needle")
    filename = ctx.name("source") + ".py"
    content = ("# " + token + "\nprint('" + token + "')\n").encode()
    ctx.api("PUT", f"repos/{ctx.repo}/contents/{filename}", {
        "message": ctx.name("commit"), "content": base64.b64encode(content).decode(),
    })
    return token, filename


def commit_fixture(ctx):
    message = ctx.name("commit-message")
    branch = ctx.api("GET", f"repos/{ctx.repo}")["default_branch"]
    parent = ctx.api("GET", f"repos/{ctx.repo}/git/ref/heads/{branch}")["object"]["sha"]
    person = {"name": ctx.account.name, "email": ctx.account.name + "@example.test"}
    row = ctx.api("PUT", f"repos/{ctx.repo}/contents/{ctx.name('file')}.txt", {
        "message": message, "content": base64.b64encode(b"commit search fixture\n").decode(),
        "author": person, "committer": person,
    })
    commit = ctx.api("GET", f"repos/{ctx.repo}/git/commits/{row['commit']['sha']}")
    return message, commit["sha"], parent, commit["tree"]["sha"]


def discussion_fixture(ctx, pull=False):
    row = ctx.pr if pull else ctx.issue
    label = ctx.name("label")
    milestone = ctx.api("POST", f"repos/{ctx.repo}/milestones", {"title": ctx.name("milestone")})
    ctx.api("POST", f"repos/{ctx.repo}/labels", {"name": label, "color": "12ab34"})
    endpoint = f"repos/{ctx.repo}/issues/{row['number']}"
    ctx.api("PATCH", endpoint, {"body": "sweep @" + ctx.account.name,
                                "assignees": [ctx.account.name], "labels": [label],
                                "milestone": milestone["number"]})
    ctx.api("POST", endpoint + "/comments", {"body": "comment by @" + ctx.account.name})
    ctx.api("POST", endpoint + "/reactions", {"content": "+1"})
    return row["number"], label, milestone["title"]


@case(command="search code", variant="positive/negative text; each code qualifier; JSON/jq/template/table", needs=("repo",), timeout=180)
def search_code(ctx):
    token, filename = code_fixture(ctx)
    paths(ctx.gh("search", "code", token, "--repo", ctx.repo, "--json", "path,repository,sha,url,textMatches"), [filename])
    for args, wanted in (
        (("--extension", "py"), [filename]), (("--extension", "rs"), []),
        (("--filename", filename), [filename]), (("--filename", ctx.name("missing")), []),
        (("--language", "python"), [filename]), (("--language", "rust"), []),
        (("--match", "file"), [filename]), (("--match", "path"), []),
        (("--owner", ctx.account.name), [filename]), (("--owner", other_account(ctx).name), []),
        (("--size", "<1000"), [filename]), (("--size", ">1000000"), []),
        (("--limit", "1"), [filename]),
    ):
        paths(ctx.gh("search", "code", token, "--repo", ctx.repo, *args, "--json", "path"), wanted)
    assert expect_ok(ctx.gh("search", "code", token, "--repo", ctx.repo, "--json", "path", "--jq", ".[0].path")).stdout.strip() == filename
    assert expect_ok(ctx.gh("search", "code", token, "--repo", ctx.repo, "--json", "path", "--template", "{{range .}}{{.path}}{{end}}")).stdout == filename
    assert filename in expect_ok(ctx.gh("search", "code", token, "--repo", ctx.repo)).stdout
    paths(ctx.gh("search", "code", ctx.name("absent"), "--repo", ctx.repo, "--json", "path"), [])
    client_error(ctx.gh("search", "code", token, "--limit", "0", ok=None), "limit")
    client_error(ctx.gh("search", "code", token, "--match", "invalid", ok=None), "match")


@case(command="search code", variant="private code absent from other account's global results", timeout=180)
def search_code_private(ctx):
    row = ctx.sweep.create_repo(ctx.account, ctx.name("private"), private=True)
    ctx.fixtures["repo"] = row["full_name"]
    token, filename = code_fixture(ctx)
    paths(ctx.gh("search", "code", token, "--repo", ctx.repo, "--json", "path"), [filename])
    paths(ctx.sweep.gh(other_account(ctx), "search", "code", token, "--owner", ctx.account.name, "--json", "path"), [])


@case(command="search commits", variant="each author/committer/date/hash/tree/parent qualifier", needs=("repo",), timeout=180)
def search_commits(ctx):
    message, sha, parent, tree = commit_fixture(ctx)
    for args, wanted in (
        ((), [sha]), (("--author", ctx.account.name), [sha]),
        (("--author-name", ctx.account.name), [sha]),
        (("--author-email", ctx.account.name + "@example.test"), [sha]),
        (("--author-date", ">=2000-01-01"), [sha]), (("--author-date", "<2000-01-01"), []),
        (("--committer", ctx.account.name), [sha]), (("--committer-name", ctx.account.name), [sha]),
        (("--committer-email", ctx.account.name + "@example.test"), [sha]),
        (("--committer-date", ">=2000-01-01"), [sha]), (("--committer-date", "<2000-01-01"), []),
        (("--hash", sha), [sha]), (("--hash", "0" * 40), []),
        (("--parent", parent), [sha]), (("--parent", "0" * 40), []),
        (("--tree", tree), [sha]), (("--tree", "0" * 40), []),
        (("--merge=false",), [sha]), (("--merge",), []),
        (("--visibility", "public"), [sha]), (("--visibility", "private"), []),
        (("--owner", ctx.account.name), [sha]), (("--owner", other_account(ctx).name), []),
    ):
        rows = expect_json(ctx.gh("search", "commits", message, "--repo", ctx.repo, *args, "--json", "sha"), [{"sha": str}])
        assert [r["sha"] for r in rows] == wanted, (args, rows)
    rows = expect_json(ctx.gh("search", "commits", message, "--repo", ctx.repo, "--limit", "1", "--sort", "author-date", "--order", "asc", "--json", "sha,author,committer,commit,parents,repository,url"), [{"sha": sha, "commit": dict}])
    assert len(rows) == 1, rows
    for sort in ("author-date", "committer-date"):
        assert expect_ok(ctx.gh("search", "commits", message, "--repo", ctx.repo, "--sort", sort, "--order", "desc", "--json", "sha", "--jq", ".[0].sha")).stdout.strip() == sha
    assert expect_ok(ctx.gh("search", "commits", message, "--repo", ctx.repo, "--json", "sha", "--template", "{{range .}}{{.sha}}{{end}}")).stdout == sha
    assert message in expect_ok(ctx.gh("search", "commits", message, "--repo", ctx.repo)).stdout
    client_error(ctx.gh("search", "commits", message, "--sort", "invalid", ok=None), "sort")
    expect_error(ctx.gh("search", "commits", message, "--repo", ctx.repo, "--author-date", "not-a-date", ok=None), status=422)


@case(command="search commits", variant="private commit visibility", timeout=180)
def search_commits_private(ctx):
    row = ctx.sweep.create_repo(ctx.account, ctx.name("private"), private=True)
    ctx.fixtures["repo"] = row["full_name"]
    message, sha, _, _ = commit_fixture(ctx)
    assert expect_ok(ctx.gh("search", "commits", message, "--repo", ctx.repo, "--visibility", "private", "--json", "sha", "--jq", ".[0].sha")).stdout.strip() == sha
    expect_json(ctx.sweep.gh(other_account(ctx), "search", "commits", message, "--owner", ctx.account.name, "--json", "sha"), [])


@case(command="search issues", variant="each identity, count, date, ownership and visibility qualifier", needs=("issue",), timeout=240)
def search_issues_filters(ctx):
    number, label, milestone = discussion_fixture(ctx)
    for args, wanted in (
        (("--author", ctx.account.name), [number]), (("--author", other_account(ctx).name), []),
        (("--assignee", "@me"), [number]), (("--assignee", other_account(ctx).name), []),
        (("--commenter", ctx.account.name), [number]), (("--commenter", other_account(ctx).name), []),
        (("--involves", ctx.account.name), [number]), (("--involves", other_account(ctx).name), []),
        (("--mentions", ctx.account.name), [number]), (("--mentions", other_account(ctx).name), []),
        (("--app", ctx.name("missing-app")), []),
        (("--label", label), [number]), (("--label", ctx.name("missing-label")), []),
        (("--milestone", milestone), [number]), (("--milestone", ctx.name("missing-milestone")), []),
        (("--comments", ">=1"), [number]), (("--comments", ">1000000"), []),
        (("--reactions", ">=1"), [number]), (("--reactions", ">1000000"), []),
        (("--interactions", ">=2"), [number]), (("--interactions", ">1000000"), []),
        (("--created", ">=2000-01-01"), [number]), (("--created", "<2000-01-01"), []),
        (("--updated", ">=2000-01-01"), [number]), (("--updated", "<2000-01-01"), []),
        (("--closed", ">=2000-01-01"), []),
        (("--archived=false",), [number]), (("--archived",), []),
        (("--locked=false",), [number]), (("--locked",), []),
        (("--state", "open"), [number]), (("--state", "closed"), []),
        (("--owner", ctx.account.name), [number]), (("--owner", other_account(ctx).name), []),
        (("--visibility", "public"), [number]), (("--visibility", "private"), []),
        (("--visibility", "internal"), []),
        (("--project", ctx.account.name + "/1"), []),
        (("--team-mentions", ctx.name("org") + "/" + ctx.name("team")), []),
        (("--language", "nonexistent-language"), []),
        (("--search-type", "lexical"), [number]),
    ):
        numbers(ctx.gh("search", "issues", "--repo", ctx.repo, *args, "--json", "number"), wanted)
    ctx.api("PUT", f"repos/{ctx.repo}/issues/{number}/lock", {"lock_reason": "resolved"})
    numbers(ctx.gh("search", "issues", "--repo", ctx.repo, "--locked", "--json", "number"), [number])
    numbers(ctx.gh("search", "issues", "--repo", ctx.repo, "--locked=false", "--json", "number"), [])
    ctx.api("PATCH", f"repos/{ctx.repo}/issues/{number}", {"state": "closed"})
    numbers(ctx.gh("search", "issues", "--repo", ctx.repo, "--state", "closed", "--closed", ">=2000-01-01", "--json", "number"), [number])
    ctx.api("PATCH", f"repos/{ctx.repo}", {"archived": True})
    numbers(ctx.gh("search", "issues", "--repo", ctx.repo, "--archived", "--json", "number"), [number])


@case(command="search issues", variant="each absence qualifier; include PRs; field matching and formatting", needs=("issue",), timeout=180)
def search_issues_output(ctx):
    number = ctx.issue["number"]
    for flag in ("--no-assignee", "--no-label", "--no-milestone", "--no-project"):
        numbers(ctx.gh("search", "issues", "--repo", ctx.repo, flag, "--json", "number"), [number])
    _, _, _ = discussion_fixture(ctx)
    for flag in ("--no-assignee", "--no-label", "--no-milestone"):
        numbers(ctx.gh("search", "issues", "--repo", ctx.repo, flag, "--json", "number"), [])
    numbers(ctx.gh("search", "issues", ctx.issue["title"], "--repo", ctx.repo, "--match", "title", "--json", "number"), [number])
    numbers(ctx.gh("search", "issues", "sweep", "--repo", ctx.repo, "--match", "body", "--json", "number"), [number])
    numbers(ctx.gh("search", "issues", "comment", "--repo", ctx.repo, "--match", "comments", "--json", "number"), [number])
    numbers(ctx.gh("search", "issues", ctx.name("absent"), "--repo", ctx.repo, "--json", "number"), [])
    fields = "number,title,state,author,assignees,labels,body,isPullRequest,isLocked,commentsCount,repository,url,createdAt,updatedAt"
    rows = expect_json(ctx.gh("search", "issues", "--repo", ctx.repo, "--json", fields), [{"number": number, "title": ctx.issue["title"], "isPullRequest": False, "commentsCount": 1}])
    assert len(rows) == 1, rows
    assert expect_ok(ctx.gh("search", "issues", "--repo", ctx.repo, "--json", "number", "--jq", ".[0].number")).stdout.strip() == str(number)
    assert expect_ok(ctx.gh("search", "issues", "--repo", ctx.repo, "--json", "title", "--template", "{{range .}}{{.title}}{{end}}")).stdout == ctx.issue["title"]
    assert ctx.issue["title"] in expect_ok(ctx.gh("search", "issues", "--repo", ctx.repo)).stdout
    pull = ctx.pr
    numbers(ctx.gh("search", "issues", "--repo", ctx.repo, "--include-prs", "--json", "number"), [number, pull["number"]])
    numbers(ctx.gh("search", "issues", "--repo", ctx.repo, "--json", "number"), [number])


@case(command="search issues", variant="sort/order/limit and each sort key", needs=("issue",), timeout=180)
def search_issues_sort(ctx):
    first, _, _ = discussion_fixture(ctx)
    second = ctx.api("POST", f"repos/{ctx.repo}/issues", {"title": ctx.name("second")})["number"]
    for order, expected in (("desc", [first, second]), ("asc", [second, first])):
        rows = expect_json(ctx.gh("search", "issues", "--repo", ctx.repo, "--sort", "comments", "--order", order, "--json", "number"), [{"number": int}])
        assert [r["number"] for r in rows] == expected, rows
    numbers(ctx.gh("search", "issues", "--repo", ctx.repo, "--sort", "comments", "--order", "desc", "--limit", "1", "--json", "number"), [first])
    for sort in ("created", "updated", "interactions", "reactions", "reactions-+1", "reactions--1",
                 "reactions-heart", "reactions-smile", "reactions-tada", "reactions-thinking_face"):
        numbers(ctx.gh("search", "issues", "--repo", ctx.repo, "--sort", sort, "--order", "asc", "--json", "number"), [first, second])


@case(command="search issues", variant="real visible team mention", needs=("issue",))
def search_issues_team(ctx):
    org = ctx.org()
    team = ctx.team(org)
    slug = org + "/" + team["slug"]
    ctx.api("PATCH", f"repos/{ctx.repo}/issues/{ctx.issue['number']}", {"body": "ping @" + slug})
    numbers(ctx.gh("search", "issues", "--repo", ctx.repo, "--team-mentions", slug, "--json", "number"), [ctx.issue["number"]])


@case(command="search issues", variant="GHES semantic/hybrid rejection, invalid enum/range and private visibility", timeout=180)
def search_issues_errors(ctx):
    for search_type in ("semantic", "hybrid"):
        client_error(ctx.gh("search", "issues", ctx.name("query"), "--search-type", search_type, ok=None), "Enterprise")
    client_error(ctx.gh("search", "issues", "--state", "invalid", ok=None), "state")
    expect_error(ctx.gh("search", "issues", "--repo", ctx.repo, "--comments", "not-a-number", ok=None), status=422)
    row = ctx.sweep.create_repo(ctx.account, ctx.name("private"), private=True)
    title = ctx.name("private-issue")
    issue = ctx.api("POST", "repos/" + row["full_name"] + "/issues", {"title": title})
    numbers(ctx.gh("search", "issues", title, "--repo", row["full_name"], "--visibility", "private", "--json", "number"), [issue["number"]])
    numbers(ctx.sweep.gh(other_account(ctx), "search", "issues", title, "--owner", ctx.account.name, "--json", "number"), [])


@case(command="search prs", variant="each common identity/count/date/ownership/visibility qualifier", needs=("pr",), timeout=240)
def search_prs_filters(ctx):
    number, label, milestone = discussion_fixture(ctx, pull=True)
    for args, wanted in (
        (("--author", ctx.account.name), [number]), (("--author", other_account(ctx).name), []),
        (("--assignee", "@me"), [number]), (("--assignee", other_account(ctx).name), []),
        (("--commenter", ctx.account.name), [number]), (("--commenter", other_account(ctx).name), []),
        (("--involves", ctx.account.name), [number]), (("--involves", other_account(ctx).name), []),
        (("--mentions", ctx.account.name), [number]), (("--mentions", other_account(ctx).name), []),
        (("--app", ctx.name("missing-app")), []),
        (("--label", label), [number]), (("--label", ctx.name("missing-label")), []),
        (("--milestone", milestone), [number]), (("--milestone", ctx.name("missing-milestone")), []),
        (("--comments", ">=1"), [number]), (("--comments", ">1000000"), []),
        (("--reactions", ">=1"), [number]), (("--reactions", ">1000000"), []),
        (("--interactions", ">=2"), [number]), (("--interactions", ">1000000"), []),
        (("--created", ">=2000-01-01"), [number]), (("--created", "<2000-01-01"), []),
        (("--updated", ">=2000-01-01"), [number]), (("--updated", "<2000-01-01"), []),
        (("--closed", ">=2000-01-01"), []),
        (("--archived=false",), [number]), (("--archived",), []),
        (("--locked=false",), [number]), (("--locked",), []),
        (("--state", "open"), [number]), (("--state", "closed"), []),
        (("--owner", ctx.account.name), [number]), (("--owner", other_account(ctx).name), []),
        (("--visibility", "public"), [number]), (("--visibility", "private"), []),
        (("--visibility", "internal"), []),
        (("--project", ctx.account.name + "/1"), []),
        (("--team-mentions", ctx.name("org") + "/" + ctx.name("team")), []),
        (("--language", "nonexistent-language"), []),
        (("--base", ctx.pr["base"]["ref"]), [number]), (("--base", ctx.name("absent-base")), []),
        (("--head", ctx.pr["head"]["ref"]), [number]), (("--head", ctx.name("absent-head")), []),
        (("--draft=false",), [number]), (("--draft",), []),
        (("--merged=false",), [number]), (("--merged",), []),
        (("--merged-at", ">=2000-01-01"), []),
    ):
        numbers(ctx.gh("search", "prs", "--repo", ctx.repo, *args, "--json", "number"), wanted)
    ctx.api("PUT", f"repos/{ctx.repo}/issues/{number}/lock", {"lock_reason": "resolved"})
    numbers(ctx.gh("search", "prs", "--repo", ctx.repo, "--locked", "--json", "number"), [number])
    ctx.api("PATCH", f"repos/{ctx.repo}/pulls/{number}", {"state": "closed"})
    numbers(ctx.gh("search", "prs", "--repo", ctx.repo, "--state", "closed", "--closed", ">=2000-01-01", "--json", "number"), [number])
    ctx.api("PATCH", f"repos/{ctx.repo}", {"archived": True})
    numbers(ctx.gh("search", "prs", "--repo", ctx.repo, "--archived", "--json", "number"), [number])


@case(command="search prs", variant="absence, match, limit, JSON/jq/template/table and sort keys", needs=("pr",), timeout=180)
def search_prs_output(ctx):
    number = ctx.pr["number"]
    for flag in ("--no-assignee", "--no-label", "--no-milestone", "--no-project"):
        numbers(ctx.gh("search", "prs", "--repo", ctx.repo, flag, "--json", "number"), [number])
    discussion_fixture(ctx, pull=True)
    for flag in ("--no-assignee", "--no-label", "--no-milestone"):
        numbers(ctx.gh("search", "prs", "--repo", ctx.repo, flag, "--json", "number"), [])
    numbers(ctx.gh("search", "prs", ctx.pr["title"], "--repo", ctx.repo, "--match", "title", "--json", "number"), [number])
    numbers(ctx.gh("search", "prs", "sweep", "--repo", ctx.repo, "--match", "body", "--json", "number"), [number])
    numbers(ctx.gh("search", "prs", "comment", "--repo", ctx.repo, "--match", "comments", "--json", "number"), [number])
    fields = "number,title,state,isDraft,isPullRequest,isLocked,author,assignees,labels,body,commentsCount,repository,url"
    rows = expect_json(ctx.gh("search", "prs", "--repo", ctx.repo, "--limit", "1", "--json", fields), [{"number": number, "title": ctx.pr["title"], "isDraft": False, "isPullRequest": True, "commentsCount": 1}])
    assert len(rows) == 1, rows
    assert expect_ok(ctx.gh("search", "prs", "--repo", ctx.repo, "--json", "number", "--jq", ".[0].number")).stdout.strip() == str(number)
    assert expect_ok(ctx.gh("search", "prs", "--repo", ctx.repo, "--json", "title", "--template", "{{range .}}{{.title}}{{end}}")).stdout == ctx.pr["title"]
    assert ctx.pr["title"] in expect_ok(ctx.gh("search", "prs", "--repo", ctx.repo)).stdout
    for sort in ("comments", "created", "updated", "interactions", "reactions", "reactions-+1", "reactions--1",
                 "reactions-heart", "reactions-smile", "reactions-tada", "reactions-thinking_face"):
        for order in ("asc", "desc"):
            numbers(ctx.gh("search", "prs", "--repo", ctx.repo, "--sort", sort, "--order", order, "--json", "number"), [number])
    numbers(ctx.gh("search", "prs", ctx.name("absent"), "--repo", ctx.repo, "--json", "number"), [])
    client_error(ctx.gh("search", "prs", "--review", "invalid", ok=None), "review")
    expect_error(ctx.gh("search", "prs", "--repo", ctx.repo, "--comments", "not-a-number", ok=None), status=422)


def collaborator(ctx, account):
    invitation = ctx.api("PUT", f"repos/{ctx.repo}/collaborators/{account.name}", {"permission": "push"})
    if invitation:
        ctx.sweep.api(account, "PATCH", "user/repository_invitations/" + str(invitation["id"]))


@case(command="search prs", variant="checks pending/success/failure and second-account review lifecycle", needs=("pr",), timeout=180)
def search_prs_reviews(ctx):
    number = ctx.pr["number"]
    other = other_account(ctx)
    collaborator(ctx, other)
    numbers(ctx.gh("search", "prs", "--repo", ctx.repo, "--review", "none", "--json", "number"), [number])
    numbers(ctx.gh("search", "prs", "--repo", ctx.repo, "--review", "approved", "--json", "number"), [])
    numbers(ctx.gh("search", "prs", "--repo", ctx.repo, "--review", "required", "--json", "number"), [])
    ctx.api("POST", f"repos/{ctx.repo}/pulls/{number}/requested_reviewers", {"reviewers": [other.name]})
    numbers(ctx.gh("search", "prs", "--repo", ctx.repo, "--review", "required", "--json", "number"), [number])
    numbers(ctx.gh("search", "prs", "--repo", ctx.repo, "--review-requested", other.name, "--json", "number"), [number])
    numbers(ctx.sweep.gh(other, "search", "prs", "--repo", ctx.repo, "--review-requested", "@me", "--json", "number"), [number])
    numbers(ctx.gh("search", "prs", "--repo", ctx.repo, "--reviewed-by", other.name, "--json", "number"), [])
    ctx.sweep.api(other, "POST", f"repos/{ctx.repo}/pulls/{number}/reviews", {"event": "APPROVE", "body": ctx.name("approval")})
    numbers(ctx.gh("search", "prs", "--repo", ctx.repo, "--reviewed-by", other.name, "--json", "number"), [number])
    numbers(ctx.gh("search", "prs", "--repo", ctx.repo, "--review", "approved", "--json", "number"), [number])
    ctx.sweep.api(other, "POST", f"repos/{ctx.repo}/pulls/{number}/reviews", {"event": "REQUEST_CHANGES", "body": ctx.name("changes")})
    numbers(ctx.gh("search", "prs", "--repo", ctx.repo, "--review", "changes_requested", "--json", "number"), [number])
    numbers(ctx.gh("search", "prs", "--repo", ctx.repo, "--review", "approved", "--json", "number"), [])
    context = ctx.name("check")
    for state, checks in (("pending", "pending"), ("success", "success"), ("failure", "failure")):
        ctx.api("POST", f"repos/{ctx.repo}/statuses/{ctx.pr['head']['sha']}", {"state": state, "context": context})
        numbers(ctx.gh("search", "prs", "--repo", ctx.repo, "--checks", checks, "--json", "number"), [number])
        mismatch = "failure" if checks == "success" else "success"
        numbers(ctx.gh("search", "prs", "--repo", ctx.repo, "--checks", mismatch, "--json", "number"), [])


@case(command="search prs", variant="draft and merged/merged-at; team mention", needs=("pr",), timeout=180)
def search_prs_states(ctx):
    number = ctx.pr["number"]
    org = ctx.org()
    team = ctx.team(org)
    slug = org + "/" + team["slug"]
    ctx.api("PATCH", f"repos/{ctx.repo}/pulls/{number}", {"body": "ping @" + slug})
    numbers(ctx.gh("search", "prs", "--repo", ctx.repo, "--team-mentions", slug, "--json", "number"), [number])
    merged = ctx.api("PUT", f"repos/{ctx.repo}/pulls/{number}/merge", {"merge_method": "merge"})
    assert merged["merged"] is True, merged
    numbers(ctx.gh("search", "prs", "--repo", ctx.repo, "--merged", "--json", "number"), [number])
    numbers(ctx.gh("search", "prs", "--repo", ctx.repo, "--merged=false", "--json", "number"), [])
    numbers(ctx.gh("search", "prs", "--repo", ctx.repo, "--merged-at", ">=2000-01-01", "--json", "number"), [number])
    numbers(ctx.gh("search", "prs", "--repo", ctx.repo, "--merged-at", "<2000-01-01", "--json", "number"), [])
    # A fresh branch is necessary after the first PR is merged.
    branch = ctx.name("draft-head")
    base = ctx.pr["base"]["ref"]
    sha = ctx.api("GET", f"repos/{ctx.repo}/git/ref/heads/{base}")["object"]["sha"]
    ctx.api("POST", f"repos/{ctx.repo}/git/refs", {"ref": "refs/heads/" + branch, "sha": sha})
    ctx.api("PUT", f"repos/{ctx.repo}/contents/{ctx.name('draft-file')}.txt", {
        "branch": branch, "message": ctx.name("draft-commit"), "content": base64.b64encode(b"draft\n").decode(),
    })
    draft = ctx.api("POST", f"repos/{ctx.repo}/pulls", {"title": ctx.name("draft-pr"), "head": branch, "base": base, "draft": True})
    numbers(ctx.gh("search", "prs", "--repo", ctx.repo, "--draft", "--json", "number"), [draft["number"]])
    numbers(ctx.gh("search", "prs", "--repo", ctx.repo, "--draft=false", "--json", "number"), [number])


@case(command="search prs", variant="private PR hidden from second identity", timeout=180)
def search_prs_private(ctx):
    row = ctx.sweep.create_repo(ctx.account, ctx.name("private"), private=True)
    ctx.fixtures["repo"] = row["full_name"]
    title = ctx.pr["title"]
    numbers(ctx.gh("search", "prs", title, "--repo", ctx.repo, "--visibility", "private", "--json", "number"), [ctx.pr["number"]])
    numbers(ctx.sweep.gh(other_account(ctx), "search", "prs", title, "--owner", ctx.account.name, "--json", "number"), [])


MIT_LICENSE = """MIT License

Copyright (c) 2026 Sweep fixture

Permission is hereby granted, free of charge, to any person obtaining a copy
of this software and associated documentation files (the "Software"), to deal
in the Software without restriction, including without limitation the rights
to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
copies of the Software, and to permit persons to whom the Software is
furnished to do so, subject to the following conditions:

The above copyright notice and this permission notice shall be included in all
copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
SOFTWARE.
"""


def repository_names(result, expected):
    rows = expect_json(result, [{"fullName": str}])
    assert sorted(row["fullName"] for row in rows) == sorted(expected), rows


@case(command="search repos", variant="each metadata/count/date/topic/license/language qualifier", needs=("repo",), timeout=240)
def search_repos_filters(ctx):
    name = ctx.repo.split("/")[1]
    topic = ctx.name("topic")
    ctx.api("PATCH", f"repos/{ctx.repo}", {"description": name})
    ctx.api("PUT", f"repos/{ctx.repo}/topics", {"names": [topic]})
    ctx.api("PUT", f"repos/{ctx.repo}/contents/LICENSE", {
        "message": ctx.name("license"), "content": base64.b64encode(MIT_LICENSE.encode()).decode(),
    })
    code_fixture(ctx)
    for args, wanted in (
        (("--owner", ctx.account.name), [ctx.repo]), (("--owner", other_account(ctx).name), []),
        (("--archived=false",), [ctx.repo]), (("--archived",), []),
        (("--created", ">=2000-01-01"), [ctx.repo]), (("--created", "<2000-01-01"), []),
        (("--updated", ">=2000-01-01"), [ctx.repo]), (("--updated", "<2000-01-01"), []),
        (("--followers", ">=0"), [ctx.repo]), (("--followers", ">1000000"), []),
        (("--forks", "0"), [ctx.repo]), (("--forks", ">1000000"), []),
        (("--good-first-issues", "0"), [ctx.repo]), (("--good-first-issues", ">1000000"), []),
        (("--help-wanted-issues", "0"), [ctx.repo]), (("--help-wanted-issues", ">1000000"), []),
        (("--include-forks", "true"), [ctx.repo]), (("--include-forks", "false"), [ctx.repo]),
        (("--include-forks", "only"), []),
        (("--language", "python"), [ctx.repo]), (("--language", "rust"), []),
        (("--license", "mit"), [ctx.repo]), (("--license", "gpl-3.0"), []),
        (("--match", "name"), [ctx.repo]), (("--match", "description"), [ctx.repo]),
        (("--match", "readme"), [ctx.repo]),
        (("--number-topics", "1"), [ctx.repo]), (("--number-topics", ">1000"), []),
        (("--size", ">=0"), [ctx.repo]), (("--size", ">1000000000"), []),
        (("--stars", "0"), [ctx.repo]), (("--stars", ">1000000"), []),
        (("--topic", topic), [ctx.repo]), (("--topic", ctx.name("absent-topic")), []),
        (("--visibility", "public"), [ctx.repo]), (("--visibility", "private"), []),
        (("--visibility", "internal"), []),
        (("--limit", "1"), [ctx.repo]),
    ):
        repository_names(ctx.gh("search", "repos", name, *args, "--json", "fullName"), wanted)
    # Give the label-based counts a positive, nonzero case too.
    for label in ("good first issue", "help wanted"):
        ctx.api("POST", f"repos/{ctx.repo}/labels", {"name": label, "color": "123456"})
    ctx.api("POST", f"repos/{ctx.repo}/issues", {"title": ctx.name("triage"), "labels": ["good first issue", "help wanted"]})
    repository_names(ctx.gh("search", "repos", name, "--good-first-issues", "1", "--json", "fullName"), [ctx.repo])
    repository_names(ctx.gh("search", "repos", name, "--help-wanted-issues", "1", "--json", "fullName"), [ctx.repo])
    ctx.api("PATCH", f"repos/{ctx.repo}", {"archived": True})
    repository_names(ctx.gh("search", "repos", name, "--archived", "--json", "fullName"), [ctx.repo])


@case(command="search repos", variant="JSON fields, jq/template/table, empty, bad enums/ranges and private isolation", needs=("repo",), timeout=180)
def search_repos_output(ctx):
    name = ctx.repo.split("/")[1]
    rows = expect_json(ctx.gh("search", "repos", name, "--owner", ctx.account.name, "--json", "fullName,name,isPrivate,isFork,isArchived,visibility,owner,url,stargazersCount,forksCount,defaultBranch,description,license,language"), [{"fullName": ctx.repo, "name": name, "isPrivate": False, "isFork": False, "isArchived": False}])
    assert len(rows) == 1, rows
    assert expect_ok(ctx.gh("search", "repos", name, "--json", "fullName", "--jq", ".[0].fullName")).stdout.strip() == ctx.repo
    assert expect_ok(ctx.gh("search", "repos", name, "--json", "fullName", "--template", "{{range .}}{{.fullName}}{{end}}")).stdout == ctx.repo
    assert ctx.repo in expect_ok(ctx.gh("search", "repos", name)).stdout
    repository_names(ctx.gh("search", "repos", ctx.name("absent"), "--json", "fullName"), [])
    client_error(ctx.gh("search", "repos", name, "--include-forks", "invalid", ok=None), "include-forks")
    client_error(ctx.gh("search", "repos", name, "--sort", "invalid", ok=None), "sort")
    expect_error(ctx.gh("search", "repos", name, "--stars", "not-a-number", ok=None), status=422)
    private = ctx.sweep.create_repo(ctx.account, ctx.name("private"), private=True)
    repository_names(ctx.gh("search", "repos", private["name"], "--visibility", "private", "--json", "fullName"), [private["full_name"]])
    repository_names(ctx.sweep.gh(other_account(ctx), "search", "repos", private["name"], "--owner", ctx.account.name, "--json", "fullName"), [])


@case(command="search repos", variant="sort/order/limit with distinguishable stars; every sort key", timeout=180)
def search_repos_sort(ctx):
    prefix = ctx.name("sort")
    first = ctx.sweep.create_repo(ctx.account, prefix + "-a")["full_name"]
    second = ctx.sweep.create_repo(ctx.account, prefix + "-b")["full_name"]
    ctx.sweep.api(other_account(ctx), "PUT", "user/starred/" + first)
    for order, expected in (("desc", [first, second]), ("asc", [second, first])):
        rows = expect_json(ctx.gh("search", "repos", prefix, "--owner", ctx.account.name, "--sort", "stars", "--order", order, "--json", "fullName"), [{"fullName": str}])
        assert [row["fullName"] for row in rows] == expected, rows
    repository_names(ctx.gh("search", "repos", prefix, "--owner", ctx.account.name, "--sort", "stars", "--order", "desc", "--limit", "1", "--json", "fullName"), [first])
    for sort in ("forks", "help-wanted-issues", "updated"):
        for order in ("asc", "desc"):
            repository_names(ctx.gh("search", "repos", prefix, "--owner", ctx.account.name, "--sort", sort, "--order", order, "--json", "fullName"), [first, second])


@case(command="status", variant="assigned issue and explicit exclusion", needs=("issue",), timeout=180)
def status(ctx):
    title = ctx.issue["title"]
    ctx.api("PATCH", f"repos/{ctx.repo}/issues/{ctx.issue['number']}", {"assignees": [ctx.account.name]})
    result = expect_ok(ctx.gh("status"))
    text = result.stdout + result.stderr
    assert "Assigned Issues" in text and title in text, text
    result = expect_ok(ctx.gh("status", "--exclude", ctx.repo))
    text = result.stdout + result.stderr
    assert "Assigned Issues" in text and title not in text, text


@case(command="status", variant="organization scoping and exclusions compose", timeout=180)
def status_org(ctx):
    org = ctx.org()
    repo = ctx.sweep.create_repo(ctx.account, ctx.name("org-repo"), org=org)["full_name"]
    title = ctx.name("org-issue")
    ctx.api("POST", f"repos/{repo}/issues", {"title": title, "assignees": [ctx.account.name]})
    outside = ctx.issue
    ctx.api("PATCH", f"repos/{ctx.repo}/issues/{outside['number']}", {"assignees": [ctx.account.name]})
    result = expect_ok(ctx.gh("status", "--org", org))
    text = result.stdout + result.stderr
    assert title in text and outside["title"] not in text, text
    result = expect_ok(ctx.gh("status", "--org", org, "--exclude", repo, "--exclude", ctx.repo))
    text = result.stdout + result.stderr
    assert "Assigned Issues" in text and title not in text and outside["title"] not in text, text


@case(command="status", variant="private assigned activity does not leak to second identity", timeout=180)
def status_private(ctx):
    row = ctx.sweep.create_repo(ctx.account, ctx.name("private"), private=True)
    title = ctx.name("private-assignment")
    ctx.api("POST", "repos/" + row["full_name"] + "/issues", {"title": title, "assignees": [ctx.account.name]})
    output = expect_ok(ctx.sweep.gh(other_account(ctx), "status")).stdout
    assert "Assigned Issues" in output and title not in output, output


@case(command="api", variant="nested object and array payload fields", needs=("repo",))
def api_nested_fields(ctx):
    filename = ctx.name("gist-file") + ".txt"
    description = ctx.name("gist")
    value = expect_json(ctx.gh("api", "gists", "--field", "public=true",
                               "--raw-field", "description=" + description,
                               "--raw-field", "files[" + filename + "][content]=nested payload"),
                        {"id": str, "public": True, "description": description, "files": dict})
    assert value["files"][filename]["content"] == "nested payload", value
    expect_ok(ctx.gh("api", "gists/" + value["id"], "--method", "DELETE", "--silent"))
    label = ctx.name("array-label")
    ctx.api("POST", f"repos/{ctx.repo}/labels", {"name": label, "color": "123456"})
    issue = expect_json(ctx.gh("api", f"repos/{ctx.repo}/issues", "--raw-field", "title=" + ctx.name("array-issue"),
                               "--field", "labels[]=" + label), {"number": int, "labels": [{"name": str}]})
    assert [row["name"] for row in issue["labels"]] == [label], issue


def project_fixture(ctx, row, pull=False):
    base = f"/users/{ctx.account.name}/projects"
    location = ctx.native("form", base, "--action", base, "--has", "title",
                          "--set", "title=" + ctx.name("project"), "--status", "303", "--print", "location")
    number = int(location.rstrip("/").rsplit("/", 1)[1])
    path = f"users/{ctx.account.name}/projectsV2/{number}"
    owner, repo = ctx.repo.split("/")
    ctx.api("POST", path + "/items", {"type": "PullRequest" if pull else "Issue",
                                      "owner": owner, "repo": repo, "number": row["number"]})
    return ctx.account.name + "/" + str(number)


@case(command="search issues", variant="project presence and positive language filter", needs=("issue",), timeout=180)
def search_issues_project(ctx):
    project = project_fixture(ctx, ctx.issue)
    numbers(ctx.gh("search", "issues", "--repo", ctx.repo, "--project", project, "--json", "number"), [ctx.issue["number"]])
    numbers(ctx.gh("search", "issues", "--repo", ctx.repo, "--no-project", "--json", "number"), [])
    code_fixture(ctx)
    numbers(ctx.gh("search", "issues", "--repo", ctx.repo, "--language", "python", "--json", "number"), [ctx.issue["number"]])


@case(command="search prs", variant="project presence and positive language filter", needs=("pr",), timeout=180)
def search_prs_project(ctx):
    project = project_fixture(ctx, ctx.pr, pull=True)
    numbers(ctx.gh("search", "prs", "--repo", ctx.repo, "--project", project, "--json", "number"), [ctx.pr["number"]])
    numbers(ctx.gh("search", "prs", "--repo", ctx.repo, "--no-project", "--json", "number"), [])
    code_fixture(ctx)
    numbers(ctx.gh("search", "prs", "--repo", ctx.repo, "--language", "python", "--json", "number"), [ctx.pr["number"]])
