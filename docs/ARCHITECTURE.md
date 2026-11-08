# Codeyard architecture

Codeyard is a self-hosted Git forge for teams: repositories, organizations and teams,
issues, pull requests with reviews and branch protection, a web editor, notifications,
packages and GitHub-Actions-style CI. It is one Rust application and one binary. The
server renders Dioxus pages to HTML and the same views hydrate in the browser.

The same binary runs a single machine, a cluster of machines sharing one PostgreSQL
database, and several such clusters ("cells") behind a router. A single machine needs
none of the cluster parts and may use SQLite.

Other documents: [running a cluster](CLUSTER.md), [cluster readiness](CLUSTER_PLAN.md)
(what is accepted and what is left), [storage](STORAGE_ARCHITECTURE.md),
[Kubernetes](KUBERNETES.md), [CI runner](CI_LOCAL_RUNNER.md),
[codebase style](CODEBASE_STYLE.md) and the [codebase map](CODEBASE_MAP.md), one line per
file.

## Crates

The workspace is flat: `backend/<name>`, `backend/db/<domain>` and `backend/api/<adapter>`,
plus `webui/`. Every package is named `codeyard-<name>`. The root package `codeyard` builds
the binary; its code is in `backend/app/wiring/` (main, CLI, node startup, wiring).

Dependencies point one way, roughly in this order:

```
types → security, git, runner → database → db-* → services → api-* → app → codeyard
webui → types
```

Normal dependencies have no cycles. Backend crates that build page models still depend on
`webui` for its view types; the browser bundle never depends on a backend crate and links
no sqlx, Git, SSH or AWS code (`scripts/check-web-dependencies.py` checks this).

| Crate | Owns |
|---|---|
| `webui` | Dioxus views and safe page models, server rendering and browser hydration |
| `types` | Browser-safe types, syntax rules and limits; server-only models behind `cfg` |
| `security` | Cryptography, token hashes, outbound address checks, secret detection, node keys, private state directories and their file locks |
| `git` | libgit2 reads, hardened `git` processes, repository locks and ref publication |
| `runner` | The Actions workflow runner (`codeyard-actions`), also usable on its own |
| `database` | Pools, migrations, shared records, cross-domain transactions and cell id ranges |
| `db-identity`, `db-orgs`, `db-repos`, `db-actions`, `db-security`, `db-placement` | Domain queries, each with `postgres/` and `sqlite/` implementations |
| `db-directory` | The cells directory database (PostgreSQL only) |
| `identity` | Accounts, logins, sessions, tokens, keys and avatars |
| `storage` | Uploaded files: local directory, S3 or storage-node copies |
| `cache` | Disposable content cache: bounded local tier and optional Valkey |
| `nodes` | Node membership, pairing, iroh and TCP transport, node settings, quorum write decisions |
| `placement` | Node listeners and heartbeats, drain, provisioning, scaling, replicas, CI requests from workers |
| `router` | Cells router: directory client and routing of HTTP, SSH and GraphQL to a cell |
| `notifications` | Notifications, broadcast hints and email |
| `webhooks` | Webhook payloads, subscriptions and delivery |
| `repos` | Repository policy, parsing, imports and editor preparation |
| `git-storage` | `RepoStore` (local and remote), Git sessions and receive hooks, the RPC client, workflow readers and webhook enrichment |
| `repos-rpc` | Storage-node RPC handlers and Git streams |
| `ci` | CI configuration, admission, queues, workers, logs, credentials and environment values, code owners, pull request refresh |
| `repos-pulls` | Pull requests, issues, repository operations and security workers |
| `search` | Repository catalog search and live code and commit search |
| `ssh` | Built-in SSH server and the host OpenSSH gateway |
| `app` | Application state and the composition of domains (`backend/app/src/`) |
| `api-http` | Extractors, sessions, CSRF, origin checks, rate limits, middleware, metrics |
| `api-github` | The GitHub REST surface under `/api/v3` |
| `api-graphql` | `/api/graphql` on top of `github-graphql` |
| `api-repos` | Repository pages, forms and their JSON routes |
| `api-rest` | The router that merges every resource, and the remaining resources |
| `github-graphql` | Types and resolver traits generated from GitHub's schema by `scripts/github-graphql-types.py`; never edited by hand |
| `testing` | Test fixtures; a dev-dependency only |

