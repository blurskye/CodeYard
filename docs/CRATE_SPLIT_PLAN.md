# Crate split plan

Goal: edit one area and rebuild only that area. Before the split, `codeyard` was one
crate of about 401,000 lines (server 340k, views 58k), so any edit recompiled all of it.
The views now live in `webui/` and their pure shared helpers in `backend/support/`.
Behaviour does not change; this remains one application and one binary.

## Layout

Crates live under `backend/`, named for the part of the server they hold, so the folder
tells you where a thing belongs. Crate names are `codeyard-<part>`.

```
Cargo.toml                     the workspace
webui/                         Dioxus pages and components, also built for the browser
backend/
  support/                     fs, locks, crypto helpers, small shared code
  git/                         running Git
  model/                       types shared by several parts
  db/
    core/                      DbPool, connection, migrations, models, native_operation!
    identity/ orgs/ repos/ actions/ security/ cluster/     queries by domain
  repositories/                repository store, pull requests, Git sessions
  ci/                          queue, workers, tokens
    runner/                    the Actions runner (today crates/codeyard-actions)
  cluster/                     cluster and node
  identity/                    login, tokens, keys
  notifications/               notifications, email, webhooks
  api/
    http/                      shared HTTP plumbing (config, context, CSRF, errors, sessions)
    github/                    one crate: the router, the generated route table and one
                               folder per GitHub category (actions/, issues/, pulls/, repos/, ...)
    github-graphql/            generated GraphQL types (today crates/codeyard-github-graphql)
    graphql/                   our GraphQL resolvers
    repos/                     web and Git HTTP for repositories
    (rest)                     the remaining routes
  app/                         the `codeyard` binary: wiring, ssh, cli, backups, storage
tests/                         unchanged; they use the crates' public items
```

## What the code looks like now (measured on `fix/merged-1029`)

| Part | Lines | Notes |
|---|---|---|
| `api` | 122k | `api/github` 51k, `api/repos` 28k, `api/graphql` 10.6k |
| `db` | 111k | PostgreSQL 41.7k, SQLite 40.2k, the rest shared |
| `webui` | 58k | no references into the backend |
| `repositories` | 47k | |
| `git` 12k, `ci` 12k, `cluster` 9k, `identity` 6.5k, `webhooks` 5.9k, `ssh` 3k | | |
| `codeyard-github-graphql` | 142k | generated; depends only on `async-graphql` |

Findings that decide the order:

- **`webui`** has no `crate::backend` reference, so it comes out first and cleanly.
- **`api/github`** (about 51k lines) follows GitHub's REST categories at file level: files
  are named for the resource, and the generated table lists every operation under GitHub's
  own operation id (`pulls/merge`, `security-advisories/get-global-advisory`). It stays one
  crate. Its folders are renamed to GitHub's categories, so a handler for `pulls/merge`
  lives in `pulls/`, the same name as in the API and in GitHub's documentation, and the
  table's id prefix tells you the folder. Splitting it into several crates would need
  group names GitHub does not use, and the code gains little: 51k lines is a fine crate
  size, and the groups share about 70 references to `auth`, `json_error`, `node_id` and
  `pagination`. Split it later only if the timing run shows it on the critical path.
- **`db`** is written by domain, not by engine: each file holds the types and a
  `native_operation!` dispatch to `postgres/<domain>.rs` and `sqlite/<domain>.rs`. Splitting
  by engine would make the crates call each other in a circle, so the split is by domain.
  Everything depends on identity (65 to 167 references from each other domain), so identity
  is the lowest. Repositories and pull requests/issues reference each other (53 each way)
  and stay in one crate.
- **Circles to cut** before the matching move: `db` uses identity, ci and vulnerability
  types (move them to `model`); `repositories`, `ci` and `cluster` use each other
  (repositories to ci 43, ci to repositories 14, repositories to cluster 24, cluster to
  repositories 33; the `RepoStore` trait is the cut: the cluster crate implements it and
  `app` injects it); `webhooks`, `ci`, `storage` and `email` reach into `api` (43, 5, 2, 2
  references; the helpers move to `api/http` or `model`); `git` uses `repositories` in 4
  places.
