#!/usr/bin/env python3
"""HTTP and HTML helper for scripts/e2e.sh. Standard library only.

One invocation is one request (or one form submission) against the running
server. Cookies persist between invocations in a JSON jar, CSRF tokens are taken
from the real forms, and the response is checked against the expectations on the
command line. stdout carries only what --print asks for; the trace and every
failure go to stderr. Exit status: 0 ok, 1 an expectation failed, 2 bad usage.

  e2e-web.py req METHOD PATH [options]       one request
  e2e-web.py form PATH --action A [options]  GET a page, fill its form, POST it
                                             (--file NAME=PATH sends a file input)
  e2e-web.py form-get PATH --action A --save FILE
  e2e-web.py form-post --from FILE [options] POST a form saved by form-get
  e2e-web.py page PATH [options]             server HTML plus hydration contract
  e2e-web.py bundle --dir DIR                the browser bundle is served intact
"""
import argparse
import hashlib
import html
import http.client
import json
import mimetypes
import os
import posixpath
import re
import sys
import time
import urllib.parse
from html.parser import HTMLParser
from pathlib import Path

HEX64 = re.compile(r"^[0-9a-f]{64}$")
MODEL_TYPE = "application/vnd.codeyard.page+json"
ACCEPT = {"html": "text/html", "json": "application/json", "model": MODEL_TYPE, "any": "*/*"}


class Failure(Exception):
    """An expectation did not hold."""


# ---------------------------------------------------------------- HTML --


def strip_comments(text):
    return re.sub(r"<!--.*?-->", "", text, flags=re.S)


def visible_text(text):
    text = strip_comments(text)
    text = re.sub(r"<(script|style)\b.*?</\1>", " ", text, flags=re.S | re.I)
    text = re.sub(r"<[^>]+>", " ", text)
    return " ".join(html.unescape(text).split())


class Form:
    def __init__(self, attrs):
        self.attrs = attrs
        self.fields = []  # (name, value) in document order, as a browser would send them
        self.files = []  # names of the file inputs, which a browser sends as file parts
        self.buttons = []  # (name, value, formaction) of the buttons that post somewhere else

    @property
    def action(self):
        return self.attrs.get("action") or ""

    @property
    def method(self):
        return (self.attrs.get("method") or "get").lower()

    def get(self, name):
        for key, value in self.fields:
            if key == name:
                return value
        return None


def truthy(attrs, name):
    """HTML boolean attribute: present means true, except an explicit false."""
    if name not in attrs:
        return False
    value = attrs[name]
    return value is None or value.strip().lower() not in ("false", "0")


