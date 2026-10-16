#!/usr/bin/env python3
"""Reject native authority in the actual resolved browser dependency graph.

Feed `cargo tree --locked -p codeyard --target wasm32-unknown-unknown
--no-default-features --features web --edges normal --prefix none --format '{p}'`.
This does not assert a host graph contains no WASM-support crates: Dioxus may
legitimately depend on shared types there without enabling the web renderer.
"""
import re
import sys

FORBIDDEN = {
    'sqlx', 'sqlx-core', 'sqlx-postgres', 'sqlx-macros', 'sqlx-macros-core',
    'redis', 'deadpool-redis', 'git2', 'libgit2-sys', 'russh',
    'aws-sdk-s3', 'aws-config', 'axum', 'tower-http',
}
packages = set()
for line in sys.stdin:
    match = re.match(r'^([A-Za-z0-9_-]+) v[0-9]', line.strip())
    if match:
        packages.add(match[1])
if 'codeyard' not in packages or 'dioxus-web' not in packages:
    raise SystemExit('Expected an actual root-package web dependency graph, not empty/skipped evidence')
violations = sorted(packages & FORBIDDEN)
if violations:
    raise SystemExit('Browser dependency graph imports native authority: ' + ', '.join(violations))
print('Resolved browser dependency isolation OK (compile/browser acceptance still required)')
