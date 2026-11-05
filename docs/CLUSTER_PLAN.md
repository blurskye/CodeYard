# Cluster readiness plan

Goal: a cluster that survives losing any one machine, grows and shrinks from the admin
panel or the API, and can be trusted in production. This plan covers three things: what
to check in the cluster code that exists, what is still missing, and the decision made for
every open question. [Architecture](ARCHITECTURE.md#3-scaling-out-cells-behind-a-router-plan-not-built)
keeps the longer plan for cells; this file comes first and does not repeat it.

Items 1–9 have source implementations and acceptance tests. The earlier source batch,
backup/restore and organization OIDC pass workspace, Clippy and WASM checks. Acquisition
waiter tracking passes source checks and awaits runtime acceptance. Majority defaults
and the final primary-loss e2e step pass source checks. Private organization OIDC
enrollment through the operator CLI passes source checks. One runtime job passed 1,013 library
tests and Helm render checks; its single CI fixture failure is corrected. The next runtime job found a quorum-copy lock deadlock and was cancelled. Its fix
and fixture corrections pass all three source checks; cluster, storage, identity, backup/restore and
end-to-end acceptance remain pending. Rolling upgrades
and measured load limits are not complete.
Cluster mode stays hidden behind
`CODEYARD_EXPERIMENTAL_CLUSTER=1` until part A passes and part B items 1 to 6 are done.

## Where we are

Built: roles (`web`, `ssh`, `storage`, `runner`), pairing by token or key, iroh and TCP
transport, placement of new repositories on the storage node with the most free space,
asynchronous copies with `cluster promote`, take-over after a node stops reporting, CI
workers on other machines, live logs and notifications across web nodes, and a four-node
round in `scripts/e2e.sh`.

The experimental write path uses two storage voters, PostgreSQL decisions and a ref
journal. Cluster files use two storage-node copies or optional shared S3. Repository initialization
and copy now use the same quorum path; source checks pass and runtime tests are pending.

Not complete: drain and failover acceptance, backup and restore verification,
rolling upgrades and measured load limits.

Current acceptance status (runtime checks are still pending):

- 1: hiding switch built; source checks pass.
- 2: two-copy storage-node backend, shared drain reservations and HTTP acceptance written; source checks pass, runtime pending.
- 3: quorum initialization, copy and voter replacement pass source checks; majority defaults and explicit legacy selection pass source checks.
- 4: drain source checks pass; native and hydrated browser acceptance written, runtime pending.
- 5: quorum, drain and acquisition waiter metrics pass source checks; runtime acceptance pending.
- 6: bounded invitations and environment-only provisioning source checks pass; runtime pending.
- 7: lease leader, policy settings, pause and dry run source checks pass; runtime pending.
- 8: mover and bandwidth limit source checks pass; runtime pending.
- 9: pool configuration source checks and Helm render checks pass; PostgreSQL failover, PgBouncer and load runs pending.
- 10: previous-release target needs confirmation before compatibility work.
- 11: primary snapshots, object copies, fenced import and per-node restore pass source checks; runtime pending.
- 12: cross-node REST account/IP and audit acceptance written; organization OIDC login and commit-time membership checks pass source checks; private enrollment CLI passes source checks, runtime pending.

## A. Checks on what exists

Run these first, on the builder, against real servers. Each has a pass condition. A check
that fails today is a bug or a known gap; it is fixed, or listed as a gap with the item in
part B that closes it. "Expected" is what the documents imply, not something already run.

| # | Check | Pass when | Current check |
|---|---|---|---|
| A1 | `scripts/e2e.sh` cluster round | green | pending builder acceptance |
| A2 | Push over HTTP and SSH, `kill -9` the storage primary the moment the push returns | within 2 minutes `git clone` has the commit, no `--force` | final cluster e2e step written; runtime pending |
| A3 | Upload a package or CI artifact through web node 1, download it through web node 2 | same bytes | cross-node HTTP regression written; runtime pending |
| A4 | Stop one web node while clients run `gh` and `git` | other nodes keep serving, sessions survive, no half-written state | gh/Git process-loss step and browser-session regression written; runtime pending |
| A5 | Kill a storage node during a clone and a push | clear error to the client, no changed refs, clone retries from a copy | e2e pauses real pack transfers for SIGKILL and verifies copy retries; runtime pending |
| A6 | Fill a storage node's disk | placement skips it, writes fail cleanly, no corrupt repository | bounded tmpfs Git failure/retry and HTTP placement acceptance written; checks pending |
| A7 | Restart PostgreSQL under load | `/ready` goes 503 then 200, nodes reconnect without restart, no lost acknowledged write | owned-database crash step written; runtime pending |
| A8 | Revoke a node key | the node is refused on its next call, within 60 seconds | pending builder acceptance |
| A9 | Send an oversized, truncated, replayed and unauthorised frame to the node port | refused, bounded memory, logged without secrets | pending builder acceptance |
| A10 | A compromised runner node asks for another run's token, another repository, another job's logs | refused every time | pending builder acceptance |
| A11 | Comment on an issue on web node 1 with a browser open on node 2 | page updates within 2 seconds | pending builder acceptance |
| A12 | SIGTERM during running CI and a push | stops within 90 seconds, jobs end failed with a reason, no leaked containers or locks | pending builder acceptance |
| A13 | Old and new release side by side | refused with a clear message, or both work | older protocol refused; B10 pending |
| A14 | Join token: reuse, expiry, cancel, wrong role | each behaves as `CLUSTER.md` says | pending builder acceptance |
| A15 | Clock skew of 5 minutes on one node | tokens, leases and take-over still behave | second web process runs five minutes ahead in OIDC and e2e acceptance; runtime pending |
| A16 | Load test on stated hardware: pushes, clones, API reads, CI jobs, 50 organizations inviting at once | numbers recorded in `CLUSTER.md` limits | benchmark written; no measurements yet |

Each of A2 to A10, A12 and A15 becomes a test in `tests/`, driven through HTTP, SSH and
Git, or an `e2e.sh` step when it needs several processes. A check that cannot be automated
is written as manual steps beside the operations guide.

## B. What to build, in order

Every item has one answer. Numbers are defaults that an admin can change in settings.

1. **Hide the cluster until it is ready.** `codeyard cluster enable` and `serve --cluster`
   print "cluster mode is not ready yet" unless `CODEYARD_EXPERIMENTAL_CLUSTER=1` is set;
   `scripts/e2e.sh` sets it. The docs say single node is the supported setup. The switch is
   removed when items 1 to 6 are done and part A passes.
2. **Files survive a web node.** Packages, CI artifacts, caches and LFS stay in the object
   store, which gets a third backend: storage nodes. An object is content-addressed
   (sha256, already declared at upload), written to two storage nodes before the upload is
   acknowledged, and read from either. S3 stays as an option for operators who have it.
   Cluster mode refuses to start with node-local files. Local disk remains the single-node
   default. Why not S3 only: it adds a required dependency; storage nodes already have
   disks, placement and copies.
3. **Write quorum.** As in the architecture document: a `reference-transaction` hook sends
   the change to the voting copies, a push is acknowledged after a majority hold it, and a
   push is refused with the reason when there is no majority within 10 seconds. Default
   `majority`; `1` keeps today's behaviour. Closes A2.
4. **Node states and drain.** A node is `active`, `draining` or `drained` (the schema
   column goes into `0001`). Drain is one function used by the panel, the REST endpoint and
   the CLI.
   - It first checks capacity: every repository on the node must have a current copy
     elsewhere or space for one, counting only active storage nodes and keeping 15% of
     each disk free. If not, it answers 409 with the missing bytes and changes nothing.
   - A draining node gets no new repositories. Each repository is promoted to its best
     other copy, using the existing promote code. The old copy is removed only after the
     replacement has applied.
   - A runner or web node stops taking new work, waits for running jobs up to the maximum
     job time, then cancels what is left with a message.
   - States, progress and the result appear on the Cluster nodes page. "Cancel drain"
     returns the node to `active`. Only site admins may drain; each action is audited.
   - A node that is already dead uses `cluster promote --all-from` as today.
5. **Metrics.** `/metrics` (Prometheus text) with disk free per node, repository bytes per
   node, copy lag, quorum failures, queued jobs and their age, drain progress, pool
   waiters. The documents list the alerts to set.
6. **Add nodes.** The panel and API create a join token (exists). If the operator sets
   `NODE_PROVISION_COMMAND`, the server also runs it, with the role and count in the
   environment, to start the machine. It comes from the server's environment only, never
   from the database or a request, has a timeout, and its output is logged. Provider
   drivers wait until someone needs one.
7. **Automatic scaling.** A policy per role in site settings: minimum and maximum nodes,
   the trigger, cooldown and step. One leader (a database lease) evaluates it every minute
   and only calls the add and drain functions above. A dry-run mode logs what it would do,
   and a switch pauses it. Triggers: storage when free space stays below 20% for 10
   minutes; runners when a queued job waits over 2 minutes. Runners scale down when idle
   for 15 minutes. **Storage nodes never scale down on their own**; an admin drains them.
   The maximum node count is the cost limit.
8. **Rebalancing.** A background mover copies repositories from nodes above 80% full to
   nodes below 60%, one move per node at a time, smallest and coldest first, with a
   bandwidth limit. It reuses copy-then-promote. After item 3, a move adds the new node as
   a voting copy, waits until it is current, and switches the primary with a write pause
   of a few seconds. A zero-pause move is not planned; GitHub pauses writes the same way.
9. **PostgreSQL high availability.** The operator runs Patroni, PgBouncer or a managed
   service; Codeyard does not manage it. Codeyard's part: the pool size is configurable
   (it is fixed at 20 today), connections recover after a failover, and the documents give
   a tested setup. Stage 0 check 6 in the architecture document (kill the primary during
   `gh issue create`) is the acceptance test. Reads to replicas come with stage 1.
10. **Rolling upgrades.** The node protocol accepts the previous release. After the first
    public release, schema changes are additive and a release is deployed in two steps
    (add, then remove). Acceptance: old and new binaries serve together while `e2e.sh`
    traffic runs. This replaces "cores and workers must match" in the limits.
11. **Cluster backup and restore.** `codeyard backup` stops refusing a cluster. The
    database comes from the operator's PostgreSQL backup tool; repositories are fetched
    from their primaries into a mirror; files come from the object store. Restore is tested
    into an empty cluster, followed by `repo verify --repair`.
12. **Production controls.** Per-account and per-IP rate limits, SAML or OIDC login for
    organizations, an audit log for organization and site actions, and the abuse controls
    in [ABUSE_PREVENTION.md](ABUSE_PREVENTION.md), all checked in the cluster round.
13. **Cells.** Stages 1 to 6 of the architecture document, in order, each with its own
    acceptance list. Not started until one cell is measured (A16) and PostgreSQL is the
    limit.

## Decisions that settle the open questions

- **Where do Git repositories live?** On storage-node disks, never on S3. Git does many
  small random reads; GitHub and GitLab do the same.
- **Who scales machines?** Codeyard decides and records when to add or drain; the
  provisioning command or an operator starts and stops machines. No cloud SDK inside the
  server.
- **What stops a bug or an attack from scaling the cluster without limit?** The maximum
  node count, the cooldown, the dry-run mode, the pause switch and the audit log.
- **What if a drain cannot fit?** It is refused with numbers before anything moves. An
  admin adds a node first.
- **What if a drain stalls?** It is resumable and shows which repository is stuck. The node
  stays `draining` and nothing is removed until the replacement has applied.
- **Zero-pause live moves?** Not planned. A few seconds of refused writes with a retry
  message.
- **SQLite in a cluster?** Not supported. One machine on SQLite is one cell; it joins a
  multi-cell deployment through `codeyard db copy` to PostgreSQL.
- **Is "ready" claimed early?** No. The hiding switch comes off only when part A passes,
  items 1 to 6 are done, and A16 has numbers.

## Stop rules

The acceptance list is part A plus items 1 to 12. New edge cases found on the way become
notes for the owner, not new items. If three rounds each find new failures in one item,
stop and report in plain words what works and what does not.