class FormParser(HTMLParser):
    """Collects forms and the fields a browser would submit for them."""

    def __init__(self):
        super().__init__(convert_charrefs=True)
        self.forms = []
        self._form = None
        self._select = None
        self._option = None
        self._textarea = None
        self._controls = []

    def close(self):
        super().close()
        by_id = {form.attrs["id"]: form for form in reversed(self.forms) if form.attrs.get("id")}
        for owner, control in self._controls:
            form = by_id.get(owner) if isinstance(owner, str) else owner
            if form is not None:
                form.fields.extend(control.fields)
                form.files.extend(control.files)
                form.buttons.extend(control.buttons)

    def handle_starttag(self, tag, attrs):
        a = dict(attrs)
        if tag == "form":
            self._form = Form(a)
            self.forms.append(self._form)
            return
        form = None
        if tag in ("input", "button", "textarea", "select"):
            # Explicit owners can appear later in the document or outside the enclosing form.
            owner = a.get("form") if "form" in a else self._form
            form = Form({})
            self._controls.append((owner, form))
        if tag == "input":
            name = a.get("name")
            kind = (a.get("type") or "text").lower()
            if not name or truthy(a, "disabled"):
                return
            if kind == "file":
                form.files.append(name)
                return
            if kind in ("submit", "button", "image", "reset"):
                if a.get("formaction"):
                    form.buttons.append((name, a.get("value") or "", a["formaction"]))
                return
            if kind in ("checkbox", "radio"):
                if truthy(a, "checked"):
                    form.fields.append((name, a["value"] if a.get("value") is not None else "on"))
                return
            form.fields.append((name, a.get("value") or ""))
        elif tag == "button":
            if a.get("formaction") and not truthy(a, "disabled"):
                form.buttons.append((a.get("name"), a.get("value") or "", a["formaction"]))
        elif tag == "textarea":
            self._textarea = [a.get("name"), [], truthy(a, "disabled"), form]
        elif tag == "select":
            self._select = {"name": a.get("name"), "options": [], "disabled": truthy(a, "disabled"), "form": form}
        elif tag == "option" and self._select is not None:
            self._option = {"value": a.get("value"), "selected": truthy(a, "selected"), "text": []}
            self._select["options"].append(self._option)

    def handle_endtag(self, tag):
        if tag == "form":
            self._form = None
        elif tag == "textarea" and self._textarea is not None:
            name, chunks, disabled, form = self._textarea
            self._textarea = None
            text = "".join(chunks)
            # A browser drops one newline right after <textarea>.
            if text.startswith("\r\n"):
                text = text[2:]
            elif text.startswith("\n"):
                text = text[1:]
            if name and not disabled:
                form.fields.append((name, text))
        elif tag == "option":
            self._option = None
        elif tag == "select" and self._select is not None:
            select, self._select = self._select, None
            if select["name"] and not select["disabled"] and select["options"]:
                chosen = [o for o in select["options"] if o["selected"]] or select["options"][:1]
                option = chosen[0]
                value = option["value"] if option["value"] is not None else "".join(option["text"]).strip()
                select["form"].fields.append((select["name"], value))

    def handle_data(self, data):
        if self._textarea is not None:
            self._textarea[1].append(data)
        elif self._option is not None:
            self._option["text"].append(data)


def parse_forms(text):
    parser = FormParser()
    parser.feed(text)
    parser.close()
    return parser.forms


def describe_forms(forms):
    if not forms:
        return "no forms on the page"
    return "; ".join(
        f"{f.method.upper()} {f.action or '(self)'} [{', '.join(n for n, _ in f.fields)}]" for f in forms
    )


def pick_form(forms, action, has):
    for form in forms:
        if action is not None:
            targets = [form.action] + [target for _, _, target in form.buttons]
            if not any(t == action or urllib.parse.urlsplit(t).path == action for t in targets):
                continue
        good = True
        for spec in has:
            name, eq, value = spec.partition("=")
            if not any(n == name and (not eq or v == value) for n, v in form.fields):
                good = False
                break
        if good:
            return form
    raise Failure(f"no form with action={action!r} has={has!r}; {describe_forms(forms)}")


def submit_target(form, action, submit):
    """Where a browser posts: the formaction of the clicked button, else the form's action."""
    name, _, value = (submit or "").partition("=")
    for n, v, target in form.buttons if form else []:
        if submit and n == name and v == value:
            return target
    return action


def apply_overrides(fields, sets, unsets, submit):
    out = [(n, v) for n, v in fields if n not in unsets]
    for item in sets:
        name, _, value = item.partition("=")
        seen = False
        replaced = []
        for n, v in out:
            if n == name:
                if not seen:
                    replaced.append((n, value))
                    seen = True
            else:
                replaced.append((n, v))
        if not seen:
            replaced.append((name, value))
        out = replaced
    if submit:
        name, _, value = submit.partition("=")
        out.append((name, value))
    return out


class PageParser(HTMLParser):
    """Title, ids and scripts: enough to check the hydration contract."""

    def __init__(self):
        super().__init__(convert_charrefs=True)
        self.title = []
        self.in_title = False
        self.scripts = []
        self._script = None

    def handle_starttag(self, tag, attrs):
        a = dict(attrs)
        if tag == "title":
            self.in_title = True
        elif tag == "script":
            self._script = {"attrs": a, "data": []}
            self.scripts.append(self._script)

    def handle_endtag(self, tag):
        if tag == "title":
            self.in_title = False
        elif tag == "script":
            self._script = None

    def handle_data(self, data):
        if self.in_title:
            self.title.append(data)
        if self._script is not None:
            self._script["data"].append(data)


