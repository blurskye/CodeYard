# Gists

`/gists` lists public, owned and starred gists. Signed-in users can create gists, edit
files and descriptions, star, fork and comment. These are native forms: JavaScript is
not required. Historical views and raw links select immutable Git revisions.

The twenty GitHub REST gist operations live under `/api/v3/gists` and
`/api/v3/users/{username}/gists`; see [coverage](GITHUB_API.md#gists).

## Visibility and authority

Public gists appear in public lists. **Secret means unlisted, not private**: anyone
with the link can read, clone or fork one. Secret pages and raw responses discourage
indexing. Do not put passwords or other confidential material in either kind.

Only the owner can change content or delete the gist, including over Git. Site
administrators and repository deploy keys do not gain write access. Account blocks
prevent new stars, comments and forks between the affected accounts; public reading
remains possible. Comment authors can edit their comments, and authors or the gist
owner can delete them. Deleting a parent gist does not delete independent forks.

## Git and limits

Clone the URL shown on the gist page, for example:

```sh
git clone https://host/gists/GIST_ID.git
```

HTTP account tokens and enrolled SSH authentication keys use the same live authority
as ordinary Git transport. A gist has one nonempty `main` branch, with 1–100 flat
regular files and at most 10 MiB of content. Executable file modes are preserved.
Nested trees, symlinks, extra branches, tags and deletion of `main` are rejected.
Every introduced commit must satisfy the shape and representable-date rules, not
just the final tree. Reachable commit graphs are bounded to 200,000 commits and
64 MiB of commit metadata, with a 30-second traversal deadline. Object headers are
checked before commit payloads are parsed. Revision history keeps children before
parents even when their timestamps run backwards. Request buffering is also bounded.

The native editor edits at most 1 MiB of text. Binary and larger files remain unchanged
unless explicitly renamed or deleted; use Git for larger replacements. Previews and
REST inline content may be truncated. Raw links provide the complete content.
Native saves use the editor's original revision and retain refused drafts on conflict.

Git publication and SQL metadata are reconciled rather than pretending they share a
transaction. Accepted writes survive disconnected requests. If a response says the
save outcome is uncertain, reload the identified gist before retrying. The owner's
list also reconciles interrupted initial publications, so accepted creations can be
found without knowing their IDs.

Gists are not ordinary repository discovery, policy or CI resources. Issued IDs remain
reserved after deletion to prevent stale clone URLs from resolving to another
resource. Preexisting accounts named `gists` remain reachable at
`/gists/@users/gists`, with their ordinary repository pages under
`/gists/@repos/gists/NAME`; their existing, nonconflicting Git URLs remain valid.

## Browser checks

On the builder, build the server and the release browser bundle, then run:

```sh
cargo build --locked --bin codeyard
bash scripts/build-webui.sh release
xvfb-run -a python3 tests/browser/test-gists-browser.py --headed
```

Unstripped debug WASM exceeds the runtime's 64 MiB asset limit; do not raise that
limit for a browser test. This needs Chromium and Python Playwright. It starts an owned disposable SQLite
installation, checks JavaScript-disabled forms and hydrated navigation separately,
and writes 375px and 1280px screenshots to `dist/gist-browser`. It never uses an
existing installation. Inspect those screenshots before visual sign-off.
