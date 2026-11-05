# Codeyard architecture

Codeyard is a self-hosted Git forge for teams: repositories, organizations and
teams, issues, pull requests with reviews and branch protection, a web editor,
notifications, and GitHub-Actions-style CI. It is one Rust application: the
backend renders Dioxus pages on the server and the same views hydrate in the
browser.

This document covers the current code, repository storage across paired iroh
nodes, and the cluster's current limits.

## 1. Code map (today)

```
Cargo.toml                         workspace and codeyard binary package
backend/app/wiring/                main, binary wiring, CLI and node startup
backend/app/src/                   app state, repositories/store, CI, cluster,
                                  SSH, storage, backups and domain composition
backend/identity/src/              login, accounts, sessions, tokens and keys
backend/notifications/src/         notifications, broadcast hints and email
backend/db/core/                   pools, migrations, records and shared transactions
backend/db/{identity,orgs,repos,actions,security,cluster}/
                                  domain queries, with both native engines
backend/model/                    shared types, validation and policy
backend/support/                  pure helpers and native support
backend/git/                      Git reads and hardened child processes
backend/ci/runner/                native Actions runner
backend/api/{http,github,repos,graphql,rest}/
                                  HTTP plumbing and resource adapters
backend/api/github-graphql/        generated GraphQL schema types
webui/                            SSR-first views, assets and browser hydration
schema/{postgres,sqlite}/          baseline schemas
```

The repository, CI and cluster runtime still share a concrete `RepoStore`; their
remaining service split needs the interface described in the crate split plan.
API crates depend on the app library, and binary wiring composes them from above.

### Request flows

- **Page:** the resource adapter under `api/` uses `PageContext`, checks permissions,
  loads qualified DB operations and `repo_store(&repo, context)`, renders a
  `codeyard_webui::pages::*` view to HTML and
  embeds the same view data for hydration. Markdown (`webui/src/components/markdown.rs`) links `#N`,
  `@user`, commit SHAs and `owner/repo#N` only as far as the page's resolved references
  (`api/repos/references.rs`) allow, so the server and the browser agree.
- **Editor save / API file write:** `repositories::files::write_file_bytes` →
  write-access and `repositories::protection::check_branch_write` → `RepoStore::save_file`: per-repo
  lock → build blob, trees and commit (`editor::prepare`) → `git update-ref`
  with the old value (`editor::publish`); a moved branch is a 409 that keeps the
  user's text.
- **Merge:** `api/repos/pulls/merge.rs` → `repositories/pull_requests/merge.rs` →
  catch up the open pull requests with
  their branches → protection rules (`check_branch_write` for who may move the
  base, then approvals from writers (for the current head unless
  `dismiss_stale_reviews` is off), change requests that still block, required statuses,
  resolved conversations, up-to-date) → `RepoStore::merge`: lock → build the merge
  commit for the merge method (`prepare_merge`: a merge commit, a squash commit, or each
  commit replayed by `backend/git/src/repository/replay.rs`; the last two use `merge-tree --write-tree`,
  which needs Git 2.40 or later, and the runtime image has Git 2.47) → `publish_merge` (moves base only if head and base are
  unchanged) → `mark_pull_requests_merged` → catch up the other open pull requests,
  whose base just moved. `merge_blockers` lists every protection refusal; the merge refuses
  with the first one. The pull request page lists them above the merge button for the method the
  form preselects; this list is advisory and leaves out the stack and branch checks, which the merge
  repeats with every other check. Stacks (`repositories/pull_requests/stacks.rs`): a pull request
  joins a stack only when asked (`POST .../pulls/{number}/stack`, checked and written in one
  transaction in `db/pull_requests/records.rs`); the stack id is stored in
  `pull_requests.stack_id`. Merging a layer merges the open layers below it in the same
  step: the chain is read from the stack (`stacks::chain_below`), each layer is checked against
  the trunk, the heads are verified and the trunk moves in one `update-ref` transaction
  (`publish_merge` takes several heads; a stacked merge uses `RepoStore::publish_stacked_merge_as`),
  and every layer is marked merged with that commit. Only a merge commit merges a layer with open
  layers below it. After the merge is recorded, the members of the stack that were based on the
  merged layer's head branch move to the trunk, best effort, before the catch-up above.
  Rebase stack (`repositories/pull_requests/restack.rs`) replays each layer of the open line of
  the stack, from its lowest layer that is not on top of its base up to the top, onto the head of
  the layer below, using `replay_commits` with the layer's own range, then moves all the rewritten heads in one `update-ref` transaction that checks the
  base tip and each old head (`RepoStore::publish_restack_as`). It refreshes and queues the moved
  pull requests as a push does.
- **Fork pull requests:** the base is the repository where the pull request is
  opened. The head may be that repository or a readable direct fork. `head_repo_id`
  records the source (null after deletion). Preview comparisons use a temporary
  object database that borrows both repositories' objects without changing either
  repository. Opening a pull request explicitly shares its head:
  `RepoStore::fetch_pull_head` checks the author's current source read access, fetches
  into a temporary bare repository, checks access again, and uses quarantined
  `receive-pack` with an exact ref lease to publish `refs/pull/<number>/head`.
  Failed initial publication can be retried without overwriting a later pull ref.
  Both repositories must be on the same storage node; remote callers use RPC.
  Diff, commit reads, protection checks and merges of opened pull requests use the
  base. Merging verifies the pull ref and base branch together.
  Deleting the fork leaves its last commit reachable and mergeable from the pull ref.
- **Branch moves and pull requests:** a pull request records the commits of its
  head and base branches. Every path that moves, creates or deletes a branch (SSH
  post-receive, editor and API file saves, merges, the branch API) calls
  `repositories::pull_requests::branches_moved`, which compares the open pull
  requests of the repository with the current branch tips and stores head and base
  through independent compare-and-swaps that also advance `updated_at`, so forms
  opened earlier go stale. CAS conflicts reload with a small bounded retry, and one
  PR's failure does not prevent later PRs from refreshing. Fork pushes also refresh
  open pull requests in the base repository and their pull refs. Merged and closed
  pull requests are left alone; reopening or readying one catches it up. A deleted
  branch keeps the last commit, and the merge refuses until it is back while its
  repository still exists. A review is accepted only for the published head and is
  never rewritten by a push. Whether an approval still counts after a push is decided at merge
  time by `dismiss_stale_reviews`; a change request keeps blocking until its reviewer posts a
  newer review or an administrator dismisses it.
- **Review pages:** the Conversation tab's review form, the diff's inline composers, and the
  resolve and dismiss forms post to native routes in `api/repos/pulls/review_forms.rs`. Each
  checks the head it was written against, then calls the same repository function as its REST
  endpoint. A diff gutter number links to a composer on that line; threads show under their line
  and conversations on an earlier commit are marked Outdated. The Reviewers panel's request,
  re-request and remove forms use the same native routes and repository functions. Stale
  reviews and outdated conversations are computed when the page is read, never stored. A new
  conversation must sit on a line of the pull request diff; the shared comment path checks that
  for REST and forms alike, and a reply takes its thread root's place.
- **SSH keys:** enrollment and both transports share complete public-key parsing.
  New enrollment is serialized per fingerprint (PostgreSQL advisory lock, SQLite
  write transaction) and refuses keys already registered anywhere. Released duplicate
  rows remain manageable, but authentication refuses them until only one remains.
- **SSH push:** the built-in server checks the key and repo access →
  `RepoStore::git_session` starts an interactive process on the owning storage node →
  `repositories/git_sessions` runs `git receive-pack` with our pre/post-receive hooks.
  The storage node rechecks the live credential and write access; pre-receive
  enforces branch protection through `repositories::protection` (force pushes and
  merge commits are read from the receive quarantine); post-receive updates size and default branch (when
  HEAD is unborn after a push, the first pushed branch becomes the default), catches
  up open pull requests, records pushed commits and queues CI runs. Accepted
  updates use immutable commit IDs even after another push moves the ref;
  annotated tags are peeled to commits while keeping the full tag ref for CI.
  Git itself advertises refs and answers negotiation batches, so fetches do not wait
  for the client to send `done`. Host OpenSSH uses the same process and receive
  session core through its existing Unix-socket frames, but refuses remote repositories.
- **HTTP Git:** `api/git_http.rs` checks Basic auth (personal access token as the
  password), live account status, token scope and repository access. Public clones
  remain anonymous; private reads and pushes challenge for credentials. Settings
  manage named, optionally expiring read/write tokens through session-authenticated
  forms; only SHA-256 hashes are stored. Failed token logins share
  the password-login limiter. Bans and password resets revoke tokens. HTTP pushes
  and built-in SSH call `RepoStore::git_session`. HTTP uses stateless Git; SSH
  keeps Git running for the whole conversation. Both stream pack data with bounded
  buffers and backpressure. HTTP upload-pack requests are buffered up to 16 MiB,
  including after gzip decompression; their responses are streamed. Receive-pack
  bodies and responses are streamed without a pack-size limit. Pushes require
  identity encoding (other encodings return 415).
  Ref advertisement and all transports share a process-wide cap on Git sessions
  (`GIT_SESSION_LIMIT`, default four per CPU core and at least 16). Blame, diff and
  merge-tree children run with an address-space limit (`GIT_COMMAND_MEMORY_MIB`, default 4096, 0 for none).
  Streaming sessions have a five-minute idle timeout measured by traffic in either
  direction and a one-hour total timeout.
  A session waits up to 30 seconds (`GIT_SESSION_WAIT_SECONDS`) for a free slot, then HTTP returns 503 and SSH reports
  "Git sessions busy". A dropped session
  or timeout kills the Git process group; buffered Git and CI commands use the
  same group cleanup on errors or cancellation. Receive tasks keep running after a client
  disconnect: Git rejects incomplete packs and finishes hooks for complete ones.
