#!/usr/bin/env python3
"""Generate the /api/v3 route table and the coverage document from GitHub's REST description.

Writes src/backend/api/github/routes.rs and docs/GITHUB_API.md. With --check, fails when
either file is out of date instead. An operation counts as implemented when its operation ID
appears as a string literal in src/backend/api/github/ (the area handler lists).
"""
import gzip
import json
import re
import subprocess
import sys
from collections import Counter
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
SPEC = ROOT / "spec/github/api.github.com.json.gz"
ROUTES = ROOT / "src/backend/api/github/routes.rs"
DOC = ROOT / "docs/GITHUB_API.md"
METHODS = ("get", "post", "put", "patch", "delete")

# GitHub products and GitHub's own infrastructure. These answer 404 as GHES does for a
# feature it lacks. Keyed by category, or by (category, subcategory).
NOT_SUPPORTED = {
    "codespaces": "Codespaces is a GitHub product",
    "copilot": "Copilot is a GitHub product",
    "copilot-spaces": "Copilot is a GitHub product",
    "agents": "Copilot agents are a GitHub product; later, Codeyard's own AI features",
    "agent-tasks": "Copilot agents are a GitHub product; later, Codeyard's own AI features",
    "agent-automations": "Copilot agents are a GitHub product; later, Codeyard's own AI features",
    "code-quality": "Code quality is a GitHub product",
    "billing": "Codeyard has no billing",
    "classroom": "GitHub Classroom is a GitHub product",
    "campaigns": "Security campaigns are a GitHub product",
    "credentials": "Credential revocation is GitHub's partner programme",
    ("apps", "marketplace"): "Codeyard has no marketplace",
    ("actions", "hosted-runners"): "GitHub-hosted runners are GitHub's machines",
    ("orgs", "network-configurations"): "Network configurations are GitHub's Azure networking",
    ("orgs", "api-insights"): "API insights are GitHub's own analytics",
}

# GitHub-only concepts can share subcategories with supported forge operations.
NOT_SUPPORTED_OPERATIONS = {
    f"actions/{verb}-actions-cache-{limit}-limit-for-enterprise":
        "Codeyard has no enterprise tenancy; use site or organization cache limits"
    for verb in ("get", "set") for limit in ("retention", "storage")
}
NOT_SUPPORTED_OPERATIONS.update({
    operation: "Codeyard has no enterprise tenancy; use organization Dependabot operations"
    for operation in (
        "dependabot/list-alerts-for-enterprise",
        "dependabot/repository-access-for-enterprise",
        "dependabot/update-repository-access-for-enterprise",
        "dependabot/set-repository-access-default-level-for-enterprise",
    )
})

NOT_SUPPORTED_OPERATIONS.update({
    f"oidc/{verb}-oidc-custom-property-{suffix}-for-enterprise":
        "Codeyard has no enterprise accounts"
    for verb, suffix in (("list", "inclusions"), ("create", "inclusion"), ("delete", "inclusion"))
})

NOT_SUPPORTED_OPERATIONS.update({
    operation: "Codeyard has no Git LFS storage and no file size limit, so imports never rewrite large files"
    for operation in ("migrations/get-large-files", "migrations/set-lfs-preference")
})

# Routes real clients use that GitHub's description leaves out, and Codeyard's extensions.
# They are registered beside the generated table and listed in the document.
EXTRA = [
    ("GET", "/repos/{owner}/{repo}/zipball", "Alias", "The default branch's ZIP archive"),
    ("GET", "/repos/{owner}/{repo}/tarball", "Alias", "The default branch's tar archive"),
    ("POST", "/advisories", "Extension", "Instance administrators create or import a local global advisory in GitHub JSON shape; no network feed"),
    ("GET", "/repos/{owner}/{repo}/actions/workflows/{workflow_id}/badge.svg", "Extension", "A workflow badge from its latest run on the selected branch"),
    ("GET", "/migration-archives/{guid}", "Extension", "Authenticated migration archive bytes; the export archive endpoints redirect here"),
    ("GET", "/emojis/{name}", "Extension", "Local SVG images for the emoji catalogue"),
    ("GET", "/notifications/ws", "Extension", "Live notifications; browser session cookie and exactly one same-origin Origin header required. Heartbeats recheck the session and each event rechecks visibility. No token-only access."),
    ("GET", "/repos/{owner}/{repo}/contents", "Alias", "The repository root, as `contents/` with an empty path"),
    ("GET", "/repos/{owner}/{repo}/git/refs", "Alias", "All references; GitHub serves it undocumented"),
    ("GET", "/repos/{owner}/{repo}/git/refs/{ref}", "Alias", "One reference, the same as `git/ref/{ref}`"),
    ("GET", "/repos/{owner}/{repo}/git/matching-refs", "Alias", "All references, as `matching-refs/` with an empty prefix"),
    ("GET", "/repos/{owner}/{repo}/attestations/bundles/{attestation_id}", "Extension", "Stored signed bundle; repository visibility and token permissions apply"),
]

# A last path parameter that may hold slashes becomes a wildcard when nothing extends it.
SLASHED = {"path", "ref", "basehead", "dir"}


def support(op):
    operation = op["operationId"]
    if operation.startswith("code-scanning/"):
        if "variant-analysis" in operation:
            return "Variant analysis depends on GitHub's hosted CodeQL service"
        if "ai-scan-enablement" in operation:
            return "AI Scan depends on GitHub's AI service"
    if operation.startswith("code-security/") and "enterprise" in operation:
        return "Codeyard has no enterprise accounts"
    if op["operationId"].split("/")[0] in {"code-security", "dependabot", "oidc"} and "-enterprise" in op["operationId"]:
        return "no enterprise accounts"
    x = op.get("x-github", {})
    key = (x.get("category"), x.get("subcategory"))
    return NOT_SUPPORTED_OPERATIONS.get(op.get("operationId")) or NOT_SUPPORTED.get(key) or NOT_SUPPORTED.get(key[0])