- **GraphQL**: `objects.rs` (78k) and `resolvers.rs` (45k) form one cyclic graph (`User`
  refers to `Repository` and back) and Rust's impl rules keep each type's methods in its
  crate. Only enums, inputs and scalars (about 13k) split off easily. It is already a leaf,
  so it rebuilds only when the schema changes. Whether to touch it depends on the timing run.

## Stages

Each stage is one move, one script that rewrites the paths, and one `cargo check
--workspace --all-targets --locked` on the builder. No behaviour change, no new
re-export layers (callers switch to the new paths in the same stage), and the test count
and results stay the same. A stage can be the last one.

0. **Measure.** `cargo build --timings` once, and once more after touching a leaf file.
   Also check which linker the builder uses and whether the 11 integration test binaries
   (each linking the whole library) can become fewer. Decide the GraphQL question here.
1. **Layout.** Create `backend/`, move `crates/codeyard-actions` to `backend/ci/runner` and
   `crates/codeyard-github-graphql` to `backend/api/github-graphql`. Update paths in
   scripts, `Containerfile`, docs and `AGENTS.md`.
2. **`webui`.** Check also `--target wasm32-unknown-unknown` with the `web` feature.
3. **`support` and `git`.** Cut git's four `repositories` references.
4. **`model` and `db/core` (partial).** `codeyard-db-core` now owns the pool,
   connections, both engines' migrations, record types and `native_operation!` with
   `$crate` paths. `codeyard-model` owns independent shared types and limits; all
   per-domain queries remain in the server crate. The remaining cuts are not just
   data: CI `Permissions` calls workflow parsing, vulnerability `Advisory` methods
   call database advisory validation, and custom secret patterns own scanner state.
   Database domains also still call identity permission/validation helpers, CI
   concurrency/log/workflow helpers, email, issue, webhook and package helpers.
   Repository-secret encryption remains in the server; generic random-token/HMAC
   helpers moved to support. These dependencies must be cut before stage 5.
5. **`db` domains**, lowest first: identity, orgs, repos (with pull requests and issues),
   then actions, security, cluster. Each domain crate keeps its own PostgreSQL and SQLite
   code.
6. **Services:** `identity`, `notifications`, `repositories`, `ci`, `cluster`, in the order
   that the circle cuts allow.
7. **API:** `api/http` first (the helpers `api/github` shares move there), then `api/github`
   as one crate with its folders renamed to GitHub's categories, then `api/repos`,
   `api/graphql` and the rest.
8. **`app`:** `main`, wiring, ssh, cli, backups, node, storage.

## Move only: no behaviour change

The split reorganises code. It fixes no bugs, adds no features and changes no logic. Any
change that is not a move, a path rewrite or a visibility change is a defect in the split.

- Do not edit a function body, a query, a condition, an error message, a status code or a
  default. If a body must change to compile (for example an unreachable private item), move
  the item instead of rewriting it.
- Do not reorder, merge or delete tests. Do not weaken, skip or edit an assertion to make a
  stage pass. The same tests must exist and pass after every stage.
- Do not touch the generated route table or GraphQL output except for its path.
- Do not move a `cfg`, `feature` or `target` gate; keep each item's gates and `#[cfg(test)]`
  modules with the item.
- If a cycle cannot be cut by moving a type or a trait down, stop and report the two parts.
  Do not invent a shim, an adapter layer or a duplicate type to get past it.
- Every commit is one stage. Review it with `git diff -M --stat` and a rename-aware diff:
  the only changed lines inside moved files are `use` paths and `pub` keywords.
- Bugs found on the way are written to a list for the owner. They are not fixed here.

## Rules for every stage

- Widen visibility only as far as a cross-crate use needs: `pub(crate)` becomes `pub` item
  by item, never wholesale.
- Test helpers shared by several crates go in a dev-only crate under `backend/`; production
  code carries no test alternates.
- A type has one home. If two crates need it, it moves down to the lower one.
- The route table stays generated; the script's output path moves with it.
- Acceptance per stage: the workspace check is clean, the tests that ran before still pass
  (a skipped test is not a pass), URLs and wire formats are unchanged. After the last stage:
  fmt, Clippy with warnings denied, the full tests and `scripts/e2e.sh` once.

## Cost and stop rule

About 12 builder checks in all, plus the final gate. If a stage needs a third round of
fixes for new circles, stop and report which two parts depend on each other and a simpler
cut.