- **CI:** triggers call `CiRuns::trigger_run` → `ci::admission` pins the
  commit and workflow blobs → `db::ci::enqueue_ci_run`. Nodes with the
  `runner` role use `RunQueue` to claim runs (`claim_ci_run`,
  `FOR UPDATE SKIP LOCKED` on PostgreSQL), run `act`, stream compressed log objects with metadata in `ci_log_chunks`,
  heartbeat, and finish the run with a commit status. Each workflow uses
  `gitserv/workflow/<full path>` (including the filename extension); runs covering
  several workflows use `gitserv/pipeline/<pipeline>`. Older attempts cannot replace
  newer statuses for the same source and commit. Released `gitserv/<file stem>` and
  `gitserv/<pipeline>` protection requirements require every matching source's latest
  run to succeed; stored ambiguous statuses cannot authorize a merge. Unknown CI
  requirements remain missing. Finish and queued-run cancellation return the created
  notification ids after committing, then announce them through
  `NotificationBroadcaster`, including finishes received by the core queue RPC.
  Workers with a database
  call it directly; database-free workers dial a paired core node over encrypted
  iroh. Claims include the repository UUID, storage node address, owner and
  decrypted secrets, at most 100 secrets and 1 MiB of names and values in total;
  a run whose repository exceeds that fails when claimed. Fork claims contain no
  secrets. Checkout uses `RepoStore` on the worker. Core nodes check active
  `runner` membership on every request and bind worker ids to the
  authenticated node key. Heartbeats, log batches and finishes check the recorded
  worker; logs are limited to 16 KiB per line, 64 lines and 256 KiB per batch, and
  a run stores at most 100,000 lines and 32 MiB. The batch that reaches a run limit
  stores one truncation line, and later output is dropped. Only database nodes
  sweep stale runs. Workers try configured core addresses in order and retry
  failed claims with backoff (3–60 seconds). A final result is resent, with waits of up
  to 60 seconds, until the core records it or the worker stops; the worker claims
  nothing else meanwhile. Losing the core does not cancel
  an executing workflow; it finishes or times out, and unsent log lines are
  dropped. Shutdown cancels a run that is still preparing its workspace. Runs whose
  worker stops heartbeating are failed, never retried automatically. Open,
  non-draft pull requests queue workflows on
  opening and each head move, once per workflow and commit. A writer's "Run workflows" or
  Re-run starts a new attempt of a workflow once its latest attempt has finished. Fork
  pull requests wait for a base-repository writer to press "Run workflows" for each head
  commit; approval is stored as `workflows_approved_sha`. Queued pull-request runs retain the exact
  `pull_request_id` used for approval and `is_fork`, so overlapping revisions cannot
  borrow another pull request's approval and workers never decrypt repository or
  organization secrets for a fork run, even after the fork is deleted.
  CI host trust and deployment permissions still apply. The core's
  `CI_TRUSTED_ACTOR_IDS` governs remote claims; runner settings live on the worker.
- **Pages:** native settings and REST share `db::pages`, with transactional current
  administration/write authority. Settings, build failures and deployment failures
  are durable. No publisher, isolated site host or OIDC verifier is configured;
  attempts never pretend to publish successfully. DNS health resolves only, without
  caller-directed HTTP requests. See [Pages and security](PAGES_AND_SECURITY.md).
- **Dependency graph:** `repositories::dependency_graph` pins a Git commit before
  reading the bounded multi-ecosystem manifest/lockfile inventory, merges validated
  snapshots from `db::dependency_graph`, and emits SPDX with unknown metadata marked
  `NOASSERTION`. Generated reports are repository-scoped, immutable and expire after
  one day. Comparison includes locally synchronized OSV findings; `vulnerabilities`
  synchronizes enabled mirrors and reconciles default-branch Dependabot alerts.
- **Security disclosure:** `db::repositories::security` keeps confidential reports
  and replies separate from issues and activity. Native and REST report creation use
  `db::advisories` to add the thread and triage advisory in one transaction. `db::advisories` governs explicit
  publication and local CVE requests under live repository authority; requests do not
  mint CVE IDs or contact a registrar. `repositories::advisories` reuses checked fork
  initialization and locks remediation-fork visibility. Native drafts are keyed by
  repository and advisory/report identity so hydrated navigation cannot carry an
  editable draft onto a different target.
- **Upload:** reserve (quota, pending `files` row) → client PUTs to S3 or to
  `/api/files/:id/content` → confirm (size, checksum, file signature) → active.
  Local PUT/confirm/delete/expiry operations share a per-file lock. Deletion removes
  metadata together with durable `pending_object_deletions` work, attempts object
  removal immediately, and the bounded expiry sweep retries it. The bytes are due one
  hour after the 900-second direct-PUT capability expires. S3 deletion records remain
  and are checked daily, since an already-started PUT can finish after expiry.
- **Pictures:** `identity::avatars` points `users.avatar_file_id` or
  `organizations.avatar_file_id` at the holder's own verified upload with the
  matching reason (`avatar`, `org_avatar`); organizations need an owner or admin,
  checked in the same transaction (`db::organizations::profiles::set_organization_avatar`). The settings
  forms send the picture itself (`multipart/form-data`, no JavaScript needed): the
  server reads the type from the bytes, stores and verifies it as a public upload
  through `files::store_upload` (same quotas and checks, local or S3). The upload stays
  pending until assignment activates it. User and organization assignment lock the
  parent row, activate and validate the target, remove the picture
  it replaced unless another account still selects it, and return the previous picture
  id, all in one native transaction. Generic file deletion refuses a selected
  organization picture. User pictures are managed only through the native settings
  upload and remove forms.

Organization OIDC configuration lives only in the web nodes' environment. The existing
OAuth routes bind state to the browser, provider configuration and PKCE verifier; signed
ID tokens use the database clock. Login commits the session and organization audit event
only after rechecking active membership. Private nodes enroll an existing member through
`codeyard user enroll-oidc`; email claims never link identities.

### Data

- `DATABASE_URL` selects PostgreSQL; without it the server uses SQLite at
  `<state_dir>/gitserv.db` for plain default-role single-node serving.
  Explicit partial core roles and cluster mode require `DATABASE_URL`.
  Worker-only serving without `DATABASE_URL` creates no database or application
  state and opens no HTTP, SSH or inbound cluster listener. Its dialing endpoint
  uses an ephemeral UDP socket. `0001` is the unreleased baseline on both engines and
  changes in place until the next release; databases created by v0.1.5 or earlier are
  refused. After a release, migrations are immutable and additive. Each PostgreSQL
  migration statement stops after 120 seconds.
- Bare repositories live at `<REPOS_PATH>/<uuid>.git`, so renames and
  ownership changes never move files.
- On PostgreSQL, authority checks take shared locks scoped to the affected account ids,
  sorted and deduplicated. Account changes take those account locks exclusively; org
  authority is retained through org and membership rows. Unrelated accounts and orgs do
  not share a membership lock. A fork's sources
  are locked before the fork, lowest id first, in every path that locks both: authority
  checks, notification reads and fork deletion (`lock_fork_ancestors`). Reads take the
  sources FOR SHARE; deletions and privacy changes take them FOR NO KEY UPDATE.
- `repository_merge_epochs` is one hot row per repository, bumped by every write that can
  change a merge decision; on PostgreSQL its triggers are deferred, so a transaction takes
  it only at commit and never holds it across Git, network or file waits.
- Single-node uploads live in `<state_dir>/objects`. Cluster uploads require two
  storage-node copies or the S3 bucket from `S3_*`; Git repositories stay on storage disks.

## 2. Clustering

### Goals

- Add web, SSH, storage and CI machines to one installation.
- Repositories are spread across storage nodes (sharding), and each can keep replica
  copies on other storage nodes.
- Nodes that can't accept inbound connections (NAT, office runners) can still
  join.
- A single machine stays exactly as simple as today: it is a one-node cluster.

Replicas are asynchronous: a copy is current only after the sync loop has brought it up,
so a push can be lost if its primary is lost first. Read-only requests fall back to a
copy when the primary cannot be reached. A node that holds a current copy of every member
of a fork family takes the family over once its primary has been offline for a minute; a
family with a behind or missing copy waits for `codeyard cluster promote --force`.
The write quorum selects up to three storage voters and uses PostgreSQL decisions, with
canonical refs and fenced term directories. Empty initialization and repository copies
use that coordinator too. Read distribution and geo-routing remain future work.
Cluster files use two checksum-checked storage-node copies, or shared S3 when configured.
Cluster backup reuses replica snapshots and object placement. Restore imports a fenced
database, installs each original storage identity, and completes through offline verification;
`restore_pending` blocks core startup until the required copies are installed.
Storage-node HTTP acceptance passes on both engines. Cluster backup and restore pass
CLI acceptance, including interrupted import and per-node recovery. Single node remains supported.

