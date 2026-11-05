# Running a cluster

A single machine needs none of this. An installation starts in single-node mode: one
`server` runs the web pages, Git storage and SSH, and `runner` machines may join to run
Actions jobs ([CI local runner](CI_LOCAL_RUNNER.md)). Use cluster mode when you want web
and SSH servers separate from the machines that hold the repositories, or several
repository disks. [Architecture](ARCHITECTURE.md) section 2 explains how it works.

`cluster enable` and `serve --cluster` require PostgreSQL. A single-node installation
keeps SQLite support. The [readiness plan](CLUSTER_PLAN.md) records outstanding runtime
acceptance and measured limits.

## Turn on cluster mode

On a machine with the installation's `DATABASE_URL` (PostgreSQL):

```bash
codeyard cluster enable
```

`codeyard serve --cluster --roles web,ssh,storage` does the same first, so the first node
needs one command. Nodes that join later do not need the flag.

The mode is stored in the database, so every node sees it, and the **Administration →
Cluster nodes** page shows it. Until it is on, `serve --roles` accepts only `server` and
`runner`, `cluster invite` accepts only `--roles runner`, and a node that joined with
another role is refused on its next request. The cluster variables below (`IROH_BIND`
and the rest) open the node connection in either mode; they do not change the mode.

## What you need

- **PostgreSQL** shared by every node that serves users or repositories. SQLite is for
  one machine and the server refuses to run a cluster on it.
- **The same `SECRETS_MASTER_KEY` on every core node** (`web`, `ssh`, `storage`):
  signed push webhooks are sent on storage nodes too. Core startup requires a valid
  64-hex-character key and checks it against an existing signed hook when one exists.
  Database-free workers do not need it.
- **Two active storage nodes with free disk space** for quorum Git writes. Shared
  S3-compatible storage (`S3_*`, see the [README](../README.md)) is optional for files.
  Without S3, uploads, avatars, release assets,
  packages, Actions artifacts and caches use checked, durable copies on two storage nodes.
  Their immutable address includes the declared SHA256. A web node keeps only staging/cache
  bytes; reads can use either storage copy. Every core must select the same backend.
  Single-node mode still defaults to local disk. Both database engines pass the
  storage-node HTTP acceptance checks.
- **Public clone URLs on every core node.** Set `PUBLIC_HTTP_CLONE_BASE` and
  `PUBLIC_SSH_CLONE_BASE` to the public web and SSH frontends, including on storage
  nodes. Signed webhook payloads use these addresses even when that node does not
  serve public SSH itself.