`repos` and `repos-pulls` are the staging names of one future `repositories` crate above
`git-storage`. Workflow readers and webhook enrichment still live in `git-storage` because
the receive hooks call them.

A type has one home: when two crates need it, it moves down to the lower one. Widen
visibility item by item, only as far as a cross-crate use needs. Fixtures shared by several
crates belong in `testing`; production code carries no test alternates. A cycle is cut by
moving a type or trait down, never by a shim or a duplicate type.

Workspace tests live under `tests/<area>/` (one test binary per area: `database`,
`placement`, `cells`, `git`, `ci`, `storage` and so on) and share `tests/support/`. Crate
tests live in `<crate>/tests/`. Python end-to-end drivers are in `tests/e2e/`, browser
checks in `tests/browser/` and tests of the scripts in `tests/scripts/`.

## Request flows

Every entry point (page, form, REST, GraphQL, Git over HTTP and SSH, background job) runs
the same steps: authenticate, look up, authorize, validate, write, record, respond, map
errors. Authorization is read again in the transaction that writes.

- **Pages.** A resource adapter under `backend/api/` checks access, loads data through
  the domain crates and `RepoStore`, renders a `webui` page model to HTML and embeds the
  same model for hydration. Forms post to native routes that call the same domain
  functions as REST, so pages work without JavaScript.
- **Editor saves and file writes.** Write access and branch protection are checked, then
  `RepoStore::save_file` takes the repository lock, builds blob, trees and commit and moves
  the branch with `git update-ref <new> <old>`. A branch that moved meanwhile is a 409 that
  keeps the user's text.
- **Merges.** Protection rules (approvals, change requests, required statuses, resolved
  conversations, up to date) are checked, then `RepoStore::merge` builds the merge, squash
  or rebase commit and publishes it only if head and base are unchanged. Stacked pull
  requests merge their open lower layers in the same `update-ref` transaction.
- **Branch moves.** Every path that moves a branch (push, save, merge, branch API) calls
  `branches_moved`, which updates the open pull requests with compare-and-swap writes.
- **Fork pull requests.** The head is fetched into the base repository's
  `refs/pull/<n>/head` through a quarantined `receive-pack`, with the author's read access
  checked before and after.
- **Git over SSH and HTTP.** The built-in SSH server checks the key, HTTP checks a token
  (Basic auth). Both call `RepoStore::git_session`, which runs `git upload-pack` or
  `receive-pack` on the storage node that holds the repository. Pre-receive enforces branch
  protection; post-receive updates sizes, pull requests and commit records and queues CI.
  Sessions share a process-wide limit and have idle and total timeouts; a dropped session
  kills its Git process group.
- **CI.** A trigger pins the commit and workflow files and queues a run. Runner nodes claim
  runs (`FOR UPDATE SKIP LOCKED` on PostgreSQL), run them with `codeyard-actions` or `act`,
  stream bounded logs to object storage and finish with a commit status. Runners without a
  database talk to a core node over the node transport. Fork pull requests wait for a writer
  to approve each head and never receive secrets.
- **Uploads.** Reserve (quota and a pending file row), the client uploads to S3 or to
  `/api/files/:id/content`, then confirm (size, checksum, file signature). Deletion is
  durable work that the expiry sweep retries.
- **Notifications.** Changes write notification rows and bounded hints in the same
  transaction. Each web node polls the hints and pushes them to browsers over one
  WebSocket per signed-in page, which then reloads the data it is allowed to see.

## Data

- `DATABASE_URL` selects PostgreSQL; without it a single node uses SQLite at
  `<state_dir>/gitserv.db`. Clusters and cells need PostgreSQL. Both engines run the same
  queries from the `db-*` crates and are tested alike.
- Schemas are `schema/postgres/` and `schema/sqlite/`. Until the next release the `0001`
  baseline changes in place; after a release, migrations are immutable and additive.
- Bare repositories live at `<REPOS_PATH>/<uuid>.git`, so renames and transfers never move
  files. Deleted repositories wait seven days in `<REPOS_PATH>/deleted/`.
- Uploads live in `<state_dir>/objects`, in an S3 bucket, or, in a cluster, as two checked
  copies on storage nodes.
- On PostgreSQL, account and organization authority use row locks scoped to the accounts
  involved. `repository_merge_epochs` is bumped by every write that can change a merge
  decision, at commit time.