# ---------------------------------------------------------------- HTTP --


class Response:
    def __init__(self, method, path, status, headers, body):
        self.method = method
        self.path = path
        self.status = status
        self.headers = headers  # [(lowercase name, value)]
        self.body = body
        self.skip_scripts = False  # --contains looks at the rendered page, not the embedded JSON

    def header(self, name):
        name = name.lower()
        for key, value in self.headers:
            if key == name:
                return value
        return None

    def header_all(self, name):
        name = name.lower()
        return [value for key, value in self.headers if key == name]

    @property
    def text(self):
        return self.body.decode("utf-8", "replace")

    @property
    def is_html(self):
        return "html" in (self.header("content-type") or "")

    def searchable(self):
        """Text for --contains: HTML without hydration comments, entities decoded."""
        if self.is_html:
            text = strip_comments(self.text)
            if self.skip_scripts:
                text = re.sub(r"<script\b.*?</script>", "", text, flags=re.S | re.I)
            return html.unescape(text)
        return self.text

    def flattened(self):
        """What a reader sees: no tags, scripts or styles, one space between words."""
        if not self.is_html:
            return " ".join(self.text.split())
        return visible_text(self.text)

    def json(self):
        try:
            return json.loads(self.text)
        except ValueError as error:
            raise Failure(f"response is not JSON ({error}): {self.excerpt()}") from None

    def excerpt(self, limit=500):
        text = visible_text(self.text) if self.is_html else " ".join(self.text.split())
        return text[:limit]


class Client:
    def __init__(self, base, jar, who):
        if not base:
            raise SystemExit("e2e-web: --base (or E2E_BASE) is required")
        parts = urllib.parse.urlsplit(base)
        if parts.scheme not in ("http", "https") or not parts.hostname:
            raise SystemExit("e2e-web: --base must be an HTTP or HTTPS URL")
        self.host = parts.hostname
        self.port = parts.port or (443 if parts.scheme == "https" else 80)
        self.connection = http.client.HTTPSConnection if parts.scheme == "https" else http.client.HTTPConnection
        self.jar_path = jar
        self.who = who or "anon"
        self.cookies = {}
        if jar and os.path.exists(jar):
            with open(jar) as handle:
                self.cookies = json.load(handle)

    def _save(self):
        if self.jar_path:
            temporary = self.jar_path + ".tmp"
            with open(temporary, "w") as handle:
                json.dump(self.cookies, handle)
            os.replace(temporary, self.jar_path)

    def _absorb(self, response):
        changed = False
        for value in response.header_all("set-cookie"):
            first, *attributes = [part.strip() for part in value.split(";")]
            name, _, cookie = first.partition("=")
            attributes = [attribute.lower() for attribute in attributes]
            if cookie == "" or "max-age=0" in attributes:
                changed |= self.cookies.pop(name, None) is not None
            else:
                changed |= self.cookies.get(name) != cookie
                self.cookies[name] = cookie
        if changed:
            self._save()

    def request(self, method, path, headers=None, body=None, timeout=60):
        send = {"Accept": ACCEPT["html"], "User-Agent": "codeyard-e2e/1"}
        send.update(headers or {})
        cookie = "; ".join(f"{k}={v}" for k, v in self.cookies.items() if not k.startswith("_"))
        if cookie:
            send["Cookie"] = cookie
        started = time.time()
        connection = self.connection(self.host, self.port, timeout=timeout)
        try:
            connection.request(method, path, body=body, headers=send)
            raw = connection.getresponse()
            data = raw.read()
            response = Response(method, path, raw.status, [(k.lower(), v) for k, v in raw.headers.items()], data)
        except OSError as error:
            raise Failure(f"{method} {path}: connection failed: {error}") from None
        finally:
            connection.close()
        self._absorb(response)
        shown = path if len(path) <= 110 else path[:107] + "..."
        print(
            f"  {self.who:<6} {method:<6} {shown} -> {response.status} ({int((time.time() - started) * 1000)} ms)",
            file=sys.stderr,
        )
        return response

    def csrf(self):
        """Token for the current session, read from the sign-out form."""
        session = self.cookies.get("session_id", "")
        cached = self.cookies.get("_csrf", "")
        if session and cached.startswith(session + ":"):
            return cached.split(":", 1)[1]
        response = self.request("GET", "/logout")
        if response.status != 200:
            raise Failure(f"GET /logout for a CSRF token: status {response.status}")
        token = None
        for form in parse_forms(response.text):
            if urllib.parse.urlsplit(form.action).path == "/logout":
                token = form.get("csrf_token")
        if not token or not HEX64.match(token):
            raise Failure("the sign-out form has no 64-hex csrf_token")
        self.cookies["_csrf"] = f"{self.cookies.get('session_id', '')}:{token}"
        self._save()
        return token


