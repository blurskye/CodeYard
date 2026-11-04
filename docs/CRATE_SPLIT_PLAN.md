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
    runner/                    the Actions runner
  cluster/                     cluster and node
  identity/                    login, tokens, keys
  notifications/               notifications, email, webhooks
  api/
    http/                      shared HTTP plumbing (config, context, CSRF, errors, sessions)
    github/                    one crate: the router, the generated route table and one
                               folder per GitHub category (actions/, issues/, pulls/, repos/, ...)
    github-graphql/            generated GraphQL types
    graphql/                   our GraphQL resolvers
    repos/                     web and Git HTTP for repositories
    (rest)                     the remaining routes
  app/                         the `codeyard` binary: wiring, ssh, cli, backups, storage
backend/test-support/          dev-only shared fixtures
tests/                         same tests; imports use the crates' public items
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

Complete and commit every source stage before compiling. Then run one workspace
check on the builder and batch corrections from its log, followed by Clippy and WASM. No behaviour change, no new
re-export layers (callers switch to the new paths in the same stage), and the test count
and results stay the same. A stage can be the last one.

0. **Measure.** `cargo build --timings` once, and once more after touching a leaf file.
   Also check which linker the builder uses and whether the 11 integration test binaries
   (each linking the whole library) can become fewer. Decide the GraphQL question here.
1. **Layout.** Complete: the runner is in `backend/ci/runner` and generated
   GraphQL types are in `backend/api/github-graphql`; build and generation paths follow them.
2. **`webui`.** Native and `wasm32-unknown-unknown` with the `web` feature check.
3. **`support` and `git`.** Moved to `backend/support/` and `backend/git/`; server
   dependencies cut. All workspace targets check; runtime tests are pending.
4. **`model` and `db/core`.** Shared permissions, bounded YAML parsing and glob
   matching, advisory records and validation, snapshots, credential policies, mail
   configuration/texts and URL helpers now live in `model`. Native CI record/log
   helpers live in `db/core`; encryption, SSH selection, node identity and custom
   secret scanning live in `support`. Database domains no longer call server helpers.
   Source moves are complete; all workspace targets check.
5. **`db` domains.** Source extraction complete: `identity`, `orgs`, `repos`
   (including pulls and issues), `actions`, `security` and `cluster` each own both
   engines. Shared transaction helpers and their records live under `db/core/src/queries`.
   All workspace targets check.
6. **Services (partial).** `identity` and `notifications` are extracted. Repository
   authority, identity mail links and delivery helpers have one lower owner.
   `RepoStore` is a concrete enum-backed type, not the trait assumed above; its local
   and remote implementations call cluster RPC and CI, which call the concrete store
   back. These three services remain together under the move-only stop rule. A later
   cut needs the planned store interface and injection, which is more than a move.
7. **API.** Source extraction complete: HTTP plumbing, GitHub REST (one crate,
   with folders named for its operation categories), repository HTTP, GraphQL and
   the remaining routes are separate crates. Shared helpers have one direct lower
   home. The coupled runtime is in the app library so adapters do not depend on
   binary wiring. All workspace targets check.
8. **`app`.** Source layout complete. `backend/app/src` is the app library;
   `backend/app/wiring` contains the existing binary package's entry points, CLI
   and node startup. SSH, backups and storage live in the app library. The source
   stages are committed before workspace, Clippy and WASM checks begin. Broker
   tests now live in `backend/app/tests/git_broker.rs` and link the app library
   with REST through one `AppState`. Hook barriers have one shared owner.
   All workspace targets, Clippy with warnings denied and the web WASM target are
   clean. Runtime tests remain for the lead.

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
