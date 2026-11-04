#!/usr/bin/env python3
"""Move the independent database types, model crate, and database core in order."""
import re
import sys
from pathlib import Path

ROOTS = ("src", "tests", "backend", "webui", "crates", "scripts", "docs")
CORE_ITEMS = ("DbPool", "init_db", "sqlite_file", "run_migrations", "models", "connection")


def source_module(path):
    parts = list(path.with_suffix("").parts)
    if parts[0] == "src":
        parts[0] = "crate"
        if parts[-1] in ("mod", "lib", "main"):
            parts.pop()
        return parts
    return []


def leaves(tree, prefix=""):
    depth = 0
    start = 0
    pieces = []
    for i, char in enumerate(tree):
        depth += (char == "{") - (char == "}")
        if char == "," and depth == 0:
            pieces.append(tree[start:i])
            start = i + 1
    pieces.append(tree[start:])
    result = []
    for piece in pieces:
        piece = piece.strip()
        if not piece:
            continue
        if "{" in piece:
            head, rest = piece.split("{", 1)
            result.extend(leaves(rest.rsplit("}", 1)[0], prefix + head.strip()))
        elif piece == "self" or piece.startswith("self as "):
            result.append(prefix.removesuffix("::") + piece[4:])
        else:
            result.append(prefix + piece)
    return result


def shorten_imported_paths(text):
    imports = list(re.finditer(r"(?m)^use\s+([^;]+);", text))
    names = {}
    for match in imports:
        for leaf in leaves(match[1]):
            path, *alias = re.split(r"\s+as\s+", leaf)
            if path.startswith(("codeyard_db_core::", "codeyard_model::", "codeyard_support::")):
                name = alias[0] if alias else path.split("::")[-1]
                if name not in ("*", "_"):
                    names[path] = name
    for match in re.finditer(r"(?m)^[ \t]*(?:pub(?:\([^\n]*?\))?\s+)?use\s+([^;]+);", text):
        for leaf in leaves(match[1]):
            path, *alias = re.split(r"\s+as\s+", leaf)
            name = alias[0] if alias else path.split("::")[-1]
            names = {key: value for key, value in names.items() if value != name or key == path}
    names = {path: name for path, name in names.items() if not re.search(r"\b(?:struct|enum|type|trait)\s+" + re.escape(name) + r"\b", text)}
    if not names:
        return text
    pattern = re.compile(r"\b(" + "|".join(re.escape(path) for path in sorted(names, key=len, reverse=True)) + r")\b")
    def shorten(part):
        return pattern.sub(lambda match: names[match[0]], part)
    parts = []
    at = 0
    for match in re.finditer(r"(?m)^[ \t]*(?:pub(?:\([^\n]*?\))?\s+)?use\s+[^;]+;", text):
        parts.extend((shorten(text[at:match.start()]), match[0]))
        at = match.end()
    parts.append(shorten(text[at:]))
    return "".join(parts)