Node membership (`pending`, `active`, `revoked`) is separate from allocation state
(`active`, `draining`, `drained`). Drain admission reserves complete fork families and
15% free space in one transaction; the existing copier and promoter advance the work.
The panel, REST and CLI use `cluster::drain::change`. Initialization, REST drain,
promotion and native/hydrated browser acceptance pass. Finished-CI drain acceptance
requires a writable delegated cgroup on the builder.
Application database queries share `db::pool` acquisition tracking; metrics count yielded
acquisitions until they complete, time out or are cancelled. Streaming keeps one queued
row and releases the query and connection together when cancelled. Acquisition, cancellation
and HTTP waiter metrics pass acceptance. New clusters choose majority by default; existing
asynchronous installations explicitly choose `cluster enable --write-quorum 1`. That
offline transition preserves existing repositories and passes acceptance.
Provisioning, lease-led scaling, rebalancing and pool configuration pass library and
HTTP checks on both engines. [Cluster readiness](CLUSTER_PLAN.md) tracks acceptance.

### Roles and shared state

Every machine runs `codeyard serve --roles <list>`. An installation is in single-node
mode until `codeyard cluster enable` stores cluster mode in `node_configuration`.
Single-node mode has two roles: `server` (web, ssh and storage together, the default
with `runner`) and `runner`. The cluster transport (any `CLUSTER_*` or `IROH_*`
variable) can be open in either mode, because remote runners use it; in single-node
mode it admits only runner nodes. Cluster mode allows the split roles:

| Role | Does | Needs |
|---|---|---|
| `web` | pages, API, smart-HTTP git | database (PostgreSQL in a cluster) |
| `ssh` | SSH auth and git over SSH | database (PostgreSQL in a cluster) |
| `storage` | owns bare repositories on its disk, runs git and hooks | database, its disk |
| `runner` | runs CI jobs; alone without `DATABASE_URL`, dials out only | paired core connection, `act` and its execution environment |

"Core" nodes are those with database access (`web`, `ssh`, `storage`). Web and
SSH nodes can serve remote repositories without the `storage` role. Storage-only
nodes serve paired nodes over iroh without opening HTTP or SSH listeners.

- **PostgreSQL** is the source of truth for everything, including cluster
  membership and the job queues. A cluster requires
  PostgreSQL; SQLite is single-node only and the server refuses to start a
  second node against it.
- **Uploaded files** use two storage-node copies, or a shared S3 bucket. Git stays on storage disks.
- **No gossip.** Membership is the `nodes` table with heartbeats.

### Node identity and transport: iroh and TCP

Each node has an ed25519 key, which is its iroh endpoint id and its TLS identity on
TCP. Node-to-node traffic uses one of two transports, and both authenticate the peer by
its key. iroh is QUIC: it tries a direct connection first and falls back to a relay only
when direct fails. TCP is TLS 1.3 with the node key as a raw public key, for networks
where UDP is blocked (see `tls.rs`).

- TCP listener: `CLUSTER_TCP_BIND` accepts TLS connections. A dialer tries a peer's TCP
  addresses first, with five seconds per attempt, and uses iroh only when the peer also
  advertises iroh addresses. A node with `CLUSTER_TCP_BIND` and no `IROH_*` variable opens
  no UDP socket. On TCP each connection carries one request stream and then closes.

- Relay default: **none** (direct only). Settings: `IROH_RELAY=<url>` for a
  self-hosted relay, or `IROH_RELAY=n0` to opt in to public relays. Without a
  relay, a node behind NAT or a container port mapping tells peers where to
  dial with `IROH_ADVERTISE=host:port,...`.
- Every incoming connection is checked against `nodes`: unknown, pending or
  revoked keys are closed (code 403) right after the handshake, before any
  request is read. RPC also reads membership once before executing each
  request, so a revoked key is refused on its next request even on an existing
  connection. No connection tracking or membership cache is needed.
- `serve` opens the iroh socket only when a cluster setting is present
  (`NODE_KEY`, `NODE_KEY_FILE`, `CLUSTER_NODES`, `CLUSTER_NODES_FILE`,
  `CLUSTER_JOIN_TOKEN`, `CLUSTER_PEERS`, `CLUSTER_TCP_BIND`, `CLUSTER_TCP_ADVERTISE`,
  `IROH_BIND`, `IROH_RELAY` or `IROH_ADVERTISE`), and not when `CLUSTER_TCP_BIND` is
  set with no `IROH_*` variable. Worker-only serving uses a dialing-only endpoint, which
  is not created when no peer needs iroh. A plain node opens nothing, and a SQLite
  installation refuses cluster mode.
- Protocols are ALPNs with the same names on both transports:
  - `codeyard/pair/1`: the join handshake (see Pairing). The only protocol open
    to keys that are not members yet; it reads one request, answers and hangs up.
  - `codeyard/rpc/4`: repository request/response calls, including gist token
    credentials bound to both their ID and authenticated hash. Peers using an older
    generation fail ALPN negotiation before a request is decoded. Each bidirectional
    stream carries one request: a four-byte big-endian JSON length, JSON, a
    four-byte raw-body length, and the raw body. The response has the same
    framing. JSON frames are limited to 8 MiB and response raw frames to 128 MiB;
    lengths are checked before allocation. Buffered requests are bodyless except
    editor preparation and save, whose raw body is limited to 1 MiB. The storage
    node authorizes those operations before accepting their bytes and rechecks
    authority after the complete upload. Blobs and archives use response raw
    frames, not JSON byte arrays; each buffered download response is capped at 32 MiB,
    and raw blob byte ranges remain available. These calls time out after five minutes,
    and retain `ErrorKind` and `MergeFailure`. Git sessions instead start with a
    JSON repository/service header and an admission reply. Header admission has a
    bounded wait and releases its buffered-RPC slot before the separately limited
    Git session starts. Input then runs as raw bytes until stream EOF. Output uses
    bounded 32 KiB binary frames, followed by
    an empty frame and a JSON exit status with a 16 KiB stderr tail. Pack data has
    no total size limit. Storage runs the same local session and receive hooks.
    A storage node lost during a session ends the client stream. Dialing an
    unavailable node gives up after at most fifteen seconds (ten for its TCP
    addresses together, five for iroh) and returns `Unavailable`,
    displayed as HTTP 503 "storage node unavailable". Active `web` or `ssh`
    callers may send every operation because they authorize users before calling.
    Callers without either role need `runner` and may send only
    upload-pack Git sessions and the existing permitted CI object reads. Every such request
    checks for a `running` CI run for that repository whose recorded worker id
    starts with the caller's public key followed by `/`. Storage-only callers
    may send no repository operations. Storage nodes send each other only
    `ReplicaPack`, so a copy holder can fetch packs from the primary. Role and claim
    refusals return `Invalid` and log only the operation and caller node name, not
    request contents.
  - `codeyard/events/7`: CI claim, heartbeat, authenticated job/step observations, bounded job-tagged log batches and finish
    requests for database-free workers. Peers using an older generation fail ALPN
    negotiation before a request is decoded. Each stream carries a length-prefixed
    JSON request and reply using the same 8 MiB framing limit as repository RPC.

### Pairing

Three ways for a node to join, all ending with the node's public key in
`nodes` with its allowed roles.

1. **Interactive ticket.** On a core node or the admin page:
   `codeyard cluster invite --roles storage` prints a one-time ticket: the
   core node's endpoint address plus a random secret, expiring in 1 hour. On
   the new machine, load the ticket into `CLUSTER_JOIN_TOKEN` and run
   `codeyard cluster join` (literal argv tickets are refused). The node generates its
   key (saved in its state directory), connects, proves the secret, and is
   recorded as `active`, or `pending` if the invite requires approval
   (`--approve`, then `cluster approve <name>`). `invite` takes the core node's
   address from `nodes.addr_hint`, which the running `serve` keeps current, so
   start `serve` in cluster mode first.
2. **Reusable join token (autoscaling).** `cluster invite --reusable --roles
   runner --expires 30d` makes a token kept in your secret store as
   `CLUSTER_JOIN_TOKEN`. Every node started with it pairs itself on first boot
   and reuses its saved key afterwards. Deleting the token stops new joins;
   existing nodes keep working until revoked. Tokens expire (365 days at most);
   `cluster invites` lists the open ones and `cluster cancel-invite <id>` cancels
   one; nodes that joined with it stay.
3. **Declarative keys (GitOps, fully deterministic).**
   `codeyard cluster keygen` prints a secret key and its public key. The secret
   goes to the node through your secret store (`NODE_KEY` or `NODE_KEY_FILE`);
   the public keys and roles are listed for core nodes in
   `CLUSTER_NODES=<pubkey>:<roles>:<name>,...` (entries separated by commas or
   new lines, roles joined with `+` as in `web+ssh`; or `CLUSTER_NODES_FILE` for
   a mounted file). Core nodes upsert those rows at startup, so the cluster's
   membership is whatever the config says: no join step, no state to lose, the
   same node identity on every redeploy. A revoke wins over the list: a revoked
   node stays revoked until `cluster approve`, and removing an entry does not
   revoke it (use `cluster revoke`).

A core node (one with database access) registers itself as `active` at startup
with its `--roles`, unless `CLUSTER_JOIN_TOKEN` is set, in which case it pairs
like any other node.

Workers find core nodes via `CLUSTER_PEERS=<pubkey>@<host:port>,...`, in listed
order, followed by the core address saved in `<state_dir>/cluster-peer.json`
when joining with a ticket or token. The saved file contains only the core's
public key and address, not the invite secret. Workers reuse their node key
and saved address on restart; they do not need the join token again.
Storage addresses come with each claim, so workers never query `nodes`.

We do not derive node keys from a shared seed: anyone holding the seed could
be any node, which is the shared-secret problem with extra steps.

