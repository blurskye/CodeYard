# Running a cluster

A single machine needs none of this. An installation starts in single-node mode: one
`server` runs the web pages, Git storage and SSH, and `runner` machines may join to run
Actions jobs ([CI local runner](CI_LOCAL_RUNNER.md)). Use cluster mode when you want web
and SSH servers separate from the machines that hold the repositories, or several
repository disks. [Architecture](ARCHITECTURE.md) section 2 explains how it works.

Single node is the supported setup. Cluster mode is experimental: `cluster enable` and
`serve --cluster` refuse with "cluster mode is not ready yet" unless
`CODEYARD_EXPERIMENTAL_CLUSTER=1` is set in the environment.

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
- **Shared S3-compatible storage** for uploads, avatars, release assets, packages and
  Actions artifacts and caches (`S3_*`, see the [README](../README.md)). Every core node
  refuses to start in cluster mode without these settings. Use the same bucket on all
  nodes; single-node mode still defaults to local disk.
- **UDP or TCP between the nodes.** Nodes talk over QUIC on `IROH_BIND` (default
  `0.0.0.0:7444`). Direct connections are the default and there is no relay unless you
  set `IROH_RELAY`. Behind NAT or a container port mapping, tell the other nodes where to
  dial with `IROH_ADVERTISE=host:port`. If UDP cannot reach the nodes, use one TCP port
  instead; see [Reachable only over TCP](#reachable-only-over-tcp).

## Quorum push preparation

The opt-in two-voter push path requires both fixed storage voters. Preparation uses the
stored `write_timeout_seconds` deadline (default 10 seconds). On each storage node,
`WRITE_QUORUM_MAX_PACK_BYTES` limits a prepared pack (default 128 MiB, maximum 1 GiB).
Keep `codeyard-writes/` inside each repository with its objects: it retains undecided
and committed-but-unpublished writes for recovery. A refused preparation reports
"write quorum unavailable". An unknown outcome means fetch and check refs before retrying;
it does not mean the write was rolled back. Clusters without `write_quorum` keep the
existing push path.

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
  `codeyard_database_pool_waiters` is `NaN`: SQLx does not expose its acquisition queue.
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

On the machine that already runs Codeyard, or a new one, with PostgreSQL:

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
bypassed, and a Git session is retried only before the node's first reply. The serving node checks that version again before admitting the read, so pages, clones and
fetches never fall back to a copy that is behind. If no current copy can answer, the read
fails rather than returning older data. After a
dial to a node fails, requests for it skip the wait for five seconds and go straight to
the copies. A web node that also holds the copy reads it directly.

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

The promotion rules above describe asynchronous replication. With experimental
`write_quorum` enabled, `--force` is refused. Promotion fences the whole fork family
in new `terms/<uuid>/<term>/<uuid>.git` directories; retired directories are never reused.
The CLI attempts canonical recovery on its local voter. A pending-recovery error means
ownership changed but serving is still disabled. Retry on each voter with the same
`--to` target. Both voters must reconstruct before serving is enabled, so this two-voter
mode does not yet provide read availability while either voter remains down.
Non-push publishers (editor, merges, imports, gists, refs and retained Git-data objects)
are refused in quorum mode until their journaled preparation and apply paths exist.

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

## One-cell benchmark

Run `GITSERV_TEST_DATABASE_URL=postgres://... bash scripts/bench-cell.sh` with an already
built `target/release/codeyard`, its web bundle, Python 3, curl, Git and PostgreSQL client
tools. The database role needs `CREATEDB`. `BENCH_BINARY` and `WEBUI_DIST` override artifact
paths; `BENCH_SECONDS` sets the measurement duration (10–300, default 30).

The script starts one disposable server without CI workers, creates 50 organizations via
the native form (there is no REST creation endpoint), and creates public repositories and
issues through REST. It runs 500 concurrent anonymous reader slots against issue pages
and REST issue lists alongside 1 MiB clones and small pushes. It prints p50/p95/p99
latencies, operation rates and clone object throughput, not wire throughput. These are
client slots, not 500 new accounts. Record the server and PostgreSQL CPU, RAM, disk and
network alongside results; this is a small workload, not a proven cell capacity.
It drops its database and removes temporary files on exit. `BENCH_KEEP=1` retains private
logs and fixture credentials for diagnosis. Neither script should target production.

## Limits

The cluster assumes one PostgreSQL primary shared by every web, SSH and storage node.
Making that database highly available (Patroni, a managed service, a standby you
promote) is the operator's job; Codeyard does not route reads to database replicas.
Each process keeps a pool of 20 connections, which is not configurable yet. The cluster
has not been load tested, so size it from your own measurements.

Replicas are asynchronous: there are no synchronous writes across nodes and no
geo-routing. Reads fail over to a copy as soon as the primary cannot be reached, and are
not spread across copies. Writes wait for the take-over, which needs a current copy of
every member of the fork family; a copy that is behind is promoted only with
`cluster promote --force`. A
comparison between two repositories (a fork's compare page) is not served by a copy.
Re-check the iroh and russh versions in [Architecture](ARCHITECTURE.md) when you upgrade
either. Cores and CI workers must run the same release, because a worker only claims from
cores on the same events protocol.

Account restrictions (bans, suspensions, demotions) take PostgreSQL advisory locks keyed
by schema and account ID. Authority readers take shared locks and restrictions take
exclusive ones. A write that touches several accounts locks them in sorted order, so
unrelated accounts never wait on each other. Two global locks remain: one for the user
and organization name namespace and initial setup, and one for email ownership across
users and secondary emails.

A CI claim returns at most 128 compatible runs (1 to 1024 in the configuration), but the
database may still examine a very large queued backlog to find them, and a claim restarts
from the oldest run each time. This is not measured. A hard bound needs indexed
scheduling buckets or a persistent fair cursor, so keep the queue short for now.