def rewrite(mappings):
    for root in ROOTS:
        for path in Path(root).rglob("*"):
            if not path.is_file() or path.suffix not in (".rs", ".md", ".py", ".sh"):
                continue
            if path.resolve() == Path(__file__).resolve():
                continue
            text = path.read_text()
            original = text
            module = source_module(path)
            aliases = {"db": "crate::backend::db"}

            def canonical(name):
                parts = name.split("::")
                if parts[0] == "codeyard":
                    parts[0] = "crate"
                elif parts[0] == "super" and module:
                    base = module[:]
                    while parts and parts[0] == "super":
                        base.pop()
                        parts.pop(0)
                    parts = base + parts
                elif parts[0] == "self" and module:
                    parts = module + parts[1:]
                elif parts[0] in aliases:
                    parts = aliases[parts[0]].split("::") + parts[1:]
                return "::".join(parts)

            imports = list(re.finditer(r"(?m)^([ \t]*)(pub(?:\([^\n]*?\))?\s+)?use\s+([^;]+);", text))
            for match in imports:
                for leaf in leaves(match[3]):
                    name, *alias = re.split(r"\s+as\s+", leaf)
                    resolved = canonical(name)
                    key = alias[0] if alias else name.split("::")[-1]
                    if key != "db":
                        aliases[key] = resolved

            def mapped(name):
                resolved = canonical(name)
                for old, new in mappings:
                    if resolved == old or resolved.startswith(old + "::"):
                        result = new + resolved[len(old):]
                        if name.startswith("codeyard::") and result.startswith("crate::"):
                            result = "codeyard::" + result[len("crate::"):]
                        return result
                return name

            def import_replacement(match):
                names = leaves(match[3])
                changed = []
                for name in names:
                    item, *alias = re.split(r"\s+as\s+", name)
                    changed.append(mapped(item) + (" as " + alias[0] if alias else ""))
                if changed == names:
                    return match[0]
                return "\n".join(match[1] + (match[2] or "") + "use " + name + ";" for name in changed)

            text = re.sub(r"(?m)^([ \t]*)(pub(?:\([^\n]*?\))?\s+)?use\s+([^;]+);", import_replacement, text)
            # Qualified references, including references through an imported module alias.
            text = re.sub(r"\b[a-zA-Z_]\w*(?:::[a-zA-Z_]\w*)+", lambda m: mapped(m[0]), text)
            if path.suffix == ".rs":
                text = shorten_imported_paths(text)
            if text != original:
                path.write_text(text)


def remove_unused_database_imports():
    uses = re.compile(r"(?m)^([ \t]*)use\s+([^;]+);")
    for root in ("src", "tests"):
        for path in Path(root).rglob("*.rs"):
            text = path.read_text()
            directory = path.parent if path.name == "mod.rs" else path.with_suffix("")
            children = "".join(child.read_text() for child in directory.rglob("*.rs") if child != path) if directory.is_dir() else ""
            body = uses.sub("", text + children)
            def prune(match):
                kept = []
                for leaf in leaves(match[2]):
                    item, *alias = re.split(r"\s+as\s+", leaf)
                    name = alias[0] if alias else item.split("::")[-1]
                    moved = item.startswith(("codeyard_db_core::", "codeyard_model::")) or name == "db"
                    if name in ("*", "_") or not moved or re.search(r"(?<![:\w])" + re.escape(name) + r"\b", body):
                        kept.append(leaf)
                if kept == leaves(match[2]):
                    return match[0]
                return "\n".join(match[1] + "use " + item + ";" for item in kept)
            updated = uses.sub(prune, text)
            if updated != text:
                path.write_text(updated)


def extract(path, start, end):
    text = path.read_text()
    a = text.index(start)
    b = text.index(end, a) if end else len(text)
    item = text[a:b]
    path.write_text(text[:a] + text[b:])
    return item


def prepend(path, imports):
    text = path.read_text()
    lines = text.splitlines(keepends=True)
    at = 0
    while at < len(lines) and lines[at].startswith("//!"):
        at += 1
    lines.insert(at, imports + "\n")
    path.write_text("".join(lines))