# --------------------------------------------------------- expectations --


def json_get(value, path):
    """Dotted path with list indexes; `*` fans out over a list."""
    parts = [p for p in path.split(".") if p != ""]

    def walk(current, rest):
        if not rest:
            return current
        head, tail = rest[0], rest[1:]
        if head == "*":
            if not isinstance(current, list):
                raise Failure(f"json path {path!r}: '*' needs a list")
            return [walk(item, tail) for item in current]
        if isinstance(current, list):
            try:
                return walk(current[int(head)], tail)
            except (ValueError, IndexError):
                raise Failure(f"json path {path!r}: no element {head!r}") from None
        if isinstance(current, dict):
            if head not in current:
                raise Failure(f"json path {path!r}: no key {head!r} (keys: {sorted(current)[:12]})")
            return walk(current[head], tail)
        raise Failure(f"json path {path!r}: cannot step into {type(current).__name__} at {head!r}")

    return walk(value, parts)


def json_str(value):
    if isinstance(value, str):
        return value
    if value is True:
        return "true"
    if value is False:
        return "false"
    if value is None:
        return "null"
    return json.dumps(value, separators=(",", ":"), ensure_ascii=False)


def closest(flat, needle):
    """Where the longest leading part of the needle occurs in the visible text."""
    words = needle.split()
    for count in range(len(words), 0, -1):
        probe = " ".join(words[:count])
        at = flat.find(probe)
        if at >= 0:
            return f"\n      closest text: ...{flat[max(0, at - 60):at + len(probe) + 160]}..."
    return ""


def lookup(response, data, path):
    """json_get with the request and response in the failure message."""
    try:
        return json_get(data, path)
    except Failure as error:
        fail(response, str(error))


def fail(response, message):
    raise Failure(
        f"{response.method} {response.path}: {message}\n"
        f"      status {response.status}; location {response.header('location')!r}; "
        f"content-type {response.header('content-type')!r}\n"
        f"      body: {response.excerpt()}"
    )