`codeyard cluster revoke <name>` sets the row to `revoked`. Every incoming
connection and every RPC request checks that row. A revoke therefore refuses
the next call without waiting for a membership refresh. `/admin/nodes` lists
membership and offers approval and revocation; invites are made with the CLI.

### Repository placement and storage

- `repositories.storage_node_id` names the owning storage node; NULL keeps
  the single-node local path. Storage nodes report `nodes.disk_free` from
  their repository filesystem on startup and every 15-second heartbeat.
- New repositories use the active `storage` node with the most reported free
  space, among nodes seen within 60 seconds. A node is seen only after its services
  start, and a clean shutdown withdraws it at once. Serving nodes and repository CLI
  commands use the same placement rule; CLI commands reuse the paired node's
  identity with an ephemeral dialing endpoint.
  Without cluster context it stays local and the column stays NULL. In a cluster,
  no eligible storage candidate means creation is unavailable; there is no silent
  local fallback. Forks always use their source's node.
  Repositories from before the cluster have no node: `codeyard cluster adopt`,
  run once on the node that holds their directories, gives them to that node.
- Creation inserts the authorized database row first, then initializes Git.
  Failure removes the row and attempts to remove its directory. No database
  transaction spans Git or network work. Deletion removes the row first,
  then removes the directory on its owning node.
- RPC names repositories by validated UUID, never a directory. The storage
  node derives `<REPOS_PATH>/<uuid>.git` and checks the row belongs to it.
  Removal also works after the row is gone. Only nodes serving the `storage`
  role accept RPCs.

### Replicas

- `repositories.write_version` counts the ref changes made on the primary. A write adds one
  before Git makes its change: `begin_ref_change` (`store/local.rs`) counts it, for every
  write path (push, branch and tag changes, file save, merge, pull-request head, default
  branch). Queuing a CI run also advances the version in its database transaction,
  because copies must retain that run's commit even after its branch is deleted.
  A write that fails after it was counted, such as a push that is refused or a
  save that conflicts, still adds one, since a push can move some refs before it fails. If
  the count cannot be recorded, or the node no longer holds the repository, the write is
  refused before Git changes anything. The primary also counts each change in memory, from before
  the database count until Git has made it. A repository that no storage node holds (a
  single-machine installation) has no copies and counts nothing. Repository creation and
  forks (`repositories/lifecycle.rs`) add their count after their Git work.
- `repository_replicas` names the copies. A copy is current when its `applied_version`
  equals the write version. `node_configuration.replicas` holds the setting: `1`, a count,
  or `all`.
- Every storage node runs a sync loop (`cluster/replicas/`) every 5 seconds. A node acts
  only on its own primaries and its own copies, and the nodes coordinate only through the
  database. Each pass first takes over fork families whose primary has been offline for a
  minute (see take-over below). Then, for its own primaries, it adds and removes copies to
  match the setting, using `plan.rs`, and moves each directory to the place its role names:
  under `mirrors/` for a copy, at the primary location for a primary. It starts copies of
  the repositories it holds that are behind, and removes copies it no longer needs, never a
  primary's directory. Pack files that a crash left in `mirrors/` are removed once they have
  not been written to for a day. Copies run in the background, at most two at a time; completed
  slots take the next pending copy without rescanning the database until the next pass. A pass never
  waits for them, so a long copy holds up no take-over and no other repository's placement,
  though it takes one of the two slots. A copy reads the write version first. Then, in
  `transfer.rs`, it asks the primary for a `ReplicaPack` (a streamed RPC, like a Git
  session), indexes the pack with `git index-pack`, sets the refs to the primary's listing
  with `update-ref`, and records the version it has applied. Obsolete refs that block a new
  ref's namespace are deleted first. The listing is
  every ref under `refs/`, so pull request heads, which fork pull requests need after a
  promotion, come along (pages and the API list only branches and tags).
- A primary serves `ReplicaPack` only to a storage node that holds a copy row for the
  repository, and only while no ref change to the repository is counted in memory. Otherwise
  it replies busy: the copy records nothing and asks again on its next pass. This keeps a
  copy from claiming a version whose change it does not hold yet. The pack holds the objects
  that the listed refs and commits retained by CI runs need beyond the copy's tips.
  The pack is built from the same ref listing the copy receives, so a ref never points at
  objects the copy lacks. A bundle was tried first, but it omits refs whose tips are
  ancestors of the copy's tips.
- `promote_repository` moves one repository's `storage_node_id` in one transaction and
  leaves the old primary with a copy. It refuses a copy that is behind unless forced (a ref
  change that is counted but not yet made leaves the copy behind), and caps the other
  copies' applied versions at the promoted copy's, so a copy that was ahead
  copies again. Promoting an older copy also advances the write version, so a transfer
  from the discarded history cannot become current if the old primary returns.
  A manual promotion checks a fork family as a whole before anything moves,
  and then its members move one at a time (`promote.rs`).
- Take-over: each pass of a storage node's loop first calls `take_over` (promote.rs). The
  node remembers, in memory, since when it has seen each active primary not reporting in
  (`Silent`), and drops the entry when the primary reports again. For a primary seen
  offline for `TAKE_OVER_AFTER` (a minute), it takes over each fork family whose members are
  all on that primary, when it holds a current copy of every member. `promote_fork_family`
  moves the whole family in one transaction: it locks the family's rows and rechecks the
  primary and the copies, so one node wins and no family is split. A family that cannot be
  taken whole is checked again on every pass, and the skip is logged once per outage. Revoked primaries are not
  touched, and a node that restarts counts again, so restarting the cluster moves nothing.
  The same pass then moves the directory into place.
- Read failover: `rpc/client.rs` and `rpc/git_session.rs` retry a read-only operation
  (`Operation::is_read_only`, in `rpc/wire.rs`) on the copies when the primary gave no
  reply. The nodes are looked up only then: the repository's primary if a promotion moved it
  while the request waited, then online storage nodes that have applied a version, the
  highest applied version first, then node id. The first copy that answers
  is used, and one warning is logged. A reply from the primary, even an error, is final,
  and a Git session is retried only before its first reply. A node that holds a copy row
  answers such reads from `mirrors/<uuid>.git`, or with `Unavailable` when that directory
  is missing. A node cannot dial itself, so a read that falls to its own copy runs the same
  handler in process (`exchange_here`, `git_session_here`). A dial that failed is
  remembered for five seconds (`NodeContext::unreachable`), so later requests for a dead
  primary go straight to the copies.

### One repository interface: `RepoStore`

Callers ask for `repo_store(&repo, node_context)`
(`backend/repos-core/src/repositories/store/`). Node context holds this node's endpoint,
identity, optional database pool, learned peer addresses and repository root;
it is passed explicitly so several
nodes can run in one process. Plain single-node callers pass None. Only
`local.rs` opens a repository directory or runs `git` against one. Inputs and
outputs are owned, serializable values:

- `Local` when `storage_node_id` is NULL or names this node, preserving the
  single-node implementation.
- `Remote` otherwise: the same calls as RPCs over iroh to the owning node.

Operations: tree/blob/commits/diff/refs/archive/languages/contributors reads;
read-only cross-repository comparisons;
CI admission reads and CI checkouts; editor commit (`save_file`); merge
(`merge`, or `prepare_merge` + `publish_merge`); branch/tag create/delete;
repository create/fork/remove; git sessions (ref advertisement, upload-pack, receive-pack). A
single-node install is a cluster where everything is `Local`. Remote CI
checkouts stream the pinned commit's pack into a temporary file and materialize
it on the calling node; no destination directory is sent to the storage node.

HTTP ref advertisements remain small buffered RPCs. Clone, fetch and push use
streamed Git sessions, including when the repository is on another node.
Git sessions send the SSH key or token ID, never a user ID. The storage node requires an active caller
with the `web` or `ssh` role, verifies repository placement and rechecks credentials after
waiting for its session slot. Receive hooks repeat write authorization locally. Database and CI resources remain
explicit inputs to the store's receive operation.

`local_path_for_spawning_git` is only for the host OpenSSH streaming gateway;
a remote store refuses it with a message directing clients to HTTP or built-in SSH.

No `disk_path` reaches page data: the `webui/src` page models have no path
field, and their tests assert the key never appears in serialized page data.

### Cluster flows

- **CI workers:** claim jobs, send logs and fetch the repository from core
  nodes over iroh. They need no database credentials and can sit behind NAT.