def types():
    destination = Path("backend/app/src/model")
    destination.mkdir()
    ci = extract(Path("backend/app/src/ci/queue.rs"), "#[derive(Debug, Clone, Default, Serialize, Deserialize)]\npub struct LogLine", "/// Replaces each control byte")
    ci += extract(Path("backend/app/src/ci/tokens.rs"), "/// Reusable calls multiply job identities", "pub(crate) async fn issue_selected")
    ci += extract(Path("backend/app/src/ci/logs.rs"), "/// Lines one run may store", "const TRUNCATED_LINE")
    (destination / "ci.rs").write_text("use serde::{Deserialize, Serialize};\n\n" + ci.replace("pub(crate) const MAX_EXPANDED_JOBS", "pub const MAX_EXPANDED_JOBS"))
    identity = extract(Path("backend/identity/src/oauth_apps.rs"), "pub const CLI_CLIENT", "const CLI_SECRET")
    identity += extract(Path("backend/db/core/src/queries/users/ssh_keys.rs"), "#[derive(Debug, thiserror::Error, PartialEq, Eq)]", "#[derive(Debug, sqlx::FromRow)]")
    (destination / "identity.rs").write_text(identity)
    repositories = extract(Path("backend/app/src/repositories/dependency_graph.rs"), "#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]", "fn unavailable")
    repositories += extract(Path("backend/app/src/repositories/deploy_keys.rs"), "#[derive(Debug, thiserror::Error)]", "/// Keeps a typed key refusal")
    repositories += extract(Path("backend/app/src/repositories/code_owners.rs"), "pub(crate) const MAX_REVIEWERS", "/// One line")
    (destination / "repositories.rs").write_text("use super::identity::SshKeyWriteRefusal;\nuse serde::{Deserialize, Serialize};\n\n" + repositories.replace("pub(crate) const MAX_REVIEWERS", "pub const MAX_REVIEWERS"))
    path = Path("backend/app/src/repositories/dependency_graph.rs")
    path.write_text(path.read_text().replace("use serde::{Deserialize, Serialize};\n", ""))
    ecosystems = extract(Path("backend/app/src/vulnerabilities/mod.rs"), "pub const ECOSYSTEMS", "pub fn ecosystem")
    ecosystems += extract(Path("backend/app/src/vulnerabilities/mod.rs"), "pub(crate) const MAX_AFFECTED_PACKAGES", "/// The validated catalogue")
    (destination / "vulnerabilities.rs").write_text(ecosystems.replace("pub(crate) const", "pub const"))
    (destination / "mod.rs").write_text("pub mod ci;\npub mod identity;\npub mod repositories;\npub mod vulnerabilities;\n")
    path = Path("backend/app/src/mod.rs")
    text = path.read_text()
    path.write_text(text.replace("pub mod db;", "pub mod db;\npub mod model;"))
    names = {
        "ci::queue::LogLine": "ci::LogLine",
        "ci::tokens::MAX_EXPANDED_JOBS": "ci::MAX_EXPANDED_JOBS",
        "ci::logs::MAX_RUN_LOG_LINES": "ci::MAX_RUN_LOG_LINES",
        "ci::logs::MAX_RUN_LOG_BYTES": "ci::MAX_RUN_LOG_BYTES",
        "identity::oauth_apps::CLI_CLIENT": "identity::CLI_CLIENT",
        "db::users::ssh_keys::SshKeyWriteRefusal": "identity::SshKeyWriteRefusal",
        "repositories::dependency_graph::Package": "repositories::Package",
        "repositories::deploy_keys::DeployKeyError": "repositories::DeployKeyError",
        "repositories::code_owners::MAX_REVIEWERS": "repositories::MAX_REVIEWERS",
        "vulnerabilities::ECOSYSTEMS": "vulnerabilities::ECOSYSTEMS",
        "vulnerabilities::MAX_AFFECTED_PACKAGES": "vulnerabilities::MAX_AFFECTED_PACKAGES",
    }
    rewrite([("crate::backend::" + old, "crate::backend::model::" + new) for old, new in names.items()])
    path = Path("backend/app/src/ci/worker.rs")
    path.write_text(path.read_text().replace("super::super::logs::MAX_RUN_LOG_LINES", "crate::backend::model::ci::MAX_RUN_LOG_LINES"))
    for path, imports in {
        "ci/queue.rs": "use crate::backend::model::ci::LogLine;",
        "ci/tokens.rs": "use crate::backend::model::ci::MAX_EXPANDED_JOBS;",
        "ci/logs.rs": "use crate::backend::model::ci::{MAX_RUN_LOG_LINES, MAX_RUN_LOG_BYTES};",
        "identity/oauth_apps.rs": "use crate::backend::model::identity::CLI_CLIENT;",
        "db/users/ssh_keys.rs": "use crate::backend::model::identity::SshKeyWriteRefusal;",
        "repositories/dependency_graph.rs": "use crate::backend::model::repositories::Package;",
        "repositories/deploy_keys.rs": "use crate::backend::model::repositories::DeployKeyError;",
        "repositories/code_owners.rs": "use crate::backend::model::repositories::MAX_REVIEWERS;",
        "vulnerabilities/mod.rs": "use crate::backend::model::vulnerabilities::{ECOSYSTEMS, MAX_AFFECTED_PACKAGES};",
    }.items():
        prepend(Path("src/backend") / path, imports)