def verify(response, a):
    codes = {int(code) for code in str(a.status).split(",")}
    if response.status not in codes:
        fail(response, f"expected status {a.status}, got {response.status}")
    if a.location is not None:
        got = response.header("location")
        want = a.location
        good = got is not None and (got.startswith(want[:-1]) if want.endswith("*") else got == want)
        if not good:
            fail(response, f"expected Location {want!r}, got {got!r}")
    for spec in a.header_eq:
        name, _, want = spec.partition(":")
        got = response.header(name.strip())
        if got is None or got.strip().lower() != want.strip().lower():
            fail(response, f"expected header {name.strip()}: {want.strip()!r}, got {got!r}")
    for spec in a.header_has:
        name, _, want = spec.partition(":")
        got = response.header(name.strip())
        if got is None or want.strip().lower() not in got.lower():
            fail(response, f"expected header {name.strip()} to contain {want.strip()!r}, got {got!r}")
    text = response.searchable()
    flat = response.flattened()
    for needle in a.contains:
        # Either in the markup (attributes, values) or in the text a reader sees,
        # where inline markup such as an autolinked "#1" does not split the words.
        if needle not in text and " ".join(needle.split()) not in flat:
            fail(response, f"expected the page to contain {needle!r}{closest(flat, needle)}")
    for needle in a.lacks:
        if needle in text or " ".join(needle.split()) in flat:
            fail(response, f"expected the page NOT to contain {needle!r}")
    for needle in a.contains_raw:
        if needle not in response.text:
            fail(response, f"expected the raw body to contain {needle!r}")
    for needle in a.lacks_raw:
        if needle in response.text:
            fail(response, f"expected the raw body NOT to contain {needle!r}")
    if a.body_sha256:
        got = hashlib.sha256(response.body).hexdigest()
        if got != a.body_sha256:
            fail(response, f"expected body sha256 {a.body_sha256}, got {got} ({len(response.body)} bytes)")
    if a.json_eq or a.json_has:
        data = response.json()
        for spec in a.json_eq:
            path, _, want = spec.partition("=")
            got = json_str(lookup(response, data, path))
            if got != want:
                fail(response, f"json {path}: expected {want!r}, got {got!r}")
        for spec in a.json_has:
            path, _, want = spec.partition("=")
            found = lookup(response, data, path)
            items = found if isinstance(found, list) else [found]
            if not any(want in json_str(item) for item in items):
                fail(response, f"json {path}: expected something containing {want!r}, got {json_str(found)[:300]!r}")
    if a.field_eq:
        forms = parse_forms(response.text)
        for spec in a.field_eq:
            name, _, want = spec.partition("=")
            values = [v for f in forms for n, v in f.fields if n == name]
            if want not in values:
                shown = [v[:80] for v in values]
                fail(response, f"expected a form field {name}={want[:80]!r}; fields named {name}: {shown}")
    if a.save:
        Path(a.save).write_bytes(response.body)


def evaluate(client, response, expr, form=None, extra=None):
    kind, _, arg = expr.partition(":")
    if extra and expr in extra:
        return extra[expr]
    if kind == "status":
        return str(response.status)
    if kind == "location":
        return response.header("location") or ""
    if kind == "body":
        return response.text
    if kind == "length":
        return str(len(response.body))
    if kind == "sha256":
        return hashlib.sha256(response.body).hexdigest()
    if kind == "header":
        return response.header(arg) or ""
    if kind == "cookie":
        return client.cookies.get(arg, "")
    if kind == "json":
        return json_str(lookup(response, response.json(), arg))
    if kind == "re":
        match = re.search(arg, response.text)
        if not match:
            fail(response, f"--print re:{arg!r} did not match")
        return match.group(1) if match.groups() else match.group(0)
    if kind == "field":
        forms = [form] if form is not None else parse_forms(response.text)
        for candidate in forms:
            value = candidate.get(arg)
            if value is not None:
                return value
        fail(response, f"--print field:{arg} not found")
    raise SystemExit(f"e2e-web: unknown --print expression {expr!r}")


def emit(client, response, a, form=None, extra=None):
    lines = [evaluate(client, response, expr, form, extra) for expr in a.prints]
    for line in lines:
        print(line)


# ------------------------------------------------------------- commands --


def header_args(values):
    headers = {}
    for item in values:
        name, _, value = item.partition(":")
        headers[name.strip()] = value.strip()
    return headers


def accept_value(name):
    return ACCEPT.get(name, name)


def cmd_req(client, a):
    headers = header_args(a.header)
    if a.accept:
        headers["Accept"] = accept_value(a.accept)
    body = None
    if a.json is not None:
        body = a.json.encode()
        headers["Content-Type"] = "application/json"
    elif a.body_file:
        body = Path(a.body_file).read_bytes()
        headers["Content-Type"] = a.content_type or "application/octet-stream"
    # Browser-session API writes use the same token as native forms. File
    # capability checks deliberately keep their explicit --csrf cases.
    if a.csrf or (a.method.upper() not in ("GET", "HEAD", "OPTIONS")
                  and a.path.startswith("/api/")
                  and not (a.path == "/api/files" or a.path.startswith("/api/files/"))
                  and client.cookies.get("session_id")
                  and "X-CSRF-Token" not in headers):
        headers["X-CSRF-Token"] = client.csrf()
    response = client.request(a.method.upper(), a.path, headers, body)
    response.skip_scripts = a.no_scripts
    verify(response, a)
    emit(client, response, a)