def operations(spec):
    for path, item in spec["paths"].items():
        for method in METHODS:
            if method in item:
                yield method, path, item[method]


def axum_paths(paths):
    """Spec paths to axum 0.7 paths. Parameter names are made equal at each position, since
    the router refuses two names for one segment; handlers extract parameters by position."""
    names = {}
    extended = {tuple(p.split("/")[:i]) for p in paths for i in range(1, p.count("/") + 1)}
    out = {}
    for path in sorted(paths):
        segments = path.split("/")
        result = []
        for i, segment in enumerate(segments):
            match = re.fullmatch(r"\{(\w+)\}", segment)
            if not match:
                result.append(segment)
                continue
            prefix = tuple(result)
            name = names.setdefault(prefix, match.group(1))
            last = i == len(segments) - 1
            wildcard = last and match.group(1) in SLASHED and tuple(segments) not in extended
            result.append(("*" if wildcard else ":") + name)
        out[path] = "/".join(result)
    return out


def rust_string(value):
    return json.dumps(value, ensure_ascii=False)


def implemented_ids(ids):
    found = set()
    for file in (ROOT / "src/backend/api/github").rglob("*.rs"):
        if file == ROUTES:
            continue
        found.update(m for m in re.findall(r'"([a-z0-9-]+/[a-z0-9-]+)"', file.read_text()) if m in ids)
    return found


def render(spec):
    ops = sorted(operations(spec), key=lambda o: (o[1], METHODS.index(o[0])))
    paths = axum_paths({path for _, path, _ in ops} | {path for _, path, _, _ in EXTRA})
    done = implemented_ids({op["operationId"] for _, _, op in ops})

    rows = []
    for method, path, op in ops:
        reason = support(op)
        support_rs = "Support::NotSupported" if reason else "Support::Supported"
        rows.append(
            "    Operation {\n"
            f"        id: {rust_string(op['operationId'])},\n"
            f"        method: Method::{method.upper()},\n"
            f"        path: {rust_string(paths[path])},\n"
            f"        docs: {rust_string(op.get('externalDocs', {}).get('url', 'https://docs.github.com/rest'))},\n"
            f"        support: {support_rs},\n"
            "    },\n"
        )
    routes = (
        "// @generated by scripts/github-api-routes.py from spec/github/api.github.com.json.gz.\n"
        "// Do not edit; rerun the script.\n"
        "use axum::http::Method;\n\n"
        "pub(super) enum Support {\n    Supported,\n    NotSupported,\n}\n\n"
        "pub(super) struct Operation {\n"
        "    pub id: &'static str,\n    pub method: Method,\n    pub path: &'static str,\n"
        "    pub docs: &'static str,\n    pub support: Support,\n}\n\n"
        f"pub(super) const OPERATIONS: &[Operation] = &[\n{''.join(rows)}];\n"
    )
    routes = subprocess.run(["rustfmt", "--edition", "2021"], input=routes, capture_output=True, text=True, check=True).stdout

    by_category = {}
    for method, path, op in ops:
        by_category.setdefault(op.get("x-github", {}).get("category", "other"), []).append((method, path, op))
    status = Counter()
    sections = []
    for category in sorted(by_category):
        lines = [f"### {category}\n", "| Status | Method | Path | Operation |", "|---|---|---|---|"]
        for method, path, op in by_category[category]:
            reason = support(op)
            if reason:
                state = "Not supported"
            elif op["operationId"] in done:
                state = "Implemented"
            else:
                state = "To do"
            status[state] += 1
            name = op.get("summary", op["operationId"]).replace("|", "\\|")
            url = op.get("externalDocs", {}).get("url")
            label = f"[{name}]({url})" if url else name
            note = f" ({reason})" if reason else ""
            lines.append(f"| {state}{note} | {method.upper()} | `{path}` | {label} |")
        sections.append("\n".join(lines))
    extra = ["| Kind | Method | Path | What |", "|---|---|---|---|"] + [
        f"| {kind} | {method} | `{path}` | {what} |" for method, path, kind, what in EXTRA
    ]
    doc = (
        "# GitHub REST API coverage\n\n"
        "Generated by `scripts/github-api-routes.py` from GitHub's REST description "
        "(`spec/github/`). Codeyard serves it at `/api/v3`, as GitHub Enterprise Server does.\n"
        "The plan is in [GITHUB_API_PLAN.md](GITHUB_API_PLAN.md).\n\n"
        f"- {len(ops)} operations: {status['Implemented']} implemented, {status['To do']} to do, "
        f"{status['Not supported']} not supported.\n"
        "- \"To do\" answers 501. \"Not supported\" answers 404, as GitHub Enterprise Server does for a "
        "feature it lacks.\n\n"
        "## Beyond GitHub's description\n\n" + "\n".join(extra) + "\n\n"
        "## Operations\n\n" + "\n\n".join(sections) + "\n"
    )
    return routes, doc


def main():
    spec = json.load(gzip.open(SPEC))
    routes, doc = render(spec)
    if "--check" in sys.argv:
        stale = [str(p.relative_to(ROOT)) for p, text in ((ROUTES, routes), (DOC, doc)) if not p.exists() or p.read_text() != text]
        if stale:
            sys.exit(f"out of date, rerun scripts/github-api-routes.py: {', '.join(stale)}")
        return
    ROUTES.write_text(routes)
    DOC.write_text(doc)


if __name__ == "__main__":
    main()