- **UDP or TCP between the nodes.** Nodes talk over QUIC on `IROH_BIND` (default
  `0.0.0.0:7444`). Direct connections are the default and there is no relay unless you
  set `IROH_RELAY`. Behind NAT or a container port mapping, tell the other nodes where to
  dial with `IROH_ADVERTISE=host:port`. If UDP cannot reach the nodes, use one TCP port
  instead; see [Reachable only over TCP](#reachable-only-over-tcp).

## Quorum push preparation

New clusters default to `majority`. Initialization selects up to three fixed storage
voters, requiring at least two available nodes; three voters tolerate one stopped voter. Choose
`codeyard cluster enable --write-quorum 1` for the legacy asynchronous policy. Enabling
a new cluster mode requires every core to be stopped; existing repositories without
quorum preparation require that explicit legacy choice. The choice is kept on repeated
enable commands. Start two storage nodes before creating repositories in majority mode. Preparation uses the
stored `write_timeout_seconds` deadline (default 10 seconds). On each storage node,
`WRITE_QUORUM_MAX_PACK_BYTES` limits a prepared pack (default 128 MiB, maximum 1 GiB).
Keep `codeyard-writes/` inside each repository with its objects: it retains undecided
and committed-but-unpublished writes for recovery. A refused preparation reports
"write quorum unavailable". An unknown outcome means fetch and check refs before retrying;
it does not mean the write was rolled back. Clusters without `write_quorum` keep the
existing push path.

Empty repositories initialize a canonical HEAD on both voters before serving starts.
Fork and template objects are staged privately; their refs publish through the same
quorum coordinator as pushes, after live source and destination authority checks.
A missing voter refuses creation. Initialization, copy and voter-replacement acceptance
checks pass on both database engines.

## Monitoring

`/health` is liveness; `/ready` checks the database with a 2-second deadline. Nodes with
`storage` also probe repository-root writes and available space with a 2-second deadline.
`STORAGE_MIN_FREE_BYTES` sets the reserve (unsigned bytes, default 1 GiB). Failed checks
return 503 with the check name, never a path. Storage nodes without `web` expose only
these probes and `/metrics` on `HTTP_ADDR`; keep that listener on a private network.

`/metrics` returns Prometheus text to a site administrator's personal token or a bearer
matching `METRICS_TOKEN` (1–4096 bearer-token characters). Other callers receive 404;
browser sessions and repository-scoped tokens do not grant access. Scrapes have a
2-second collection deadline and support at most 1,000 storage nodes.

- `codeyard_repository_bytes{node_id}`: recorded repository sizes, including initialized
  replicas using their primary's recorded size; not a filesystem usage scan.
- `codeyard_storage_free_bytes{node_id}`: last reported available disk bytes (`NaN` until
  reported). Alert below the storage reserve and on missing reports.
- `codeyard_replica_lag_versions`: largest write-version gap, including pending copies.
  Alert when nonzero for several minutes.
- `codeyard_ci_queued_runs`, `codeyard_ci_oldest_queued_seconds`: queue depth and oldest
  age (zero for an empty queue). Alert when age exceeds 120 seconds.
- `codeyard_database_pool_size`: open connections in this process, including idle ones.
  `codeyard_database_pool_waiters` counts acquisitions that have yielded while waiting
  for a connection, including connection creation and health checks. Cancellation and
  timeouts release the count. It is local to the scraping process.
- `codeyard_quorum_failures_total`: refused or aborted quorum attempts in this process.
  Alert on any increase.
- `codeyard_drain_total{node_id,state}` and
  `codeyard_drain_completed{node_id,state}`: reserved and completed drain
  work. Alert on stalled progress.
- `codeyard_nodes{state}`: counts of pending, active and revoked memberships, not
  heartbeat health. Alert on unexpected membership changes and scrape failures.

## Roles

`codeyard serve --roles <list>` (or `GITSERV_NODE_ROLES`). In cluster mode `server` splits
into `web`, `storage` and `ssh`, in any combination:

| Role | What the node does | Needs |
| --- | --- | --- |
| `server` | `web`, `storage` and `ssh` together, the single-node default | `DATABASE_URL` |
| `web` | pages, API, Git over HTTP | `DATABASE_URL` |
| `ssh` | Git over SSH (built-in server for remote repositories) | `DATABASE_URL` |
| `storage` | holds bare repositories on its disk and answers the other nodes | `DATABASE_URL`, a disk, cluster settings |
| `runner` | runs Actions jobs, in either mode | `act`; with this role alone, no database |

A node with neither `web` nor `ssh` only serves other nodes, so it must have joined a
cluster first.

## Start the first node

With PostgreSQL, stop the current server before enabling the cluster. A new installation
uses majority; an existing installation with repositories first runs
`codeyard cluster enable --write-quorum 1` to retain their legacy write path. Then start
the first node:

```bash
DATABASE_URL=postgres://... NODE_NAME=core IROH_BIND=0.0.0.0:7444 \
  codeyard serve --roles web,ssh,storage
```

Run `codeyard cluster enable` before this. Setting `IROH_BIND` (or any other cluster
variable) opens the node connection. The node registers itself in the `nodes` table and
keeps its key in the state directory.

An installation that ran on SQLite moves its database first, with its server stopped.
`db copy` refuses a source whose migration history differs from the destination's, so bring
the file up to date with this build first. (This build replaced the baseline, so a file made by
0.1.5 or earlier cannot be brought up to date.) `init` upgrades the file in place, so keep a copy of it:

```bash
DATABASE_URL=sqlite://./data/gitserv.db codeyard init
```

Then run the copy:

```bash
COPY_FROM_DATABASE_URL=sqlite://./data/gitserv.db \
COPY_TO_DATABASE_URL=postgres://user:password@host:5432/name \
  codeyard db copy
```

The copy reads the source in one snapshot and writes the destination in one transaction.
The destination is migrated first and must be empty; the default file limits that the
migrations insert are replaced by the source's. The database role that runs the copy must
own the destination's tables, which the role that ran the migrations does; superuser is
not needed. The copy commits only when every table's row count matches, so a failed copy
leaves the destination empty. Id counters carry over too, so an id deleted in the source
is not issued again. After copying SQLite to PostgreSQL, start the node as above with
`DATABASE_URL` set, and run `codeyard cluster adopt` on it once. That gives it the
repositories whose directories are on its disk; without it only this node can open them.

The same command copies PostgreSQL back to SQLite, but refuses while any repository has
cluster placement (`repositories.storage_node_id` is not NULL). With all nodes stopped,
first consolidate the current repository directories on the standalone machine at their
stored `disk_path` locations, then remove placement by setting `storage_node_id` to NULL.
While all nodes remain stopped, also run `UPDATE node_configuration SET cluster_enabled = false;`
on PostgreSQL after consolidating storage and clearing placement, before copying. The copy
preserves this flag; SQLite cannot start with cluster mode enabled.
Copy only after that preparation; the command does not move storage or clear placement.
SQLite cannot serve clustered repositories, even if their directories are present locally.
Start the SQLite installation without cluster settings.

`db copy` does not copy repository directories, uploaded files (`objects` in the state
directory, or S3) or key files such as the SSH host key and the node key. Move those
separately.

## Add nodes

Pick one of three ways per node. All of them end with the node's key listed in `nodes`
with the roles you allowed.

1. **A ticket**, for one machine. On a running core node:

   ```bash
   codeyard cluster invite --roles storage          # prints a ticket, valid for 1 hour
   ```

   On the new machine, put the ticket in the environment rather than argv, then join:

   ```bash
   read -rsp 'Join ticket: ' CLUSTER_JOIN_TOKEN; echo
   export CLUSTER_JOIN_TOKEN
   codeyard cluster join --name store1
   unset CLUSTER_JOIN_TOKEN
   ```

   Then start `serve` with `IROH_BIND` set (any cluster variable opens the node connection).
   Add `--approve` to the invite to keep joiners `pending` until
   `codeyard cluster approve <name>`.
2. **A join token**, for machines you create and destroy. `cluster invite --reusable
   --roles runner --expires 30d` prints a token; give it to every such machine as
   `CLUSTER_JOIN_TOKEN`. Each machine pairs itself on first start and keeps its key.
3. **Declared keys**, for configuration managed in Git. `codeyard cluster keygen` prints
   a secret key and its public key. Give the secret to the node (`NODE_KEY` or
   `NODE_KEY_FILE`) and list the public key on the core nodes:
   `CLUSTER_NODES=<publickey>:storage:store1,<publickey>:web+ssh:web2`.

Manage nodes with `codeyard cluster list`, `approve <name>`, `revoke <name>`, `invites` and
`cancel-invite <id>`, or on the **Administration → Cluster nodes** page. A revoked node is
refused on its next request.

## Files on storage nodes

Opening the node connection for remote runners keeps a single-node installation
on its local object backend. Only persisted cluster mode selects storage-node copies.

Without `S3_*`, cluster files use the ordinary authenticated upload and download URLs.
The web node checks file policy and user credentials; runner keys grant no arbitrary
object reads. Storage peers check live membership again before recording durable bytes.
A failed upload stays pending and can be retried. Drain reserves file and Git replacement
space together, then retires old file bytes after two active replacements are ready.
Immutable-key tombstones remain through the restore window to refuse late uploads.
Objects are bounded to 100 MiB; each store and node listener admits two transfers at once.
The object backend passes HTTP checks for capacity, access, loss, retry and drain on
both database engines.

## Drain a node

Use **Administration → Cluster nodes → Drain**, `POST /api/admin/nodes/:name/drain`
with a site administrator's write token, or `codeyard cluster drain NAME`. Cancel with
`DELETE` on that endpoint, the page's **Cancel drain**, or `cluster drain NAME --cancel`.
All three use the same operation. Actions are audited, including operator commands.

Admission reserves complete fork families on active storage nodes, leaving 15% of each
reported disk capacity free. A 409 response includes `missing_bytes` and changes nothing.
Draining nodes receive no new repositories or CI claims. Web readiness becomes 503 and
new web and SSH requests ask the client to retry elsewhere. Existing storage operations
continue until copy-then-promote fences the family. An unavailable copy leaves the node
`draining`; the page shows progress and the repository waiting for a replacement.
Storage with write quorum preserves its fixed voter count on active replacements. An offline node uses
`cluster promote --all-from` instead.

Each node resumes its drain during heartbeats. Core nodes also finish idle runner drains;
running jobs retain their normal timeout, and the drain cancels remaining jobs after
`CI_RUN_TIMEOUT_SECONDS`. `CLUSTER_DRAIN_MAX_REPOSITORIES` bounds a preflight (default
10,000, maximum 1,000,000); node lists support at most 1,000 nodes. REST drain, authority,
promotion, metrics and native/hydrated browser checks pass. Finished-CI drain
acceptance still needs a writable delegated cgroup.

## Provisioning and automatic scaling

The nodes page and `POST /api/admin/nodes` accept one role (`web`, `ssh`, `storage` or
`runner`) and a count from 1 to 1,000. They return a private one-hour join ticket with
that maximum use count. A ticket issued by an administrator stops admitting machines
if that account is removed or demoted. A request cannot supply a command.

`NODE_PROVISION_COMMAND`, set only in the server environment, optionally starts machines.
It receives `CODEYARD_NODE_ROLE`, `CODEYARD_NODE_COUNT` and `CLUSTER_JOIN_TOKEN` in its
environment. Its output is bounded and redacted; `NODE_PROVISION_TIMEOUT_SECONDS`
(default 300, maximum 3,600) bounds it and its child processes. A failed or timed-out
command may have created machines already; the response preserves the ticket and
reports `failed` or `unknown`. Check the provider before retrying.

Site settings hold one scaling policy per role. All start paused and in dry-run mode,
with minimum 0, maximum 10, step 1 and a five-minute cooldown. One database lease leader
evaluates every minute, using database time. Storage adds nodes after aggregate reported
free space stays below 20% for ten minutes. Runners add nodes when a queued job is older
than two minutes, and runner-only nodes may drain after fifteen idle minutes. Storage
never drains automatically. Web and SSH policies enforce their minimum only.
Outstanding bounded invitations count toward the maximum. Dry runs record decisions
without adding or draining; pausing prevents new decisions. Provisioning needs the
operator command to create machines. Operators or their provider automation stop and
revoke drained machines; drained memberships still count toward the maximum.
Provisioning and policy admission checks pass on both database engines, including
lease leadership, pause, dry run and the storage scale-down prohibition.

## Rebalancing

The mover selects repositories on disks over 80% full and destinations below 60%,
smallest first and then oldest last-push time. Fork families move together. Capacity
reservations and unique node slots allow one move per participating node. It uses the
existing copier and promoter, waits for current copies, and then retires the source.
Quorum moves replace voters without reducing their count and briefly fence writes while
both new terms reconstruct. Busy readers leave retirement pending for another pass.

`REPLICA_BANDWIDTH_BYTES_PER_SECOND` limits all replica streams in one process (default
50 MiB/s, range 1 KiB/s–1 GiB/s). `replicas=all` disables rebalancing because every disk
must retain every repository. Reservations, bandwidth limits and busy-reader retirement
retry pass acceptance checks.

## Storage nodes

```bash
DATABASE_URL=postgres://... NODE_NAME=store1 REPOS_PATH=/srv/repos IROH_BIND=0.0.0.0:7444 \
  codeyard serve --roles storage
```

New repositories (created, imported or forked) go to the online storage node with the most
free disk space; a fork goes next to its source. A node is online while it reports in
(every 15 seconds; offline after 60, or at once on a clean shutdown). The web and SSH nodes read and write the repository
through the storage node, so every page, API call, clone, fetch and push works from any of
them. Only nodes with `web` or `ssh` may send arbitrary repository operations. A
storage-only node may call another storage node only to fetch a repository's packs for a
copy (see [Replicas](#replicas)).

- Pushes and clones through the cluster use HTTPS (with an access token) or the built-in
  SSH server. Host OpenSSH mode serves only repositories stored on its own machine and
  refuses the others with a message.
- Clone, fetch and push stream through to Git on the storage node, with no pack-size
  limit. Each process allows `GIT_SESSION_LIMIT` concurrent sessions (default four per CPU
  core, at least 16), separately from buffered
  repository calls, with five-minute idle and one-hour total timeouts. HTTP
  upload-pack requests (not packs) are capped at 16 MiB.
- Nodes drop a peer that has been silent for 15 seconds, so a crashed storage node
  ends the clones and pushes that were using it within about that time. Over TCP that
  holds for an idle connection; a transfer in flight can take longer (see
  [Reachable only over TCP](#reachable-only-over-tcp)).
- If a storage node is down, reads of its repositories are served by a copy on another
  storage node when one has copied them (see [Replicas](#replicas)). Writes to them, and
  reads of repositories without a copy, answer 503 (the API and Git say
  `storage node unavailable`; pages say the data is temporarily unavailable). Everything
  else keeps working. After a minute offline, a node's repositories move to a current copy
  by themselves when every member of their fork family has one (see [Replicas](#replicas)).
  Back up PostgreSQL either way.

## Replicas

A repository lives on its primary, the storage node named in its row. Replicas keep
copies of it on other storage nodes, current within seconds. The replica setting says
how many copies each repository keeps:

```bash
codeyard cluster replicas              # the setting, and each node's copies
codeyard cluster replicas set 2        # 1 (the default, no copies), a count up to 999, or all
```

`2` keeps the primary and one copy, on the storage node with the most free disk.
`all` keeps a copy on every storage node. Nothing needs to be moved by hand: each
primary's node adds or removes copies within a few seconds. For an offline copy it adds
the replacement first, and removes the offline copy only once the replacement has applied
a version.

A copy is a bare repository at `<REPOS_PATH>/mirrors/<uuid>.git`, never at the primary's
location. Every storage node runs a sync loop every 5 seconds. A copy asks its primary what
HEAD and refs (all of them, pull request heads included) it has and which objects the copy
lacks. The primary sends those objects as a Git pack, and the copy stores them and sets
its refs to the primary's in one ref
transaction. Pushes, merges, editor saves and branch and tag changes go only to the primary. Each ref
change on the primary adds one to the repository's write version, and a copy is current
when it has applied that version.

Reads go to the primary. When the primary gives no reply, a read-only request (a page, an
API read, or a clone or fetch over HTTP or SSH) is answered by a copy instead. The copies
are tried in turn: only online storage nodes whose copy has applied the repository's current
write version are eligible. A primary that answers, even with "not found", is never
bypassed, and a Git session is retried only before the node's first reply. The serving node
checks that version again before admitting the read, so pages, clones and fetches never
fall back to a copy that is behind. If no current copy can answer, the read fails rather
than returning older data. After a dial to a node fails, requests for it skip the wait for
five seconds and go straight to the copies. A web node that also holds the copy reads it directly.

`codeyard cluster replicas` shows, for each node, its primaries, its copies, how many of
them are current and how many are behind.

A storage node takes a repository over when it has seen the primary offline for a minute
and holds a copy with every write. A copy that is behind is not taken over: only a person
can accept losing those writes, and `cluster promote --force` does that. A fork family goes
with its repository. A node takes over a family only when every member is on that primary
and it holds a current copy of every member, so a family with a behind or missing copy
waits as a whole. It is checked again on every pass, so it is taken once its copies are
current. The family moves in one transaction, so when several nodes hold such copies, the
first to lock the family takes it and the others find it moved. A node that restarts is back
within that minute and loses nothing. Only an active node that stopped reporting counts; a
revoked node is left to the operator. The node logs a warning for each repository it takes
over, and, once per outage, one for each fork family it skips. A family is skipped when a
copy here is behind, when a member has no copy here, or when its members are not all on the
offline primary; a family that is already split (by `cluster promote --force`, say) waits for
the operator. A crashed node is noticed a minute after its last report, so a take-over follows
a crash after about two minutes and a clean stop after about one. Writes to the repository
fail until then.

A person can promote a copy at any time, for example to move a repository off a node or
to take the best copy of a primary that was lost with writes it never copied:

```bash
codeyard cluster promote owner/repo             # the copy that holds the most writes
codeyard cluster promote owner/repo --to store2 # or a particular copy
codeyard cluster promote --all-from store1      # every repository of a node that is down
```

A promotion needs a copy that holds every write, and is refused otherwise. `--force`
promotes the best copy anyway and reports how many writes are lost, but a copy that was
never made cannot be promoted. A promotion goes by the status each copy last reported, which
is at most a few seconds old: a copy whose directory was removed by hand within that time is
noticed at the next check, so confirm with `cluster replicas` that the copy is healthy before
promoting by hand. The old primary keeps its directory and becomes a copy, which
catches up when its node is back. A node removes a former primary's directory only when its
repository row names another node or is gone, and the node holds no copy of it. Unplaced
repositories and repositories whose primary is the node itself keep their directories.

Promote a repository whose primary is still running while it is idle. The old primary
refuses writes once the promotion is recorded, but a write that was already past that check
can be lost when the old directory is replaced.

A `--force` promotion of a copy that is still being packed after a transfer makes the
repository unavailable on the new primary until packing finishes, which can take up to ten
minutes.

A promotion moves the whole fork family together. The family is every repository linked to
the repository by a chain of fork links, so a source and all of its forks are one family.
Every member with the same primary moves to the copy's node, and
each one needs a copy there, made and (unless `--force`) current. If one cannot move, the
promotion is refused, names the repository that blocks it, and changes nothing. Pull
requests between a fork and its source need both on one node, so this keeps them working.
`--force` accepts a split family: members with no copy on that node stay on the old primary
and can be promoted from their own copies. Pull requests between them and the rest wait until
both are on one node.
Without `--to`, promotion chooses among nodes with eligible copies of the moving family.
The members move one at a time, so a promotion that loses a race part way, including a
concurrently created fork, reports partial progress; run it again to move the rest.

Replication is asynchronous. A push is acknowledged when the primary has it, and copies
follow it. If the primary is lost before a copy has a push, that push is lost. The loss
window is the time since the last sync, about 5 seconds plus the transfer.

### Quorum promotion

The promotion rules above describe asynchronous replication. With
`write_quorum` enabled, `--force` is refused. Promotion fences the whole fork family
in new `terms/<uuid>/<term>/<uuid>.git` directories; retired directories are never reused.
The CLI attempts canonical recovery on its local voter. A pending-recovery error means
ownership changed but serving is still disabled. Retry on each voter with the same
`--to` target. The primary and a voting majority must reconstruct before serving is
enabled. A stale voter refills from a current voter using the same authenticated snapshot
transport as backups; fenced terms only supply snapshots to their assigned voters.
Editor saves, merges, restacks, imports, gist edits, branch/tag updates, pull heads,
provenance and retained Git-data refs use the same quorum publication path. Branch renames
journal the old and new refs, provenance and default HEAD together. Default-branch
settings changes, automatic HEAD selection and imported HEAD changes are journaled.
Empty repositories initialize canonical HEAD on the selected voters before serving.
Fork and template objects are staged privately and publish refs through that same path.
Stage 0 three-voter majority and fenced-recovery tests pass on both database engines.
The real-process quorum round passes HTTP/SSH push, editor, branch and merge survival
after immediate primary SIGKILL, plus majority-loss refusal and recovery.
`E2E_DATABASES=quorum bash scripts/e2e.sh` selects these checks without requiring a
completed CI job. Single-node fsync, backup, restore, verify and rebuild and cluster plan
items 3 and 11 are reused. The [VM hard-reset check](STORAGE_ARCHITECTURE.md#crash-durability-check)
still needs a disposable target.

## Runners

A machine that only runs Actions jobs needs no database and no inbound port, in either
mode:

```bash
CLUSTER_JOIN_TOKEN=... CLUSTER_PEERS=<corekey>@core.example:7444 \
  codeyard serve --roles runner
```

It dials a core node, claims runs, checks the repository out through the storage node
and reports logs and results. Only a core node with the `web` or `ssh` role answers
claims, because it reads each run's workflow files from the storage node. Secrets are decrypted on the core node and sent per run over
the encrypted connection; runs for pull requests from forks get none. `act`, Podman and the
trust rules in [CI local runner](CI_LOCAL_RUNNER.md) apply on the worker as on any node.
Workers without `web` or `ssh` may only fetch checkout packs and read CI commits
and workflow files. The storage node checks each request against a running run
claimed by that worker node for that repository. Queued or finished runs give no
access, and workers cannot write or remove repositories.

## Reachable only over TCP

When UDP cannot reach the nodes, a node can take node connections on one TCP port. Such a
node opens no UDP socket at all:

```
CLUSTER_TCP_BIND=0.0.0.0:7445            # listen for node connections (off by default)
CLUSTER_TCP_ADVERTISE=core.example:7445  # optional: the addresses peers dial
```

- Set `CLUSTER_TCP_BIND` and none of `IROH_BIND`, `IROH_RELAY` or `IROH_ADVERTISE`. The
  node then runs without iroh. Setting an `IROH_*` variable as well keeps the iroh
  listener too.
- Without `CLUSTER_TCP_ADVERTISE` the bind address is advertised, unless it is unspecified
  (`0.0.0.0` or `::`). Then nothing is advertised and the node logs a warning, so set
  `CLUSTER_TCP_ADVERTISE` for it.
- The node reports its TCP addresses in its heartbeat, so tickets from `cluster invite`
  carry them. A node that dials a peer with TCP addresses tries TCP first. iroh is the
  fallback, and only when the peer also has iroh addresses. The TCP addresses share ten
  seconds between them, and iroh gets five more, so one dial takes at most fifteen seconds.
- For a peer that has no iroh address, write `CLUSTER_PEERS=<key>@tcp://host:port`. A
  `<key>@host:port` entry keeps using iroh.
- Open the TCP port instead of UDP in the firewall, between every pair of nodes.
- The shipped `compose.yaml` forwards both variables and publishes TCP 7445 next to the UDP port; set `CLUSTER_TCP_PORT` to change the host port.

What is authenticated: every connection uses TLS 1.3. Both sides present their node key
as a raw ed25519 public key and prove that they hold it. The dialer accepts only the key
it was given for that node, so a node at the same address with another key is refused. A
listener admits only active members on every protocol except pairing, the same rule as
iroh. A peer has five seconds from connecting to finish the handshake and the membership
check. After that it is closed. At most 64 peers may be in either step at once.

What is not: there is no relay and no NAT traversal, so the nodes must reach each other
directly. A TCP connection carries one request and then closes. Each call opens a new
connection, as iroh calls do. The traffic is encrypted, but an open TCP port is reachable
by anyone who can connect to it, so expose it only between the nodes.

Failure detection: every node connection turns on TCP keepalive, on both ends. An idle
connection to a peer that has vanished ends in about fifteen seconds, as it does over
iroh. A transfer in flight is different. TCP keeps retransmitting unacknowledged data
until the kernel's retransmission limit, which can take many minutes, so a clone or push
that was streaming to a dead storage node can hang that long over TCP.

## Backup and restore

Cluster backup runs from a stopped, enrolled web or SSH node, with its usual cluster
settings and key. Other cores keep serving. `codeyard backup DIR` uses `pg_dump` with a
shared snapshot, fetches each placed repository from its primary through the replica
transport, and copies the storage-node object inventory. S3 buckets still use the
operator's versioned bucket backup tools. Keep node private keys and `SECRETS_MASTER_KEY`
in the operator's separate secret backup; the archive contains only its coordinator key.

Restore into a new PostgreSQL database using the same schema name and empty data paths:

1. Keep every new core stopped. Run `codeyard restore DIR` on the coordinator.
2. Give each storage machine its original enrolled `NODE_KEY` or `NODE_KEY_FILE`, the
   restored database URL and an empty `REPOS_PATH`. Run `codeyard restore DIR --node`.
   A failed installation can be repeated against the same owned paths.
3. Run `codeyard repo verify --repair` from the coordinator with its restored repository
   catalogue. It refuses completion until every required voter and two copies of every
   stored object have been installed. Cores refuse startup while restore is pending.
4. Start the storage nodes and then web/SSH nodes. Existing term recovery verifies Git
   before enabling writes. Other web nodes and runners remain drained until the operator
   cancels their drain. Scaling policies remain paused and in dry run after restore.

A coordinator failure after import is resumable with the same archive and coordinator
paths; the pending database records their digest and refuses a different archive or root.
The snapshot pins pull-request and unfinished-CI revisions in the replica stream, so
concurrent branch changes cannot leave the database naming missing commits.

Import and initial fencing share one PostgreSQL transaction. Live addresses, join tickets
and running CI claims are cleared; old job tokens cannot write into the restored cluster.
The installation's serving leases exclude offline restore and verification. Offline
cluster verification uses this restore catalogue, and is refused after the cluster leaves
restore mode. It does not inspect a running cluster through a stale archive.

`BACKUP_MAX_ENTRIES` bounds repositories and stored objects (default 10,000, max 1,000,000).
`BACKUP_MAX_REFERENCES` bounds SQL-snapshot revision pins (default 1,000,000, max 10,000,000).
`BACKUP_COMMAND_TIMEOUT_SECONDS` bounds each database tool (default 3,600, max 86,400).
`BACKUP_MAX_DATABASE_BYTES` bounds restore SQL on disk (default 10 GiB, max 10 TiB).
`REPLICA_MAX_PACK_BYTES` bounds each repository transfer (default 4 GiB, max 1 TiB).
Cluster backup and CLI restore acceptance pass, including interrupted import, restored
Git and object copies, fencing and completion.

## Watching it

- `GET /health` answers while the process is up; `GET /ready` answers 503 while the node
  cannot reach PostgreSQL. Point a load balancer at `/ready`.
- `codeyard cluster list` and the Cluster nodes page show status, last report and version.
  The page also shows reported free disk space; the CLI does not.
- Nodes that cannot be reached are logged with the operation name and never with
  credentials.
- A clean stop (SIGTERM) takes at most 90 seconds: the database withdrawal waits at most 3
  seconds, the listeners and background loops share 30, and CI workers get 30, so
  `compose.yaml` gives the server a 100-second `stop_grace_period`.

## PgBouncer check

With release artifacts already built, PostgreSQL client tools and PgBouncer 1.21 or newer
on `PATH`, run `GITSERV_TEST_DATABASE_URL=postgres://... E2E_SKIP_BUILD=1 E2E_DATABASES=pgbouncer bash scripts/e2e.sh`.
The database role must be able to create disposable databases. This starts a private,
two-connection transaction pool and checks login, issues, SSH/HTTP push and clone. The
round also runs by default. It stops both processes and drops its database on exit; set
`E2E_KEEP=1` to retain logs (the private pool configuration contains database credentials).

For a complete cluster, use PgBouncer **session** pooling: serving and maintenance
leases retain database sessions. Notifications use cursor polling and retain none.
Budget backend connections for every process's primary pool, optional read pool and
these dedicated lease sessions. The small transaction-pool round above
checks ordinary API/Git traffic only. Run the session setup with
`E2E_PGBOUNCER_MODE=session E2E_DATABASES=pgbouncer bash scripts/e2e.sh`.

`builder-test.sh` marks its disposable PostgreSQL. PostgreSQL and cluster GitHub-client
rounds crash that owned database during `gh issue create`, check `/ready` goes 503 then
200 without restarting Codeyard, and verify every acknowledged issue remains. They never
restart an external database. `E2E_REQUIRE_PG_RESTART=1` makes a missing owned fixture a
failure. Helm lint and render checks pass, including the pool-size setting and credential
isolation. The session-pool round passes all 17 Git and HTTP checks. Owned PostgreSQL
crash checks pass during `gh issue create`, including readiness recovery and retained
acknowledged writes, both on one server and across cluster web nodes.

## One-cell benchmark

Run `GITSERV_TEST_DATABASE_URL=postgres://... bash scripts/bench-cell.sh` with an already
built `target/release/codeyard`, `codeyard-actions`, its web bundle, Python 3, curl, Git
and PostgreSQL client tools. Local CI needs the delegated cgroup described in
[CI_LOCAL_RUNNER.md](CI_LOCAL_RUNNER.md); `CI_PROCESS_CGROUP_ROOT` selects it. The database role needs `CREATEDB`. `BENCH_BINARY` and `WEBUI_DIST` override artifact
paths; `BENCH_SECONDS` sets the measurement duration (10–300, default 30).

The script starts one disposable server with four local CI slots and 512 page-render
waiters for its 500-reader burst (the production default remains 64), creates 50 organizations
via the native form (there is no REST creation endpoint), and creates public repositories
and issues through REST. During Git and reader traffic all 50 organizations issue an
invitation at once. It runs 500 concurrent anonymous page-reader slots and 50 initial REST issue-list reads
alongside 1 MiB clones and small pushes, each scheduling a CI job. The API sample stays
inside the real anonymous 60/hour IP budget; later reader batches contain only pages.
Every job must finish successfully; missing or queued runs fail the benchmark.
It prints p50/p95/p99 latencies, operation rates and clone object throughput, not wire throughput.
It also records the client/server CPU, RAM, fixture filesystem and PostgreSQL host; Git and API traffic use loopback.
CI latency includes admission and queue time. `CODEYARD_ACTIONS_BIN` overrides the runner path. These are
client slots, not 500 new accounts. Record the server and PostgreSQL CPU, RAM, disk and
network alongside results; this is a small workload, not a proven cell capacity.
It drops its database and removes temporary files on exit. `BENCH_KEEP=1` retains private
logs and fixture credentials for diagnosis. Neither script should target production.

## Organization login and API controls

The REST API's account and client-IP budgets live in PostgreSQL, so changing web nodes
or personal tokens does not reset a budget. Core requests allow 5,000 per account or
60 per anonymous IP per hour; search has its own budget. Client addresses come from the
connection unless `HTTP_TRUSTED_PROXY_IPS` names the proxy. Account bans and permissions
are read again on each request. The existing upload, login, body and concurrency limits
are listed in [Abuse prevention](ABUSE_PREVENTION.md).

An operator can configure one OIDC issuer per organization with `OIDC_PROVIDERS`, a JSON
array of at most 32 entries, at most 64 KiB in all. Each entry has `organization`,
`issuer`, `authorization_url`, `token_url`, `jwks_url`, `client_id` and
`client_secret_env`; the last field names an environment variable containing the secret.
Organization names in this configuration are 3–43 ASCII letters, digits, underscores or
hyphens. Endpoints require HTTPS; loopback HTTP is allowed for tests. Use the same
configuration on every web node. See [.env.example](../.env.example) for an example.

Give members `/login/oauth/oidc-<organization>` and register
`PUBLIC_URL/login/oauth/oidc-<organization>/callback` at the provider, using `client_secret_post` for token exchange. The flow uses the
existing single-use, browser-bound OAuth state, PKCE S256 and a nonce. Only signed RS256
ID tokens from the configured issuer, client and keys are accepted. Expiry uses the
database clock, so web-node clock differences do not change token validation. Changing issuer or
client invalidates unfinished flows. Identity uses issuer and subject, never an email;
organization membership must already exist when the session commits. A public-node
first login can create the local identity, but issues no session until an owner invites
that identity. On a private node, create the local account and its organization membership first, then
obtain its exact case-sensitive `sub` from the identity provider's console. With the same
OIDC environment, a local operator enrolls it using an active site administrator:

```bash
codeyard user enroll-oidc alice --administrator site-admin --organization acme --subject PROVIDER_SUBJECT
```

The command verifies live administrator and member access and records `org.sso_enroll`
in the same transaction as the identity link. It accepts an identifier, not a provider
access token. Issuer and subject form the identity, following
[OIDC Core](https://openid.net/specs/openid-connect-core-1_0.html#IDTokenValidation).
Password and token login remain available.

Successful organization logins and their session insert share an `org.sso_login` audit
transaction. Revoked membership, a restricted account, suspended organization governance
or an unavailable audit table refuses the login. The cross-node HTTP acceptance for this
flow passes on both database engines and across web processes with five-minute clock skew.

## Limits

The cluster assumes one PostgreSQL primary shared by every web, SSH and storage node.
Making that database highly available (Patroni, a managed service, a standby you
promote) is the operator's job. Optional `DATABASE_READ_URL` selects a physical read replica
for issue and pull request GET content. The server checks database identity and WAL replay
once per request and falls back to primary when it cannot prove the copy current within
two seconds. Authentication, permission checks and writes stay on primary; the same
credential's reads stay there for five seconds after a write. Other content families still
use primary. Workspace, Clippy and WASM checks pass; replica runtime acceptance is
pending.
`DATABASE_POOL_SIZE` sets each process's primary pool and optional read pool separately
(1–1,000 each, default 20); metrics include both. The primary has a five-second
acquisition deadline; replica selection and acquisition have a two-second deadline.
Both validate connections before reuse. Lost
connections reconnect through `DATABASE_URL`; writes with an uncertain result are
not automatically replayed. The debug load workload completed its 500-reader traffic
and 50 simultaneous invitations, but CI jobs failed without cgroup delegation. There
is no completed capacity measurement; size the installation from your own measurements.

Majority writes require both fixed storage voters before acknowledgement. Additional
copies and the explicit `--write-quorum 1` policy are asynchronous; there is no
geo-routing. Reads fail over to an eligible copy when the primary cannot be reached,
and clone/fetch sessions are spread across current copies. Other reads still prefer
 the primary. Take-over requires a current copy of every member of
the fork family. Legacy copies that are behind require `cluster promote --force`;
quorum promotion must retain every committed ref. A comparison between two repositories
(a fork's compare page) is not served by a copy.
Re-check the iroh and russh versions in [Architecture](ARCHITECTURE.md) when you upgrade
either. Cores and CI workers must run the same release, because a worker only claims from
cores on the same events protocol.

Account restrictions (bans, suspensions, demotions) take PostgreSQL advisory locks keyed
by schema and account ID. Authority readers take shared locks and restrictions take
exclusive ones. A write that touches several accounts locks them in sorted order, so
unrelated accounts never wait on each other. Names reserve only their case-folded user
and organization namespace key; email ownership reserves only the affected addresses,
in sorted order. The setup exclusion is used only before setup completes. Attestation
and artifact metadata changes serialize within their owner, and bundle collection locks
only its bounded candidate rows. Cache policy uses its settings row, and object uploads
retain only their selected capacity rows. These lock changes await runtime acceptance.

A CI claim returns at most 128 compatible runs (1 to 1024 in the configuration), but the
database may still examine a very large queued backlog to find them, and a claim restarts
from the oldest run each time. This is not measured. A hard bound needs indexed
scheduling buckets or a persistent fair cursor, so keep the queue short for now.


The cell directory source uses a separate PostgreSQL database. Initialize it with
`DIRECTORY_DATABASE_URL=... codeyard cell directory initialize`, register cells with
`cell directory register`, and grant read-only router keys with
`cell directory router-key --public-key <key>`. `cell directory serve` owns its node-key
TLS listener; callers pin that public key. Cell mutations recheck live cell enrollment
in their transaction. Public HTTP/SSH wiring is written but unchecked; signup claims and
foreign authorization are unfinished, so multiple cells are not yet a usable deployment.

Router source now includes bounded HTTP streaming and GraphQL field dispatch using the
application route catalogue. The unchecked public entry point is `serve --roles router`
with `DIRECTORY_ADDRESS`, `DIRECTORY_PUBLIC_KEY` and `CELL_REGION`. It opens no cell database.
Enroll its node key with `cell directory router-key` before starting it. HTTP and SSH
addresses use the existing serve options. Cell SSH listeners use builtin mode with the
same pinned directory settings; their cell node keys must be registered. Include each
cell's SSH host key SHA256 wire digest (hex) in its registered key pins. The router
terminates client SSH and the receiving cell rechecks the original client key and native
repository access. Router enrollment is checked live by the receiving cell.
These paths still need acceptance tests;
foreign credentials and stub transport remain unfinished. Stub storage and local access/listing
guards are written but unchecked. The generated schema crate is unchanged.

Native rooted-ID readers and common repository/team projections pass source checks.
Metadata-only owner lookup also passes source checks.
The router uses those repository/org roots for directory lookups; full node-family issuance,
foreign authorization and the public HTTP/SSH role remain open.

The directory source adds `cell directory allocate-prefix --node-key <public key>
--previous <prefix> --last-id <id>` and the matching authenticated TLS operation. A range
must reach 80% use; retries reserve the same successor and prefix zero is never allocated.
Successor reservations and stopped-cell renewal pass source checks. The command is
`cell renew-range --directory-address <address> --directory-key <public key>`. It retains
state/maintenance locks, reserves through pinned TLS and activates all sequence ranges and
the issuer together; rollback leaves old rows and stable cell ownership intact. Automatic
renewal remains open. A local issuer-history follow-up is written but unchecked so pending
OAuth and device codes can finish after their original range is retired.

Cell authorization RPC source uses `CELL_RPC_ADDRESS` with the pinned directory settings; register that listener with `cell directory register --rpc-address`. It shares the directory’s bounded TLS transport. Only live cell keys may request home answers; read-only router keys cannot. Configure `HTTP_TRUSTED_PROXY_IPS` on cell web nodes for the public routers’ addresses so client rate limits retain the original address. Home answers and RPC are unchecked, and foreign ingress and publication are still unfinished. App user-token export is not implemented yet.

Foreign token and SSH admission, exact Git proofs and repository scope checks are now written but unchecked. HTTP tests cover a collaborator writing across cells and live resource/token revocation; SSH tests push and clone through the public listener. Foreign browser-session admission and remote membership pointers remain unfinished.