def resolve_action(page_path, action):
    target = urllib.parse.urljoin(page_path, action or page_path)
    parts = urllib.parse.urlsplit(target)
    if parts.netloc:
        raise Failure(f"form action {action!r} leaves the origin")
    return parts.path + (f"?{parts.query}" if parts.query else "")


def get_form(client, a, expected_status):
    page = client.request("GET", a.path, {"Accept": ACCEPT["html"]})
    if page.status != int(expected_status):
        fail(page, f"expected status {expected_status} for the form page, got {page.status}")
    forms = parse_forms(page.text)
    form = pick_form(forms, a.action, a.has)
    token = form.get("csrf_token")
    if token is not None and not HEX64.match(token):
        raise Failure(f"form {form.action!r}: csrf_token is not 64 hex characters")
    return page, form


def multipart_body(fields, files):
    """The form as a browser sends it with enctype multipart/form-data."""
    boundary = "----codeyard-e2e-" + os.urandom(8).hex()
    chunks = []
    for name, value in fields:
        chunks.append(
            f'--{boundary}\r\nContent-Disposition: form-data; name="{name}"\r\n\r\n{value}\r\n'.encode()
        )
    for name, path in files:
        kind = mimetypes.guess_type(path)[0] or "application/octet-stream"
        head = (
            f'--{boundary}\r\nContent-Disposition: form-data; name="{name}"; '
            f'filename="{os.path.basename(path)}"\r\nContent-Type: {kind}\r\n\r\n'
        )
        chunks.append(head.encode() + Path(path).read_bytes() + b"\r\n")
    chunks.append(f"--{boundary}--\r\n".encode())
    return f"multipart/form-data; boundary={boundary}", b"".join(chunks)


def post_form(client, a, page_path, action, fields, form, files=()):
    headers = {"Content-Type": "application/x-www-form-urlencoded"}
    if a.origin:
        headers["Origin"] = a.origin
    if files:
        headers["Content-Type"], body = multipart_body(fields, files)
    else:
        body = urllib.parse.urlencode(fields).encode()
    response = client.request("POST", resolve_action(page_path, action), headers, body)
    verify(response, a)
    emit(client, response, a, form)


def cmd_form(client, a):
    page, form = get_form(client, a, a.get_status)
    fields = apply_overrides(form.fields, a.set, a.unset, a.submit)
    files = []
    for spec in a.file:
        name, _, path = spec.partition("=")
        if name not in form.files:
            raise Failure(f"form {form.action!r} has no file input {name!r}; its file inputs: {form.files}")
        if (form.attrs.get("enctype") or "").lower() != "multipart/form-data":
            raise Failure(f"form {form.action!r} sends files but its enctype is {form.attrs.get('enctype')!r}")
        files.append((name, path))
    post_form(client, a, a.path, submit_target(form, form.action, a.submit), fields, form, files)


def cmd_form_get(client, a):
    page, form = get_form(client, a, a.status)
    if a.save:
        Path(a.save).write_text(
            json.dumps({"page": a.path, "action": form.action, "fields": form.fields}, ensure_ascii=False)
        )
    emit(client, page, a, form)


def cmd_form_post(client, a):
    saved = json.loads(Path(a.from_file).read_text())
    fields = apply_overrides([tuple(f) for f in saved["fields"]], a.set, a.unset, a.submit)
    post_form(client, a, saved["page"], saved["action"], fields, None)


def check(condition, message):
    if not condition:
        raise Failure(message)