def crypto():
    helpers = extract(Path("backend/support/src/secrets/mod.rs"), "/// 32 random bytes", "pub struct SecretEncryptor")
    Path("backend/support/src/crypto.rs").write_text("//! Random tokens and keyed hashes shared by native server code.\nuse rand::RngCore;\n\n" + helpers)
    path = Path("backend/support/backend/app/wiring/lib.rs")
    path.write_text(path.read_text() + '\n#[cfg(not(target_arch = "wasm32"))]\npub mod crypto;\n')
    rewrite([("crate::backend::crypto::" + name, "codeyard_support::crypto::" + name) for name in ("random_hex", "is_hex_token", "hmac_sha256")])


def model():
    destination = Path("backend/model/src")
    destination.parent.mkdir()
    Path("backend/app/src/model").rename(destination)
    (destination / "mod.rs").rename(destination / "lib.rs")
    path = Path("backend/app/src/mod.rs")
    path.write_text(path.read_text().replace("pub mod model;\n", ""))
    rewrite([("crate::backend::model", "codeyard_model")])


def core():
    destination = Path("backend/db/core/src")
    destination.mkdir(parents=True)
    old = Path("backend/app/src/db")
    for name in ("connection.rs", "migrations.rs", "models"):
        (old / name).rename(destination / name)
    for engine in ("postgres", "sqlite"):
        target = destination / engine
        target.mkdir()
        (old / engine / "migrations.rs").rename(target / "migrations.rs")
        (target / "mod.rs").write_text("pub mod migrations;\n")
        path = old / engine / "mod.rs"
        path.write_text(path.read_text().replace("pub(super) mod migrations;\n", ""))
    path = old / "mod.rs"
    text = path.read_text()
    start = text.index("macro_rules! native_operation")
    end = text.index("mod postgres;", start)
    macro = text[start:end].replace("macro_rules!", "#[macro_export]\nmacro_rules!")
    macro = macro.replace("crate::backend::db::DbPool", "$crate::DbPool")
    text = text[:start] + text[end:]
    for line in ("mod connection;\n", "mod migrations;\n", "pub mod models;\n", "pub use connection::{init_db, sqlite_file, DbPool};\n", "pub use migrations::run_migrations;\n"):
        text = text.replace(line, "")
    path.write_text(text)
    (destination / "lib.rs").write_text("//! Shared database connections, migrations and record types.\n\n" + macro + "pub mod connection;\nmod migrations;\npub mod models;\npub mod postgres;\npub mod sqlite;\n\npub use connection::{init_db, sqlite_file, DbPool};\npub use migrations::run_migrations;\n")
    mappings = [("crate::backend::db::" + item, "codeyard_db_core::" + item) for item in CORE_ITEMS]
    mappings += [("crate::backend::db::" + engine + "::migrations", "codeyard_db_core::" + engine + "::migrations") for engine in ("postgres", "sqlite")]
    mappings += [("crate::backend::db::" + engine + "::models", "codeyard_db_core::models") for engine in ("postgres", "sqlite")]
    rewrite(mappings)
    for engine in ("postgres", "sqlite"):
        path = destination / engine / "migrations.rs"
        text = path.read_text().replace("pub(in crate::backend::db) fn validate_history", "pub fn validate_history")
        text = text.replace("../../../../schema/", "../../../../../schema/")
        path.write_text(text)
    for engine in ("postgres", "sqlite"):
        path = old / engine / "mod.rs"
        path.write_text(path.read_text().replace("pub(crate) use codeyard_db_core::models;\n", ""))
    path = destination / "connection.rs"
    path.write_text(path.read_text().replace("codeyard_db_core::run_migrations", "crate::run_migrations"))
    path = destination / "models/ci.rs"
    path.write_text(path.read_text().replace("pub(crate) fn workflow_reference", "pub fn workflow_reference"))
    for path in old.rglob("*.rs"):
        if re.search(r"(?m)^\s*native_operation!\(", path.read_text()):
            prepend(path, "use codeyard_db_core::native_operation;")
    remove_unused_database_imports()


if __name__ == "__main__":
    {"types": types, "crypto": crypto, "model": model, "core": core}[sys.argv[1]]()