- Git processes run with `core.fsync=all`. `codeyard backup` copies the database first and
  the repositories after, so the database is never newer than Git; `codeyard restore` and
  `codeyard repo verify --repair` reconcile the two.

## Clusters

A cluster is one installation spread over several machines that share one PostgreSQL
database. `codeyard serve --roles <list>` picks what a machine does:

| Role | Does |
|---|---|
| `web` | pages, APIs and Git over HTTP |
| `ssh` | SSH logins and Git over SSH |
| `storage` | holds bare repositories and runs Git and hooks |
| `runner` | runs CI jobs; needs no database, dials out only |

`server` is `web`, `ssh` and `storage` together, the single-node default. PostgreSQL is the
source of truth for membership and job queues; there is no gossip.

- **Transport.** Each node has an ed25519 key. Nodes talk over iroh (QUIC, direct by
  default, optional relay) or TCP with TLS 1.3 using the node key. Every connection and
  every request checks the caller's row in `nodes`, so a revoked node is refused on its next
  request. Protocols are `codeyard/pair/1` (joining), `codeyard/rpc/4` (repository calls and
  Git sessions, length-prefixed frames with size limits) and `codeyard/events/7` (CI
  claims, logs and results from database-free runners).
- **Joining.** A node joins with a one-time ticket, a reusable join token, or a key listed
  in `CLUSTER_NODES`. `codeyard cluster revoke` removes it.
- **Placement.** `repositories.storage_node_id` names the primary storage node; NULL means
  the local single-node path. New repositories go to the storage node with the most free
  space; forks stay with their source. `RepoStore` is the one repository interface: `Local`
  when this node holds the repository, `Remote` (RPC to the holder) otherwise.
- **Copies and quorum.** With `write_quorum=majority` (the default for new clusters) a ref
  change is acknowledged only after a majority of up to three fixed voters hold it; PostgreSQL
  decides which changes commit, and term fencing recovers a committed change after the
  primary dies. Further copies are asynchronous: each storage node runs a sync loop that
  fetches a `ReplicaPack` from the primary when its copy is behind `write_version`.
- **Failures.** Reads fall back to a current copy when the primary does not answer. A node
  that holds current copies of a whole fork family takes it over after the primary has been
  silent for a minute; otherwise an operator runs `codeyard cluster promote`. Writes to a
  repository whose primary is down answer 503 until then.
- **Lifecycle.** Drain moves repositories off a node; provisioning, scaling and rebalancing
  run from the admin panel, REST and CLI through the same functions.

Operator steps and settings are in [Running a cluster](CLUSTER.md).

## Cells

A cell is a complete cluster with its own PostgreSQL. Cells share nothing; an existing
installation is cell 0 with no data change. The owner account (a user or an organization) is
the unit of placement: its repositories, issues, pull requests, packages, CI and settings
live in one home cell.

- **Directory.** A separate PostgreSQL database (`db-directory`) maps user and organization
  names, emails and SSH key fingerprints to cells and keeps them unique, and holds the cell
  registry and forwarding entries for moved accounts and repositories. A claim is a pending
  row with an expiry that the cell confirms; there is no two-phase commit.
- **Router.** A stateless `router` role proxies pages, both APIs, Git over HTTP and SSH to
  the right cell: by the owner in the path, by id prefix, by the credential's cell, or by
  placement for signups. GraphQL top-level fields route on their own arguments. Every
  application route has a routing rule, and a test fails when one is missing.
- **Ids.** Every id is `prefix × 2^37 + n`, below 2^53, so ids stay unique without
  coordination and exact in JavaScript. Cell 0 is prefix 0.
- **Accounts across cells.** A cell keeps stub rows for remote users, organizations and
  repositories it refers to, without credentials. Credentials carry their home cell;
  another cell asks the home for each admission and caches nothing. Permission checks stay
  in the resource's cell.
- **Events between cells.** Each cell writes an outbox row in the same transaction as the
  change; a worker delivers events at least once, and receivers apply them idempotently.
  Stars, memberships, mentions, cross-cell pull requests and the shared search index are
  fed this way.
- **Moves.** `codeyard cell move` freezes an account, copies its repositories and rows to
  the target cell, switches the directory and turns the source rows into stubs. Ids do not
  change. A failed move before the switch leaves the source serving.

[Cluster readiness](CLUSTER_PLAN.md) lists what is accepted and what is still open,
including regions, disaster recovery and measured capacity.