def cmd_page(client, a):
    response = client.request("GET", a.path, {"Accept": ACCEPT["html"]})
    response.skip_scripts = True
    verify(response, a)
    where = f"GET {a.path}"

    def header(name):
        return response.header(name) or ""

    check(header("content-type").startswith("text/html"), f"{where}: content-type {header('content-type')!r}")
    check(header("x-codeyard-rendering") == "ssr-hydrated", f"{where}: x-codeyard-rendering {header('x-codeyard-rendering')!r}")
    check("no-store" in header("cache-control"), f"{where}: cache-control {header('cache-control')!r}")
    csp = header("content-security-policy")
    check("script-src 'self' 'wasm-unsafe-eval'" in csp and "default-src 'none'" in csp, f"{where}: CSP {csp!r}")
    check(header("x-frame-options").upper() == "DENY", f"{where}: x-frame-options")
    check(header("x-content-type-options") == "nosniff", f"{where}: x-content-type-options")
    raw = response.text
    check(raw.startswith("<!DOCTYPE html>"), f"{where}: no doctype")
    check('id="dioxus-root"' in raw, f"{where}: no #dioxus-root")
    check("data-node-hydration" in raw, f"{where}: server HTML has no hydration markers")
    parser = PageParser()
    parser.feed(raw)
    parser.close()
    title = "".join(parser.title).strip()
    check(title.endswith("Codeyard"), f"{where}: title {title!r}")
    boot = [s for s in parser.scripts if s["attrs"].get("id") == "codeyard-bootstrap"]
    check(len(boot) == 1 and boot[0]["attrs"].get("type") == "application/json", f"{where}: bootstrap JSON script")
    try:
        data = json.loads("".join(boot[0]["data"]))
    except ValueError as error:
        raise Failure(f"{where}: bootstrap JSON does not parse: {error}") from None
    check(data.get("protocol") == 1 and data.get("renderer") == "0.7.10", f"{where}: bootstrap protocol/renderer")
    initial = data["initial"]
    check(initial["location"] == a.path, f"{where}: bootstrap location {initial['location']!r}")
    shown = initial["title"] if initial["title"].endswith("Codeyard") else initial["title"] + " · Codeyard"
    check(shown == title, f"{where}: bootstrap title {initial['title']!r} vs {title!r}")
    check(HEX64.match(initial["chrome"]["csrf_token"] or ""), f"{where}: bootstrap csrf token")
    check(isinstance(data["hydration"]["data"], str) and data["hydration"]["data"], f"{where}: no hydration data")
    lane = initial["page"].get("lane", "")
    if a.lane:
        check(lane == a.lane, f"{where}: lane {lane!r}, wanted {a.lane!r}")
    if a.user:
        check(initial["chrome"]["username"] == a.user, f"{where}: chrome user {initial['chrome']['username']!r}")
        check(f"Account menu for {a.user}" in response.searchable(), f"{where}: no account menu for {a.user}")
    if a.anonymous:
        check(initial["chrome"]["username"] is None, f"{where}: expected an anonymous page")
        check('href="/login"' in raw, f"{where}: no sign-in link")
    modules = [s for s in parser.scripts if s["attrs"].get("type") == "module"]
    check(len(modules) == 1, f"{where}: {len(modules)} module scripts")
    module = modules[0]["attrs"].get("src") or ""
    check(module.startswith("/assets/") and module.endswith(".js"), f"{where}: module script {module!r}")
    inline = [s for s in parser.scripts if "src" not in s["attrs"] and s["attrs"].get("type") != "application/json"]
    check(not inline, f"{where}: inline executable script")
    emit(client, response, a, None, {"module": module, "lane": lane})


def cmd_bundle(client, a):
    root = Path(a.dir)
    index = (root / "index.html").read_text()
    match = re.search(r'<script[^>]*type="module"[^>]*src="([^"]+)"', index)
    check(match is not None, f"{root}/index.html names no module script")
    module = posixpath.normpath("/" + match.group(1).lstrip("/"))
    served = set()
    wasm = 0
    for path in sorted(root.rglob("*")):
        if not path.is_file() or path.suffix not in (".js", ".wasm"):
            continue
        url = "/" + path.relative_to(root).as_posix()
        if not url.startswith(("/assets/", "/wasm/")):
            continue
        response = client.request("GET", url, {"Accept": ACCEPT["any"]})
        want_type = "application/wasm" if path.suffix == ".wasm" else "text/javascript"
        check(response.status == 200, f"GET {url}: status {response.status}")
        check(want_type in (response.header("content-type") or ""), f"GET {url}: content-type {response.header('content-type')!r}")
        check(response.header("x-content-type-options") == "nosniff", f"GET {url}: nosniff")
        check(response.body == path.read_bytes(), f"GET {url}: served bytes differ from {path}")
        served.add(url)
        wasm += path.suffix == ".wasm"
    check(module in served, f"module script {module} was not among the served files")
    check(wasm >= 1, "no .wasm file in the bundle")
    for url in ("/index.html", "/assets/index.html"):
        response = client.request("GET", url, {"Accept": ACCEPT["any"]})
        check(response.status == 404, f"GET {url}: the generated CSR index must not be served (got {response.status})")
    print(f"bundle ok: {len(served)} files, module {module}", file=sys.stderr)


