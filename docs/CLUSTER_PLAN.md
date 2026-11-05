# Cluster readiness plan

Goal: a cluster that survives losing any one machine, grows and shrinks from the admin
panel or the API, and can be trusted in production. This plan covers three things: what
to check in the cluster code that exists, what is still missing, and the decision made for
every open question. [Architecture](ARCHITECTURE.md#3-scaling-out-cells-behind-a-router-plan-not-built)
keeps the longer plan for cells; this file comes first and does not repeat it.

Items 1–9, 11 and 12 have source implementations and acceptance tests. Workspace,
Clippy and WASM checks pass, including the fault-test source and full-disk placement fix.
Library checks (1,058 tests), identity checks (161 tests), CI checks (211 passed,
19 ignored), focused runner isolation and Helm rendering pass.
Storage checks pass (99 passed, 3 ignored). Before cell work, all 221 cluster integration
checks passed, including full-disk failure/retry and interrupted cluster backup/restore.
Stage 0's alias fix passes workspace, Clippy and WASM checks, 307 focused library/store
tests and all 225 cluster tests. The dedicated quorum client round passes all 20 checks
without skips: HTTP/SSH push, editor save, gh branch creation and merge survive primary
SIGKILL; majority loss refuses a push. VM hard-reset acceptance needs a disposable target.
Browser bundle publication, delivery, native and hydrated cluster forms, and live
cross-node issue updates pass. The builder needs a writable delegated cgroup for finished-CI acceptance. Rolling upgrades need a previous-release target, and A16
has no measured limits yet.
Cluster mode requires PostgreSQL; the owner removed the experimental environment gate
when authorizing cells. Outstanding acceptance above remains open.

## Where we are

Built: roles (`web`, `ssh`, `storage`, `runner`), pairing by token or key, iroh and TCP
transport, placement of new repositories on the storage node with the most free space,
asynchronous copies with `cluster promote`, take-over after a node stops reporting, CI
workers on other machines, live logs and notifications across web nodes, and a four-node
round in `scripts/e2e.sh`.

The write path selects up to three fixed storage voters, PostgreSQL decisions and a ref
journal. Cluster files use two storage-node copies or optional shared S3. Repository initialization
and copy use the same quorum path and pass runtime checks on both database engines.

Not complete: finished-CI drain and shutdown, rolling upgrades and measured load limits.
The latest cluster round has 48 passes, one CI execution failure and six CI-dependent
skips on the builder without cgroup delegation.

Current acceptance status:

- 1: hiding-switch CLI refusal checks pass.
- 2: storage-node copies, capacity, loss, retry, drain and private access pass HTTP checks on both engines; native startup without S3 passes. Single-node uploads remain readable after runner transport starts and after restore.
- 3: quorum initialization, copy, default selection, voter replacement, primary loss and full-disk retry pass.
- 4: drain REST, authority, promotion and native/hydrated browser checks pass. Finished-CI drain acceptance needs a delegated cgroup.
- 5: quorum, drain, bounded scraping and acquisition waiter metrics pass HTTP acceptance.
- 6: bounded invitations and environment-only provisioning pass library and REST checks.
- 7: policy forms, lease leader, pause, dry run and storage rules pass on both database engines.
- 8: mover reservations, bandwidth limits and busy-reader retirement retry pass.
- 9: pool, Helm and the 17-check PgBouncer session round pass. The debug workload completes 500-reader traffic and 50 simultaneous invitations; CI jobs fail without a delegated cgroup, so no completed load measurement is recorded.
- 10: previous-release target needs confirmation before compatibility work.
- 11: primary snapshots, object copies, interrupted fenced import and per-node restore pass CLI acceptance.
- 12: cross-node REST account/IP, audit, OIDC membership and private enrollment CLI checks pass; OIDC also passes across web processes with five-minute skew.

## A. Checks on what exists

Run these first, on the builder, against real servers. Each has a pass condition. A check
that fails today is a bug or a known gap; it is fixed, or listed as a gap with the item in
part B that closes it. "Expected" is what the documents imply, not something already run.

| # | Check | Pass when | Current check |
|---|---|---|---|
| A1 | `scripts/e2e.sh` cluster round | green | 48 passes; one CI execution failure and six CI-dependent skips need a delegated cgroup |
| A2 | Push over HTTP and SSH, `kill -9` the storage primary the moment the push returns | within 2 minutes `git clone` has the commit, no `--force` | HTTP and SSH acknowledgements survive immediate storage-primary SIGKILL; both clone paths recover the acknowledged commit |
| A3 | Upload a package or CI artifact through web node 1, download it through web node 2 | same bytes | cross-node file and CI-artifact HTTP checks pass |
| A4 | Stop one web node while clients run `gh` and `git` | other nodes keep serving, sessions survive, no half-written state | gh/Git traffic survives web1 SIGKILL through web2; saved browser sessions work on web2 |
| A5 | Kill a storage node during a clone and a push | clear error to the client, no changed refs, clone retries from a copy | real clone and push PACK interruptions return client errors; copy retries preserve expected refs |
| A6 | Fill a storage node's disk | placement skips it, writes fail cleanly, no corrupt repository | owned tmpfs Git failure/retry and HTTP placement checks pass on both engines |
| A7 | Restart PostgreSQL under load | `/ready` goes 503 then 200, nodes reconnect without restart, no lost acknowledged write | owned PostgreSQL crashes during gh issue create; readiness recovers and every acknowledged write remains |
| A8 | Revoke a node key | the node is refused on its next call, within 60 seconds | TCP/QUIC revocation checks pass |
| A9 | Send an oversized, truncated, replayed and unauthorised frame to the node port | refused, bounded memory, logged without secrets | hostile transport and pairing checks pass |
| A10 | A compromised runner node asks for another run's token, another repository, another job's logs | refused every time | network isolation passes without local CI execution: forged claims, credentials, repository access, logs and reused-connection revocation |
| A11 | Comment on an issue on web node 1 with a browser open on node 2 | page updates within 2 seconds | browser acceptance passes: posts on web1 and updates Bob's open issue on web2 within two seconds |
| A12 | SIGTERM during running CI and a push | stops within 90 seconds, jobs end failed with a reason, no leaked containers or locks | e2e runs CI with an owned child, interrupts a pack and checks shutdown, failure reason and a later write; runtime pending |
| A13 | Old and new release side by side | refused with a clear message, or both work | older protocol refused; B10 pending |
| A14 | Join token: reuse, expiry, cancel, wrong role | each behaves as `CLUSTER.md` says | invitation and role checks pass |
| A15 | Clock skew of 5 minutes on one node | tokens, leases and take-over still behave | five-minute skew passes cross-node OIDC, gh/Git and browser checks; finished-CI checks remain blocked |
| A16 | Load test on stated hardware: pushes, clones, API reads, CI jobs, 50 organizations inviting at once | numbers recorded in `CLUSTER.md` limits | 500-reader traffic and 50 simultaneous invitations complete; CI jobs need a delegated cgroup before measurements are valid |

Each of A2 to A10, A12 and A15 becomes a test in `tests/`, driven through HTTP, SSH and
Git, or an `e2e.sh` step when it needs several processes. A check that cannot be automated
is written as manual steps beside the operations guide.

## B. What to build, in order

Every item has one answer. Numbers are defaults that an admin can change in settings.

1. **Cluster availability.** The owner removed the experimental environment gate when
   authorizing cells on 2026-11-05. `codeyard cluster enable` and `serve --cluster` retain
   their PostgreSQL and enrollment checks. Single-node SQLite remains supported.
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
13. **Cells.** The owner lifted A16 on 2026-11-05. Stage 0 reuses items 3 and 11;
    automated durability acceptance passes. Hard reset is runtime pending, needs a disposable
    VM. Stage 1's 51 PgBouncer/replica/quorum e2e checks pass without skips; finished-CI
    shutdown and capacity measurements need a writable delegated cgroup.
    Stage 2 directory claims/TLS/cache, routing, scoped credentials, stable homes, stubs,
    common rooted IDs, native claim publication, transactional claim cleanup, bounded capacity
    reports and foreign GraphQL quota ownership pass workspace bb56c210807b48338ec9987c8b7472c8,
    Clippy 769b632f8bba481485b42948058bb200 and WASM 1c95951358e9455fb98c704e454e1bef.
    Generated GraphQL is unchanged. Range renewal is a committed, deferred owner follow-up.
    Earlier runtime 616868dc57e7437c85afe790dcf2161a passes 400 changed-library tests,
    CI 220 with 19 existing cgroup-dependent ignores, pull requests 324 with two existing
    cgroup-dependent ignores, and all nine selected repository contracts. Cluster is 230/one
    and database 179/two; their quota and browser-fixture fixes pass source checks.
    Runtime eb1c71056db64586b3b41c713e7bf631 covers those fixes, native publication,
    cleanup/restart, capacity and the new public two-cell gh/Git round. All 334 selected
    library tests pass without skips, including real foreign SSH push/clone and revocation;
    all 234 cluster tests also pass without skips; database tests are running.
    Numeric child IDs and remote membership delivery/REST discovery pass workspace
    95564883b4604fb5bbd2968e4c6c5188, Clippy dc45e3525fc74aaabc0f286151667188 and WASM
    ba06675531a94ac0a434f8822c9a23ca. Both baselines retain stable review-request row IDs
    and protection-rule App owners. Old output assertions now expect rooted IDs; former input
    IDs remain accepted as the architecture requires. HTTP/TLS tests cover forged roots,
    pointer rollback/replay, grant revocation and gh pagination; runtime is pending.
    The next unchecked follow-up projects 64-bit IDs through GraphQL's nullable databaseId
    and fullDatabaseId fields and prevents membership reads from calling their own home cell.
    Native/GraphQL home listing integration and synthetic node families remain open.
    No stage 2 completion is claimed.
    Stages 3 to 6 are not built. Automatic user moves and org regional presence remain
    documented owner decisions.
    Owner follow-ups: foreign-cache expiry after PostgreSQL row-lock waits; schedule bounded
    expired directory-claim collection.

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
- **Is "ready" claimed early?** No. The owner removed the hiding switch when authorizing
  cells. Outstanding acceptance and unmeasured capacity stay listed above.

## Stop rules

The acceptance list is part A plus items 1 to 12. New edge cases found on the way become
notes for the owner, not new items. If three rounds each find new failures in one item,
stop and report in plain words what works and what does not.