- **A storage node is down:** reads of its repositories are served by a copy when one
  has copied them. Writes to them, and reads of repositories without a copy, answer 503
  (`storage node unavailable` from the API and Git). Everything else keeps working. A
  node holding a current copy of every member of a fork family takes the family over
  after a minute (see [Replicas](#replicas)).

### Events

Runtime starts one notification cursor poller per web node before serving and
stops and joins it before closing the pool. Both database engines store bounded
hints in `notification_events`; batches contain at most 256 rows. Polling backs
off from 50 to 500 ms when idle and holds no connection between queries. Hints
expire after one hour through bounded cleanup. PostgreSQL transaction ids and
remembered active transactions prevent late commits from being skipped without
a shared commit lock. An outage closes local subscriptions so browsers reload
the inbox when reconnecting.

The node fans hints out in memory and ignores its own announcements. The signed-in
header owns one WebSocket and refreshes authorized page data on a hint or reconnect;
there is no per-browser polling timer. Socket delivery checks the original live
session and current repository access before loading content or counts. CI status
remains a database read; log reads load bounded metadata followed by checked object bytes.

### Schema additions

```
nodes          id, public_key UNIQUE, name UNIQUE, roles, status (pending/active/revoked),
               addr_hint, version, disk_free, last_seen, created_at
join_tokens    id, token_hash, roles, reusable, requires_approval, expires_at, uses,
               created_at
repositories   + storage_node_id (NULL = this single node), write_version
repository_replicas  repository_id, node_id, applied_version (-1 = nothing copied), synced_at
node_configuration   + replicas ('1', a count, or 'all'), cluster_enabled
```

All of these are in both baseline schemas. `public_key` is lowercase hex, `name` uses lowercase
letters, digits, `.`, `_` and `-`, and `roles` is a text array on PostgreSQL
and a JSON array on SQLite. Storage heartbeats keep `disk_free` current;
non-storage nodes leave it NULL.

### Configuration

| Variable | Meaning |
|---|---|
| `NODE_NAME` | human-readable name (defaults to hostname) |
| `NODE_KEY` / `NODE_KEY_FILE` | node secret key (otherwise generated in the state dir) |
| `CLUSTER_JOIN_TOKEN` | reusable join token |
| `CLUSTER_PEERS` | ordered, comma-separated `<pubkey>@<host:port>` core addresses for database-free workers |
| `CLUSTER_NODES` | declarative allowlist on core nodes: `<pubkey>:<roles>:<name>`, roles joined with `+` |
| `CLUSTER_NODES_FILE` | the same list in a file |
| `IROH_BIND` | UDP address for iroh (default `0.0.0.0:7444`) |
| `IROH_ADVERTISE` | `host:port,...` peers should dial when the bound address is not reachable |
| `IROH_RELAY` | unset (direct only), a relay URL, or `n0` |
| `CLUSTER_TCP_BIND` | TCP address for node connections over TLS (off by default); without any `IROH_*` variable the node runs without iroh |
| `CLUSTER_TCP_ADVERTISE` | `host:port,...` peers should dial over TCP (default: the bind address, unless it is unspecified) |

### Code map additions

```
backend/app/wiring/backend/node/  binary startup and runtime ownership
backend/support/src/node_identity.rs  node key loading and generation
backend/cluster/core/src/
  pairing.rs      invitations, enrollment and revocation
  transport.rs    iroh dialer, addresses and membership checks
  tcp.rs, tls.rs, link.rs  authenticated TCP dialing and shared request links
  settings.rs     node configuration and startup limits
  writes.rs       quorum preparation, votes and publication decisions
backend/cluster/src/
  lib.rs          listener startup, node registration and heartbeat
  tcp.rs          authenticated TCP listener
  ci.rs, worker.rs  core CI requests and database-free workers
  drain.rs, provision.rs, scaling.rs  node lifecycle and scaling policy
  replicas/       owned copy queue, snapshots, promotion, catch-up and rebalancing
backend/repos-core/src/
  context.rs      node identity, repository roots, term fencing and addresses
  wire.rs, rpc_client.rs, git_transport.rs  repository messages and Git transport
  cluster/        placement and shared replica policy
  repositories/store/  RepoStore and local Git publications
backend/repos-rpc/src/  repository handlers, Git streams and local RPC dispatch
backend/db/cluster/src/  cluster queries, with postgres/ and sqlite/ implementations
```

### Tests

`tests/storage/remote_store.rs` runs two direct-only loopback iroh nodes against
PostgreSQL with separate repository roots. It creates and forks through HTTP,
compares Remote operations with Local results, and checks membership, frame
limits, placement refusals, removal and storage outages. `tests/git/remote.rs`
runs real server processes and Git clients against remote storage: HTTP token
and built-in SSH key pushes, clone/fetch, protected branches, revoked/read-only
credentials, storage-side role and placement checks, pull request refresh and
queued CI runs. `tests/ci/workers.rs` runs a database-free worker against a
PostgreSQL core, including checkout, secrets, fork isolation, ownership,
revocation, log bounds and the real worker loop with a test executable in
place of `act`. Repository RPC tests refuse worker writes and all storage-only
calls, and allow worker CI reads only while that node holds a running claim
for the requested repository. Claim queries are tested on PostgreSQL and SQLite.
It also checks worker-only CLI startup and SIGTERM shutdown
without a database or service listeners.

`tests/cluster/replicas.rs` checks the replica rules on both engines (setting, versions,
copies, promotion, counts). On PostgreSQL it starts real in-process storage nodes: copies
follow commits, tags, branches, force moves, an empty repository and a 20 MiB file; a node
without a copy row is refused; the setting adds and removes copies; the loop copies on its
own and stops at shutdown; a promoted copy takes writes and the old primary follows it.
With the primary stopped, a web node's reads (blobs, refs, advertisements and an upload-pack
session) come from the copy, while writes are refused, a node without a copy row refuses
reads, a copy that never finished copying is unavailable, and a not-found answer from the
primary does not fail over. A node takes a fork family over only from a primary that has not
reported for the whole wait, only with a current copy of every member, and not from a
revoked primary; the wait starts again when the primary reports in.

`scripts/e2e.sh` runs SQLite, PostgreSQL and a four-process localhost cluster on
one machine. The cluster uses one PostgreSQL database, separate node disks
and ports, direct-only iroh, one web/SSH node, two storage-only nodes and a
worker without database credentials. It pairs nodes using a ticket, a declared
key and a reusable token. It checks membership through the CLI and admin page,
repository and fork placement in PostgreSQL, SSH and HTTP Git, protection,
editor saves, merges, worker identity, CI pages/logs and fork approval without
secrets. Stopping a store returns 503 for writes to its repositories, and for reads
when no copy is usable, while the other store and web readiness keep working; restarting
it restores access. Worker
revocation keeps new jobs queued until approval. Every node must exit cleanly
on SIGTERM with no panic in its log. The round temporarily revokes one store
when creating the other store's control repository, so shared-filesystem free
space reports do not make placement nondeterministic. The round also keeps a copy of
a repository on store2 (replica setting 2), stops store1, serves its page, API and
clones from the copy, refuses a push, reports 503 while store2 is revoked, waits for store2
to take the repository over after a minute, pushes to it, checks that store1 returns as a
copy that catches up, and promotes store1 again with the CLI.

Run all three rounds with
`GITSERV_TEST_DATABASE_URL=postgres://USER:PASSWORD@HOST:5432/DATABASE E2E_TIMEOUT=3500 bash scripts/e2e.sh`.
`E2E_DATABASES=cluster` selects only the cluster round; missing PostgreSQL fails
the round rather than skipping it.

### Behavioral contracts

- One-node install behaves exactly as before; all existing tests pass.
- Every repository operation works when the owning storage node is another
  machine.
- A node joins with each of the three pairing methods; an unknown or revoked
  key can't connect.
- A CI worker with no inbound ports and no database credentials runs jobs.
- Live CI logs and notifications reach browsers on every web node.

### Open items

- iroh is pinned at `=1.2.0`, the newest release that was two weeks old when it
  was added. It needs the final rand 0.10 crates. russh 0.56 is built on
  release-candidate crypto crates that cannot share a dependency graph with
  them, so russh moved to 0.63. Re-check both pins when upgrading either.

## 3. Scaling out: cells behind a router (plan, not built)

Stage 0 reuses the durability work from cluster plan items 3 and 11. Stage 1 already
has account and package lock scoping, cursor polling and configurable pools. Its new
object-store log chunks, current-copy clone distribution and issue/PR read routing are
written and pass workspace, Clippy and WASM checks. Initial CI, database, cluster,
PgBouncer and quorum acceptance passes. The rerun passes the CI-engine library, all 226
cluster tests, 168 database tests and 100 storage tests; a log-delivery deadline fix and
replica-client Python fix still await rerun.
Stage 2 ID initialization, credential framing, directory queries and operator commands pass
workspace, Clippy and WASM checks. The directory TLS transport and shared root-bearing
node-ID codec, route cache and native cookie/device-form follow-up also pass these checks;
runtime tests are pending;
the public router, foreign authorization and stubs are not built;
stages 3 to 6 are not built.
The owner authorized cells on 2026-11-05 without waiting for A16. Capacity
numbers remain assumptions until stage 1 measures a real cell.

Two findings set the order of work:

- **Durability comes before cells.** Git processes set `core.fsync=all` and
  `core.fsyncMethod=batch`, libgit2 fsync is enabled, and cluster plan items 3 and 11
  provide quorum publication, copies, backups and fenced restore. Stage 0 extends
  acceptance to three voters and every ref publisher; VM hard-reset acceptance remains
  an operator check.
- **One large cell still needs work.** Account authority locks are scoped by account,
  organizations use row locks, names and email addresses reserve their own keys, and
  package reads use snapshots without package locks. Attestation and metadata changes
  serialize by owner; orphan collection locks only candidate bundle rows.
  Notification hints use bounded cursor polling and the pool is configurable. CI logs
  use compressed objects with metadata in `ci_log_chunks`; uploads recheck the worker
  before committing their pointer. Stage 1 acceptance and measured capacity remain open.

So stages 0 and 1 work inside one installation, and cells come after. "With ease" does not
hold at 10 billion accounts: every part is well known, but the base case is about 1,000
PostgreSQL clusters and 7,000 storage nodes, and running them is the real work.

**Baseline.** GitHub runs MySQL on Vitess and Git on Spokes, which acknowledges a push once
a majority of replicas hold it. GitLab's Gitaly votes on ref updates through Git's
`reference-transaction` hook, and GitLab Cells places each Organization in one cell behind
a global service for names, emails and tokens. Slack's workspace shards broke when shared
channels made cross-workspace work common. We follow GitLab Cells for placement, Spokes and
Gitaly for ref quorum, and Notion's logical shards for the one global table.

### Stage 0: durability, on one installation and in every cell

Stage 0 needs no cells or router. It closes every gap where acknowledged data can be lost.

- **Single-node part, built:** `core.fsync` on every Git process, `codeyard backup` and
  `codeyard restore`, `codeyard repo verify [--repair]`, `codeyard repo rebuild`, and the
  7-day restore window for deleted repositories and uploads. `scripts/e2e.sh` checks
  acceptance items 5, 8 and 9 on SQLite and PostgreSQL and item 7 on PostgreSQL. Item 4
  needs a VM hard reset and is checked by hand. Operations are described in
  [Storage architecture](STORAGE_ARCHITECTURE.md#backups).
- **Cluster part:** fixed-voter ref quorum, PostgreSQL arbitration and
  ref journals, snapshot backups and fenced offline restore are implemented. Operators
  provide PostgreSQL HA. Three-voter process-loss acceptance passes; VM hard-reset
  acceptance and measured limits remain pending;
  [Cluster readiness](CLUSTER_PLAN.md) is the current acceptance list.

| Store | Decision | RPO | RTO |
|---|---|---|---|
| Git refs and objects, cluster | acknowledge after a quorum of copies hold the change (below) | 0 while a majority of voting copies survive | take-over after 1 minute |
| Git, one machine | `core.fsync=all`, `core.fsyncMethod=batch` on every Git process (`backend/git/src/repository/process.rs`); `codeyard backup` | 0 on a crash; the last backup on disk loss | restore time |
| Derived Git data: pack indexes, bitmaps, commit-graphs, the language and contributor tables | not backed up or replicated; `codeyard repo rebuild` makes it again | none | slower pages meanwhile |
| PostgreSQL | Patroni with `synchronous_mode_strict` and a synchronous standby in another zone; WAL archive and daily base backups with pgBackRest or WAL-G; 14 days of point-in-time recovery | 0 on node or zone loss; under a minute on cluster loss | about 30 s failover; about 2 h to restore 4 TB |
| SQLite | already WAL with `synchronous=FULL`; `codeyard backup` copies it online with `VACUUM INTO`; Litestream optional | 0 on a crash; the last backup on disk loss | minutes |
| CI logs | compressed chunks in object storage, metadata in SQL; a job completes only after its last chunk is stored | the unstored tail of a running job | none |
| Artifacts, caches, uploads | Two checked storage-node copies, or optional versioned S3; local disk on one machine | last completed cluster backup, or the operator's S3 recovery point | restore time |
| Node keys, membership | membership is in the database; a lost node pairs again with a new key; `SECRETS_MASTER_KEY` and `CLUSTER_*` are kept offline, apart from the backups | none | minutes |

**Ref quorum.** Initialization selects up to three fixed storage voters (at least two)
and PostgreSQL decides which ref changes commit. A majority must prepare durable objects
and publish refs before acknowledgement. Canonical refs and term fencing recover a committed change after
a primary dies; an uncertain outcome tells the client to fetch before retrying. Further
replicas remain asynchronous. `majority` is the new-cluster default; explicit `1` retains
the legacy asynchronous path. Initialization, copy and promotion share this coordinator.
See [Quorum push preparation](CLUSTER.md#quorum-push-preparation) for limits and recovery.
Three-voter preparation and stale-voter recovery pass on SQLite and PostgreSQL.
The real-process quorum round passes HTTP/SSH pushes, editor saves, gh branch creation
and merges after immediate primary loss, and majority-loss refusal. The VM hard-reset
check remains open.

**Backup and restore.** `codeyard backup <dir>` runs while the installation serves: the
database first (`VACUUM INTO`, `pg_dump`, or only the restore time when pgBackRest or WAL-G
runs), then every repository by incremental `git fetch --mirror` including the hidden
`refs/codeyard/*`, then local uploads. The rule is that **the database may be older than
Git, never newer**: rows point at commits (pull request heads, merges, CI runs, releases),
and extra commits are harmless.

`codeyard restore <dir>` loads all three into an empty installation, then runs
`codeyard repo verify --repair` and `codeyard repo rebuild`. Verify runs
`git fsck --connectivity-only` and compares each pull request with `refs/pull/N/head`:
an open pull request follows a ref that moved, a missing ref is made again, refs of pull
requests the database lacks are removed, and a commit the database names but Git lacks is
reported. Cluster restore instead installs each original storage identity while startup
is fenced, reconstructs canonical voter terms through offline `repo verify --repair`,
and permits serving only after the required Git and object copies are ready. Region
failover from a database newer than Git remains future work.

Deleted repositories move to `<REPOS_PATH>/deleted/`, and uploads keep their pending
deletion, for 7 days (`RESTORE_WINDOW_SECONDS`), after which the expiry sweep removes them.
A database restored to before a delete gets the repository back from `deleted/` through
verify. GitHub keeps deleted repositories for 90 days.

**Stage 0 acceptance**, on SQLite and PostgreSQL where it applies:

1. Three storage nodes: `git push` over HTTP and over SSH, `kill -9` of the primary as soon
   as the push returns; within 2 minutes `git clone` has the commit, without `--force`.
2. The same for an editor save, `gh pr merge` and a branch created with `gh api`.
3. Two of three voting members stopped: `git push` is refused with the reason and no
   member's refs change. One stopped, or `write_quorum=1`: the push succeeds.
4. One machine in a VM, hard reset (`echo b > /proc/sysrq-trigger`) right after an
   acknowledged push: after boot `git fsck` is clean and the commit is there.
5. `codeyard backup` during a loop of `git push` and `gh issue create`, then
   `codeyard restore` on an empty machine: verify reports nothing, and `git clone`,
   `gh issue list`, `gh pr checkout`, `gh run view --log` and attachments work.
6. The PostgreSQL primary is killed during a loop of `gh issue create`; every issue whose
   URL was returned still exists.
7. PostgreSQL restored to a time T with Git newer than T, then
   `codeyard repo verify --repair`: every pull request page and `gh pr checkout` work.
8. `gh repo delete`, then the database restored to before it: the repository clones.
9. Derived data deleted, then `codeyard repo rebuild`: pages, code search and clones work.

### Cells and placement

**The owner account, a user or an org, is the unit of sharding.** Its repositories, issues,
pull requests, packages, CI and settings have one home cell for writes. Organizations
span regions through read replicas and Git copies; their rows stay together in that cell. Why:
URLs, permissions and nearly all 4,728 query sites are scoped by owner and repository, so a
cell runs today's code unchanged, as in GitLab Cells. Cost: anything between owners in two
cells crosses cells: forks, pull requests from forks, stars, follows, outside members.

A cell is a complete installation as in section 2, sharing nothing with other cells; an
existing installation becomes cell 0 with no data change. A new account goes to the least
full open cell in the nearest region, or to the org's cell when it joins through an
invitation or SSO. A cell closes to new accounts at 2 TB or half its measured write
capacity; enterprises get their own cell.

- **An org with 100,000 employees** is 100,000 membership rows in one cell, with stubs
  (below) for members from other cells; per-org locks keep it from blocking other orgs.
- **A repository with millions of stars** keeps the star rows, with a stub of about 200
  bytes per remote stargazer; the stargazer's home cell keeps a pointer.
- **A repository with 100,000 forks**: each fork lives in its owner's cell, and the forks in
  one cell share an object pool through alternates, as on GitHub: at most one full copy per
  cell, about 1,000, not 100,000.
- **Hot cells and accounts** show in per-cell and per-account metrics. Placement closes a
  hot cell; a hot account gets bigger machines until moves exist (stage 6), then its own
  cell.

### Git storage

**Git stays on local NVMe in each cell: three voting copies across zones, more
asynchronous copies for reads, and one in the disaster-recovery region.** Why: GitHub and
GitLab run this model, and it keeps a cell one failure domain. Cost: four physical copies,
which makes storage the second largest cost.

- Clones and fetches, about 90% of git traffic, go to any current copy and use bitmaps,
  commit-graphs and a pack-objects cache, as on gitlab.com. CDN bundles are left out:
  clients ship with `transfer.bundleURI` off.
- Objects cross cells only by fetch: a fork, a pull request head, a transfer.
- `git gc --auto` never prunes (`gc.pruneExpire=never`, as today); object pools repack in
  the background and keep every object a member uses.
- Limits: 100 MB per file and 2 GB per push, as on GitHub, and a warning at 10 GB per
  repository; large files go to LFS in object storage.
- Git on object storage, which GitHub is testing in 2026, waits until storage dominates
  the bill.

### The directory

**One global directory in sharded PostgreSQL maps names to cells and keeps them unique.**
It holds user and org names (compared exactly, as the application does), rename redirects,
emails, SSH key fingerprints including deploy keys, the cell registry (id prefixes,
addresses, node keys, open or closed) and forwarding entries for moved accounts and
repositories. Why: `/alice/repo`, a login by email and an SSH key carry no id to route by.
Cost: one more global system, on the path of every signup and key change.

- About 400 bytes per account, 4 TB at 10 billion, in 256 logical shards by hash of the key
  on 16 clusters at first. Names, emails and fingerprints are separate keys, so a claim
  touches one shard.
- A claim inserts a pending row with an expiry under a unique key; the cell creates the
  account and confirms, and a failed signup expires. No two-phase commit.
- Each region has asynchronous read replicas, and routers cache routes for 60 seconds; a
  stale route reaches the wrong cell, which names the right one.
- With a shard down, existing users keep working from caches and replicas, while signups,
  renames and new emails or keys on that shard fail. The cells hold the same rows, so the
  directory can be rebuilt from them.

### The router

**A stateless `router` role in the same binary runs in every region** and proxies HTTP,
both APIs, smart HTTP and SSH to a cell's web and SSH nodes. Why: URLs, clone URLs and
tokens stay the same wherever an account lives, as behind GitHub's front ends. Cost: one
more hop, about 1 ms in a region, and about 300 routers for 5 Tbps of git.

- It routes by the owner in the path (`/{owner}/…`, `/repos/{owner}/{repo}`,
  `/users/{name}`, `/orgs/{org}`), by id prefix for bare-id routes and node ids, by the
  credential's cell for routes without an owner (`/user`, `/notifications`), by the login
  field for `POST /session`, and by placement for signup.
- GraphQL: each top-level field routes on its own arguments; a query reaching two cells is
  split and merged. Nested fields come from the parent's cell, using stubs.
- SSH: the router terminates SSH with the deployment's host key, finds the key's account in
  the directory and opens the session to the cell named by the exec path; `ssh -T` goes to
  the key's home cell.
- The rules sit next to the application's routes; a test fails when a route has none.

### Ids

**Every id is `prefix × 2^37 + n`, a 16-bit prefix and a 37-bit sequence, below 2^53.** A
new cell sets each sequence's `MINVALUE`, `MAXVALUE` and `START` to its prefix's range;
cell 0 is prefix 0 and keeps today's ids. A cell takes a second prefix when a sequence
passes 80% of its 137 billion values. Rows keep their ids when an account moves, so foreign
keys copy unchanged. Why: unique without coordination and exact in JavaScript clients and
Actions scripts; the draft's `cell << 42` leaves 11 bits, 2,048 cells, fewer than the high
case. Cost: a sequence change per new cell.

Node ids for every kind except repositories, users and orgs move to the msgpack form
`[0, root id, id]`, which GitHub uses and Codeyard already uses for discussion comments, so
a moved repository needs one forwarding entry, not one per comment. The
`{kind}_{base64 id}` form keeps resolving.

### Accounts across cells

**A cell keeps a stub row in `users`, `organizations` or `repositories` for each remote
account or repository it refers to**: global id, name, display name, avatar, `home_cell`,
no credentials (`password_hash` is already nullable), changed only by events from the home
cell. Why: 169 foreign keys point at users and orgs and 121 at repositories; stubs keep
them and nearly every query, and only listings skip stubs, about 20 to 40 of the 144
queries that read `users` plus a few repository lists. Cost: stubs lag by seconds, and a
listing that forgets the filter shows a stub as an account.

- Credentials live in the home cell and carry it: cookies `<prefix>.<random>`, tokens
  `cy_pat_<prefix>_<64 hex>` (the secret scanner accepts both forms), OAuth and device
  codes likewise. A value without a prefix belongs to cell 0.
- A cell given a foreign credential asks the home cell for the account, scopes and CSRF
  token and caches the answer for 60 seconds, so revocation reaches every cell within a
  minute. The router picks the web node by a hash of the credential to keep that cache warm.
- Permission checks stay in the resource's cell, where members, teams and collaborators
  are rows.
- A cross-cell pull request lives in the base repository's cell, which fetches the head
  into `refs/pull/N/head` on each push event from the fork's cell. A maintainer's push to
  the fork goes to the fork's cell, which asks the base cell for permission.
- A mention resolves through the directory and creates a stub. Home cells keep pointers to
  remote memberships for "my orgs" and "my repositories".

### Global features

Each cell writes an outbox table in the same transaction as the change. A worker delivers
events to other cells, at least once and idempotent by event id; global services read the
outbox by cursor. No message bus until the consumers need one.

| Feature | Where it lives | Fed by | Lag |
|---|---|---|---|
| Notifications, email | inbox in the home cell; the resource's cell finds recipients and sends one batch per home cell | outbox delivery | seconds |
| Issue, PR, repository, user search | OpenSearch; documents carry repository and visibility, results filtered to what the reader can see | outbox cursor | under a minute |
| Code search | Zoekt, indexed on push, replacing `git grep` over at most 250 repositories | push events | minutes |
| Activity feed | read from the event index when viewed, no copy per follower | outbox cursor | seconds |
| Explore, trending | hourly batch into a table copied to every region | outbox cursor | an hour |
| Dependency graph, "used by" | each cell parses manifests; a global index answers dependents | outbox cursor | minutes |
| Security advisories | one dataset copied to every cell and matched locally | pull | minutes |
| Stars, follows | rows and count in the target's cell, a pointer in the actor's home cell | outbox delivery | seconds |

### CI

**One job queue per cell, as today, with ephemeral runners shared by a region's cells.** A
regional autoscaler starts a fresh VM per untrusted job, registered to the job's cell with
a reusable token; rootless Podman stays for trusted self-hosted runners, as it is no
sandbox. Logs go to object storage in chunks (stage 1); artifacts and caches live there
with retention (90 and 7 days, as GitHub) and per-repository quotas. Abuse limits are per
account (concurrency, minutes) and per cell (queue caps), beside the existing approval for
workflows from forks. Why: CI is the largest cost and must grow apart from the database.
Cost: an autoscaler and VM images to maintain.

### PostgreSQL inside a cell

**A cell holds at most 4 TB of database, about 10 million accounts and 10,000 write
transactions per second at peak, and closes to new accounts at 2 TB.** Why: a 4 TB restore
takes about 2 hours, which bounds the worst outage; size, not load, sets the cell count.
Cost: four PostgreSQL nodes per 10 million accounts.

- Stage 1 reuses scoped account and org authority, per-package publish locks and
  lock-free registry snapshots, notification cursor polling and configurable pools behind
  PgBouncer. `ci_log_chunks` holds object keys, checksums and bounded ranges; bytes use the
  local, S3 or storage-node object backend. Cleanup is reserved before upload and committed
  pointers cancel it; deletion retains objects for the seven-day restore window.
  `DATABASE_READ_URL` routes issue/PR GET and GraphQL query content to a physical replica only after checking
  database identity and replay against the primary's WAL position. Credentials and writes
  stay on primary, including reads for five seconds after the same credential wrote.
  Missing, behind, promoted or stopped replicas use primary. Other content families remain
  on primary; bulk-table retention and completed load measurements remain open.
- Four nodes: primary, synchronous standby in a second zone, asynchronous read replica in a
  third, and an asynchronous copy in the disaster-recovery region. Patroni with etcd runs
  failover, pgBackRest or WAL-G archives WAL (pgBackRest changed maintainers in 2026), and
  every cell runs a monthly restore drill.
- Schema changes expand, then contract: one binary works with schema N and N+1, and
  releases go out in waves (a canary cell, 1%, 10%, all) that stop at the first failure.

### Account moves

**Moves are built last (stage 6).** Until then an account stays where it was placed and
cells close at half their limit. Every table declares how its rows reach an owner, by an
owner column or through a repository or another row; a test fails when a table has none,
the way `codeyard db copy` already walks the catalog. A move copies rows in the background,
freezes the account for the final catch-up (reads work, writes get a retry message), copies
its repositories, switches the directory and forwarding entries, turns the source rows into
stubs and lifts the freeze. Ids do not change, and the source copy stays until the move is
checked. Why: rebalancing matters only once cells fill unevenly, and logical replication
cannot do it, since row filters see only a table's own columns. Cost: ownership rules for
all 278 tables, kept complete for good, and a freeze of minutes for a normal account, hours
for a very large org. Splitting an account by repository is rejected, since its teams and
permissions would span cells; a single repository transferred to another cell uses the
same rules (stage 3).

Automatic region moves apply only to individual user accounts. Routers count requests
per authenticated account, source region and day, including its API and CI tokens and SSH
keys. Anonymous requests and outside readers of public repositories never count. Defaults
in site settings: a seven-day window, at least 1,000 counted requests, at least 70% from
one other region, held on three consecutive days. The hourly evaluator moves the account
to the least full open cell in that region using the move above. It reuses scaling-policy
database leases, dry run and pause: one leader, a 30-day account cooldown, one move per
cell, a daily cap per region pair, no moves for pinned accounts, and no evaluation with
only one region. Every move is audited; dry run only logs and audits "would move". All
thresholds and controls are editable in site settings.

Organizations never move automatically. They gain presence regions where at least 15% of
their members' counted requests originate over seven days, with at least 500 requests and
the condition held for three days. Counting follows the same authenticated-actor rule.
Admins can edit thresholds and add or remove presence by hand. Remove presence when its
share remains below 5% for 14 days. An org still has one home cell for every write, with
no multi-master, two-phase commit or splitting its rows across cells.

In a presence region, the home cell has an asynchronous database read replica shared by
all its organizations; provision one when any organization first needs it. The org's
repositories have non-voting Git copies there, using the existing copier, rebalancer and
publication path. Routers send GETs and fetch/clone to the nearest current replica or Git
copy, fall back to home when unavailable, and send all writes and reads for a few seconds
after the same session wrote to home. A behind Git copy never serves clones. Regional
presence adds one full copy of the cell database per presence region, plus one full Git
copy of each present org's repositories per storage copy, indexes and WAL included in
capacity planning. Only reads become local: distant writes still pay the round trip to
the home region.

### Failures

| Failure | What users see | RPO | RTO |
|---|---|---|---|
| Web, SSH or router node | a retried request | 0 | seconds |
| Storage node or zone | writes to its repositories wait for take-over; reads use copies | 0 | 1 to 2 minutes |
| A cell's PostgreSQL primary | writes fail during failover | 0 | about 30 seconds |
| A whole cell (bad migration, corruption) | its accounts, about 0.1% of users, see an error page; other cells show its stubs as last seen | the recovery point chosen | about 2 hours |
| A region | its cells restart from their copies in the paired region | seconds | about 30 minutes, an operator's call |
| A directory shard | signups, renames, new emails and keys on that shard fail | 0 | about 30 seconds |
| Search or another index | those pages show an error | 0, the outbox keeps events | until it catches up |

### Regions

**Each cell lives in one region across three zones, with an asynchronous copy of its
database and Git in a paired region.** Routers, directory replicas and runners run in every
region; anonymous public pages and release downloads go through a CDN. A request inside a
region costs milliseconds; a contributor on another continent pays 100 to 250 ms per
request to the repository's cell. An account can be pinned to a region; residency covers
the data it owns, not its comments in other regions' repositories. China is a separate
deployment with its own directory. Why: synchronous writes across regions would add a long
round trip to every write. Cost: losing a region loses seconds of writes and needs an
operator to fail over.

### Capacity model

Base-case assumptions, none measured: 10 billion accounts, 1 billion active a day, 200
requests per active user a day (5.8 million a second at peak), 5% of them writes, 10 to 20
queries each; 400 KB of database per account (GitHub is nearer 2 MB; the high case uses
3 MB); 2 repositories per account at 5 MB after object pools; 1 billion pushes and 20 PB of
git egress a day; 500 million CI jobs a day at 7 vCPU-minutes each.

| Part | Base case | High case |
|---|---|---|
| Database | 4 PB: about 1,000 cells, 4,000 PostgreSQL nodes | 30 PB: about 7,500 cells, 30,000 nodes |
| Load per cell | about 6,000 requests, 300 writes and 60,000 to 120,000 queries a second | lower |
| Directory | about 4 TB on 16 clusters | the same |
| Git | 100 PB, 400 PB on disk with four copies: about 7,000 storage nodes of 60 TB | the same |
| Bandwidth | about 5 Tbps of git at peak, plus pages | the same |
| Web and SSH; routers | about 3,000; about 300 | the same |
| CI | about 3.5 million vCPU at peak | the same |

CI dominates the cost, then Git storage and bandwidth. The database is the smallest line
but the hardest to run: a cell's writes are a few percent of what one primary takes, so data
size and restore time set the cell count.

### Stages

A single machine stays one cell after every stage, with no router or directory, and SQLite
stays allowed; an SQLite installation joins a multi-cell deployment after `codeyard db copy`
to PostgreSQL. Every stage keeps `scripts/e2e.sh` passing; each list is checked with real
`gh` and `git`.

| Stage | Builds | Touches old code |
|---|---|---|
| 0 | durability (above) | moderately: Git settings, hooks, `begin_ref_change`, replicas, new commands |
| 1 | one big cell | **widely**: lock calls in 44 database modules, every log path, notifications, read routing |
| 2 | two cells: router, directory, id ranges, credential prefixes, stubs | **widely**: credential formats, listing queries, a rule per route |
| 3 | cross-cell forks, pull requests, notifications, transfers; object pools | moderately |
| 4 | global indexes | moderately |
| 5 | regions, disaster recovery, waves, directory shards, CDN | little |
| 6 | account moves | **widely**: an ownership rule for every table |

Stage 2 is the smallest multi-cell deployment that works end to end; until stage 3 a fork
across cells is refused with a clear message.

**Stage 1.** (1) Invitations in 50 orgs run beside 500 clients reading issues, and no
request waits on another org's lock. (2) Registry reads take no lock; publishes to
different packages run in parallel. (3) No `NOTIFY` or `LISTEN` remains, and a notification
reaches an open browser within 2 seconds of `gh issue comment`. (4) `ci_logs` is gone,
`gh run view --log` shows the whole log, and a killed runner leaves a failed job with its
stored log. (5) With a replica, `gh issue view` right after `gh issue create` shows the
issue. (6) Clones come from current copies, never from one that is behind. (7)
`scripts/e2e.sh` passes behind PgBouncer. (8) A benchmark on stated hardware records one
cell's write rate, read rate and clone throughput, and the capacity model is updated.

**Stage 2.** (1) An existing installation becomes cell 0 with no data change, and
`scripts/e2e.sh` passes through the router. (2) A cell-1 user runs `gh auth login` and
`gh repo create`, and pushes and clones over HTTP and SSH. (3) Added with `gh api` as a
collaborator on a cell-0 repository, that user pushes there over SSH and runs
`gh pr create` and `gh pr merge`. (4) A signup with a name taken in the other cell is
refused. (5) Cell-1 ids are at least 2^37 and below 2^53, and `gh` and JavaScript read them
exactly. (6) `node(id:)` resolves objects in both cells, and one GraphQL query with
`repository` fields in both returns both. (7) A revoked cell-1 token stops working on
cell-0 repositories within 60 seconds. (8) The test suite passes with stub rows present,
and search, member lists and counts never show a stub as an account. (9) With cell 1
stopped, cell-0 users keep working and cell-1 pages show a clear error.

**Stage 3.** (1) `gh repo fork` across cells, a push to the fork, `gh pr create` upstream:
the pull request follows each push, and `gh pr checkout` and `gh pr merge` work. (2) The
upstream maintainer pushes to the fork's branch. (3) 100 forks in one cell share one pool,
and disk use grows only by new objects. (4) A mention of a user in the other cell reaches
`gh api notifications` within a minute, with one email. (5) A star across cells updates the
count and the user's star list. (6) A repository transferred to the other cell keeps its
issue and pull request numbers, and the old URL redirects. (7) A cell killed during
cross-cell traffic delivers its outbox after restart, without duplicates.

**Stage 4.** (1) `gh search repos`, `issues`, `prs` and `code` return results from both
cells, private ones only to readers, within a minute of a change. (2) An index rebuilt from
nothing returns the same results. (3) Explore, trending and "used by" cover both cells, and
a new advisory raises alerts in both. (4) With search stopped, pages work and search shows
an error.

**Stage 5.** (1) Signups go to their region's cells, and pinned accounts keep their home
region; org presence can span regions.
(2) With a region stopped, its cells serve from the paired region within 30 minutes, and
`codeyard repo verify --repair` lists what it fixed. (3) A schema change goes out in waves,
a failing canary stops it, and old and new binaries run side by side. (4) A directory shard
moves between clusters with no failed lookup. (5) With two regions, an org whose members
work from B gains presence there: clone and `gh issue list` use B's current copies and
match home, a push reaches home and the following B read shows it, a stopped replica
falls back to home, and presence is removed after the share drops.

**Stage 6.** (1) A test fails when a table has no ownership rule. (2) A user with
repositories, issues, pull requests, CI history and packages moves cells, and `git clone`,
`gh repo view`, `gh issue list`, `gh run list`, old REST and node ids and links from other
cells keep working. (3) An org with 1,000 members moves with every member's access
unchanged. (4) During the freeze writes get a retry message, and a move stopped halfway
leaves the source serving as before. (5) With two regions, self-authenticated counts from
another region crossing the threshold on three days move a user and preserve clone,
`gh` and links. Anonymous reads never trigger a move; pinned and cooled-down accounts
never move, and dry run changes no placement. Lease contention, pause, the daily region
pair cap and one-move-per-cell guard hold. (6) Organization presence meets stage 5's
two-region acceptance without moving its home cell or splitting its rows.

### Not built, rejected, and the biggest risk

Not built at all: a new database engine; multi-master writes; synchronous writes across
regions; two-phase commit across cells; splitting an org; GraphQL federation below
top-level fields; a consensus protocol for refs; our own CDN or object store; exact global
rate limits, which stay per cell and per router. Git on object storage and a message bus
wait until storage cost or the number of consumers calls for them.

Rejected:

- Sharding by repository or table (Vitess at GitHub and Slack): a query router under all
  4,728 query sites, and no joins across owners.
- Distributed SQL (CockroachDB, YugabyteDB, Spanner) or Citus: consensus or a coordinator on
  every write, one large failure domain, and gaps in PostgreSQL features Codeyard uses
  (`LISTEN`/`NOTIFY` is early access in YugabyteDB); CockroachDB is no longer open source.
  Rejected on design, not on a benchmark.
- One primary with many replicas, as OpenAI runs for 800 million users: 4 PB fits neither
  one database nor one restore.
- Resolving remote users at read time: drops 169 foreign keys and touches hundreds of
  queries.
- Forks in the upstream's cell, as GitLab Cells plans: the forker's org permissions would
  cross cells on every access.
- A host name or redirect per cell: URLs would change on moves, and Git and `gh` handle
  redirected pushes badly.
- Search by asking every cell (the draft): a thousand requests per query.
- Time-based ids (Snowflake) pass 2^53; `cell << 42` allows only 2,048 cells.
- Logical replication for moves: row filters see only a table's own columns.

The biggest risk is that cross-cell work is the normal case. With 1,000 cells, a fork, a
star or an outside contributor is in another cell 999 times in 1,000, so stubs, the outbox
and foreign credential checks carry most open-source traffic; if they lag or break, open
source on Codeyard breaks. So from stage 2 every end-to-end test spreads its users over two
cells. The next risks are a listing that forgets to skip stubs, and running thousands of
PostgreSQL clusters, which stays possible only if the waves, drills and placement of stage
5 run by themselves.