# ------------------------------------------------------------------ CLI --


def common(parser):
    parser.add_argument("--base", default=os.environ.get("E2E_BASE", ""))
    parser.add_argument("--jar", default="")
    parser.add_argument("--who", default="")


def expectations(parser, status):
    parser.add_argument("--status", default=status)
    parser.add_argument("--location")
    parser.add_argument("--header-eq", action="append", default=[])
    parser.add_argument("--header-has", action="append", default=[])
    parser.add_argument("--contains", action="append", default=[])
    parser.add_argument("--lacks", action="append", default=[])
    parser.add_argument("--contains-raw", action="append", default=[])
    parser.add_argument("--lacks-raw", action="append", default=[])
    parser.add_argument("--json-eq", action="append", default=[])
    parser.add_argument("--json-has", action="append", default=[])
    parser.add_argument("--field-eq", action="append", default=[])
    parser.add_argument("--body-sha256")
    parser.add_argument("--save")
    parser.add_argument("--print", dest="prints", action="append", default=[])


def form_selection(parser):
    parser.add_argument("--action", required=True)
    parser.add_argument("--has", action="append", default=[])


def form_fill(parser):
    parser.add_argument("--set", action="append", default=[])
    parser.add_argument("--unset", action="append", default=[])
    parser.add_argument("--submit")
    parser.add_argument("--origin")


def build_parser():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    sub = parser.add_subparsers(dest="command", required=True)

    p = sub.add_parser("req")
    common(p)
    expectations(p, "200")
    p.add_argument("method")
    p.add_argument("path")
    p.add_argument("--accept")
    p.add_argument("--json")
    p.add_argument("--body-file")
    p.add_argument("--content-type")
    p.add_argument("--header", "-H", action="append", default=[])
    p.add_argument("--csrf", action="store_true")
    p.add_argument("--no-scripts", action="store_true", help="--contains/--lacks look at the rendered HTML, not at script elements such as the hydration data")
    p.set_defaults(run=cmd_req)

    p = sub.add_parser("form")
    common(p)
    expectations(p, "303")
    form_selection(p)
    form_fill(p)
    p.add_argument("--file", action="append", default=[], help="NAME=PATH: send PATH in the form's file input NAME")
    p.add_argument("--get-status", default="200", help="status expected for the page holding the form")
    p.add_argument("path")
    p.set_defaults(run=cmd_form)

    p = sub.add_parser("form-get")
    common(p)
    form_selection(p)
    p.add_argument("--status", default="200")
    p.add_argument("--save", help="store the selected form here for form-post")
    p.add_argument("--print", dest="prints", action="append", default=[])
    p.add_argument("path")
    p.set_defaults(run=cmd_form_get)

    p = sub.add_parser("form-post")
    common(p)
    expectations(p, "303")
    form_fill(p)
    p.add_argument("--from", dest="from_file", required=True)
    p.set_defaults(run=cmd_form_post)

    p = sub.add_parser("page")
    common(p)
    expectations(p, "200")
    p.add_argument("path")
    p.add_argument("--lane")
    p.add_argument("--user")
    p.add_argument("--anonymous", action="store_true")
    p.set_defaults(run=cmd_page)

    p = sub.add_parser("bundle")
    common(p)
    p.add_argument("--dir", required=True)
    p.set_defaults(run=cmd_bundle)
    return parser


def main():
    args = build_parser().parse_args()
    client = Client(args.base, args.jar, args.who)
    try:
        args.run(client, args)
    except Failure as failure:
        print(f"FAIL [{client.who}] {failure}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
