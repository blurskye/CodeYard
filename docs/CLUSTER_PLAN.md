# Cluster readiness plan

Goal: a cluster that survives losing any one machine, grows and shrinks from the admin
panel or the API, and can be trusted in production. This plan covers three things: what
to check in the cluster code that exists, what is still missing, and the decision made for
every open question. The [cells plan](#cells-plan) at the end holds the stages and their
acceptance lists; [Architecture](ARCHITECTURE.md) describes what is built.

The results below are accepted checkpoints, not a release sign-off for every later edit.
The 2026-11-12 final native checkpoint passed 57 broker tests, including both real
OpenSSH tests and the four Prepared authority/pre-admission budget tests; eight fork
tests; 238 CI tests; a separate browser test; five executable-verifier tests; and the
pinned-act dotenv test. None failed or remained ignored in these selected suites.
Earlier native attempts failed in the OpenSSH/act fixture setup and then its
workflow-snapshot assertion; the repaired fixture passed both a focused diagnostic
and this full native run. These named results do not replace historical workspace
totals with a fresh all-targets test claim.

Items 1–9, 11 and 12 have implementations and runtime acceptance. The final end-to-end
run passed 241 checks with no failures or skips: two build checks, SQLite 59,
PostgreSQL 73, PgBouncer 15, physical read-replica 16, rolling cluster 55, quorum 18
and public cells 3. The four-node round starts from published source `49241c9c`,
checks Git continuity and completed CI after each node upgrade, then performs the
fault-injection checks. The cells round uses that published router against the current
directory and cells. Earlier 56/57-check cluster and five-check mixed-router results
remain historical checkpoints, not the counts of this run.
Six final-source Linux guest hard resets also passed on both database engines.
HTTP guest resets do not cover stateful SSH/RPC receives; those have separate
broker and process-kill acceptance above.

Current native formatting and workspace Clippy pass. Earlier WASM checks and the
repository-service consolidation's 243 affected unit tests passed. CI checks cover
both database engines, restored
membership, log replay and owned-process cleanup; runner settings pass hydrated and
JavaScript-disabled browser flows. Transient systemd services provide delegated cgroups
without persistent host policy changes.

The earlier 500-reader/50-invitation benchmark completed all 16 CI jobs. It predates
the later receive/durability changes. Its debug build, hardware, resource limits and
latency sample are recorded in
[Cluster operations](CLUSTER.md#one-cell-benchmark), not presented as production capacity.
The builder boots disposable Linux guests under QEMU TCG without KVM. On 2026-11-12,
all six single-node hard-reset checks passed: two pushes and one default-branch change
on each of SQLite and PostgreSQL, followed by clone, fsck and exact data checks.
This is guest-reset evidence, not whole-host power-loss acceptance.
Separate-region recovery still needs disposable targets.
Cluster mode requires PostgreSQL; the owner removed the experimental environment gate
when authorizing cells. Separate-region recovery and deployment-capacity acceptance
remain open.

## Where we are

Built: roles (`web`, `ssh`, `storage`, `runner`), pairing by token or key, iroh and TCP
transport, placement of new repositories on the storage node with the most free space,
asynchronous copies with `cluster promote`, take-over after a node stops reporting, CI
workers on other machines, live logs and notifications across web nodes, and a four-node
round in `scripts/e2e.sh`.

The write path selects up to three fixed storage voters, PostgreSQL decisions and a ref
journal. Cluster files use two storage-node copies or optional shared S3. Repository initialization
and copy use the same quorum path and pass runtime checks on both database engines.

Finished-CI drain/resume, revocation, interrupted-push shutdown and the published-source
rolling-upgrade fixture pass. After two historical maintained-image checks, a fresh
Windows Server 2025 Evaluation image completed unattended provisioning without manual
input. Its experimental guest API test passed under TCG in 1,034.19 seconds, checking
boot, tools, transfers, environment, exit status and cleanup; the image digest was
unchanged. See the [image acceptance record](../backend/runner/scripts/windows-base-image/README.md#validate-before-use).
Windows workflow execution remains unsupported. Final-source single-node Linux
VM hard-reset checks pass on both database engines; separate-region recovery remains open.
The published-version router passes against current cells. Production capacity requires
measurements on the intended deployment; the recorded shared-builder debug sample is
only a bounded workload check.

Current acceptance status:

- 1: hiding-switch CLI refusal checks pass.
- 2: storage-node copies, capacity, loss, retry, drain and private access pass HTTP checks on both engines; native startup without S3 passes. Single-node uploads remain readable after runner transport starts and after restore.
- 3: quorum initialization, copy, default selection, voter replacement, primary loss and full-disk retry pass.
- 4: drain REST, authority, promotion, completed-CI drain/resume and native/hydrated browser checks pass.
- 5: quorum, drain, bounded scraping and acquisition waiter metrics pass HTTP acceptance.
- 6: bounded invitations and environment-only provisioning pass library and REST checks.
- 7: policy forms, lease leader, pause, dry run and storage rules pass on both database engines.
- 8: mover reservations, bandwidth limits and busy-reader retirement retry pass.
- 9: pool, Helm, PgBouncer and physical-replica rounds pass. The debug workload completes 500-reader traffic, 50 simultaneous invitations and every CI job; measurements are recorded in `CLUSTER.md`.
- 10: rolling replacement from published source `49241c9c` passes Git and CI checks after every node. Its router also passes public two-cell acceptance against the current directory and cells. Older tagged protocols remain incompatible.
- 11: primary snapshots, object copies, interrupted fenced import and per-node restore pass CLI acceptance.
- 12: cross-node REST account/IP, audit, OIDC membership and private enrollment CLI checks pass; OIDC also passes across web processes with five-minute skew.

## A. Checks on what exists

Run these first, on the builder, against real servers. Each has a pass condition. A check
that fails today is a bug or a known gap; it is fixed, or listed as a gap with the item in
part B that closes it. "Expected" is what the documents imply, not something already run.

| # | Check | Pass when | Current check |
|---|---|---|---|
| A1 | `scripts/e2e.sh` cluster round | green | The final 241-check E2E run includes 55 published-predecessor rolling-cluster checks, without failures or skips |
| A2 | Push over HTTP and SSH, `kill -9` the storage primary the moment the push returns | within 2 minutes `git clone` has the commit, no `--force` | HTTP and SSH acknowledgements survive immediate storage-primary SIGKILL; both clone paths recover the acknowledged commit |
| A3 | Upload a package or CI artifact through web node 1, download it through web node 2 | same bytes | cross-node file and CI-artifact HTTP checks pass |
| A4 | Stop one web node while clients run `gh` and `git` | other nodes keep serving, sessions survive, no half-written state | gh/Git traffic survives web1 SIGKILL through web2; saved browser sessions work on web2 |
| A5 | Kill a storage node during a clone and a push | clear error to the client, no changed refs, clone retries from a copy | real clone and push PACK interruptions return client errors; copy retries preserve expected refs |
| A6 | Fill a storage node's disk | placement skips it, writes fail cleanly, no corrupt repository | owned tmpfs Git failure/retry and HTTP placement checks pass on both engines |
| A7 | Restart PostgreSQL under load | `/ready` goes 503 then 200, nodes reconnect without restart, no lost acknowledged write | owned PostgreSQL crashes during gh issue create; readiness recovers and every acknowledged write remains |
| A8 | Revoke a node key | the node is refused on its next call, within 60 seconds | TCP/QUIC revocation checks pass |
| A9 | Send an oversized, truncated, replayed and unauthorised frame to the node port | refused, bounded memory, logged without secrets | hostile transport and pairing checks pass |
| A10 | A compromised runner node asks for another run's token, another repository, another job's logs | refused every time | claim, credential, repository, log and reused-connection revocation checks pass; delegated host CI executes real workflows |
| A11 | Comment on an issue on web node 1 with a browser open on node 2 | page updates within 2 seconds | browser acceptance passes: posts on web1 and updates Bob's open issue on web2 within two seconds |
| A12 | SIGTERM during running CI and a push | stops within 90 seconds, jobs end failed with a reason, no leaked containers or locks | host-CI child termination, interrupted PACK, failed-job reason, clone continuity and a later quorum write pass |
| A13 | Old and new release side by side | refused with a clear message, or both work | published source `49241c9c` upgrades one node at a time with Git and completed CI; older tagged ALPN versions are not rolling-compatible |
| A14 | Join token: reuse, expiry, cancel, wrong role | each behaves as `CLUSTER.md` says | invitation and role checks pass |
| A15 | Clock skew of 5 minutes on one node | tokens, leases and take-over still behave | five-minute skew passes OIDC, gh/Git, browser and completed-CI cluster checks |
| A16 | Load test on stated hardware: pushes, clones, API reads, CI jobs, 50 organizations inviting at once | numbers recorded in `CLUSTER.md` limits | debug benchmark completes 5,500 page reads, 50 API reads, 15 pushes/clones, 50 invitations and 16 successful CI jobs; hardware and latencies recorded |

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
3. **Write quorum.** As in the [stage 0 plan](#stage-0-durability-on-one-installation-and-in-every-cell): a `reference-transaction` hook sends
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
   service; Codeyard does not manage it. Codeyard's part: the pool size is configurable,
   connections recover after a failover, and the documents give
   a tested setup. Stage 0 check 6 in the [cells plan](#cells-plan) (kill the primary during
   `gh issue create`) is the acceptance test. Reads to replicas come with stage 1.
10. **Rolling upgrades.** The node protocol accepts the previous release. After the first
    public release, schema changes are additive and a release is deployed in two steps
    (add, then remove). Acceptance: old and new binaries serve together while `e2e.sh`
    traffic runs. This replaces "cores and workers must match" in the limits.
    Set `E2E_PREVIOUS_BINARY` to an absolute previous executable path and select
    `E2E_DATABASES=cluster` to enable the fixture. It starts four old nodes, then
    replaces web1, store1, store2 and ci1 while checking the Git head, a new push and
    a successful CI run after each replacement. Fault injection follows the upgrades.
    Set `E2E_PREVIOUS_ROUTER_BINARY` with `E2E_DATABASES=cells` to run the complete
    public cells fixture through an older router against the current directory and cells.
    The verified baseline is published source `49241c9c`, not the older tagged ALPN.
11. **Cluster backup and restore.** `codeyard backup` stops refusing a cluster. The
    database comes from the operator's PostgreSQL backup tool; repositories are fetched
    from their primaries into a mirror; files come from the object store. Restore is tested
    into an empty cluster, followed by `repo verify --repair`.
12. **Production controls.** Per-account and per-IP rate limits, SAML or OIDC login for
    organizations, an audit log for organization and site actions, and the abuse controls
    in [ABUSE_PREVENTION.md](ABUSE_PREVENTION.md), all checked in the cluster round.
13. **Cells.** The owner lifted A16 on 2026-11-05. Stage 0 reuses items 3 and 11;
    automated durability acceptance and six single-node TCG guest hard-reset checks pass.
    Whole-host power loss remains distinct. Stage 1's PgBouncer, replica and quorum rounds
    pass. Delegated host
    CI shutdown passes, and the bounded debug workload now includes successful CI.
    Stage 2 runnable acceptance passes in runtime 15c77f36a9794468ac72f976e6c78209:
    five real public two-cell checks, 68 repos-core library tests, 61 store tests and
    all 234 cluster tests, without skips. Real clients cover standalone cell-zero
    IDs/data/session/PAT/SSH-key preservation; cell-1 signup and duplicate-name refusal;
    gh login, repository creation, HTTP/SSH pushes and clones; foreign collaborator
    and org/team grants; home membership lists; native-only member lists/counts and
    search; raw/hydrated navigation and JavaScript ID precision; rooted node lookup
    and split GraphQL; gh pr create/merge; strict 60-second authentication revocation;
    and clear stopped-cell errors. Both-engine database and exhaustive GraphQL
    contracts pass. Generated GraphQL remains unchanged. Stage 2's last source checks
    pass workspace 1eea95d5da154951a089895b7e135cbc, Clippy
    5bbeecaa23534c9086bf16bc198a3001 and WASM 045f16dfddd44168a4379af54001a42c.
    Stage 3 items 5/7 have resource-owned stars, distinct home discovery pointers,
    bounded authorized REST/browser lists and SIGKILL outbox replay. Runtime
    5993a963beb74088a032f3f29c97fbdc passes all five public cells checks, 144 selected
    library tests and both membership contracts without skips. Cluster passes 232/234;
    SQLite duplicate-begin recovery and replica rebalancing fail and await retry.
    The bounded star projection/third-page follow-up passes workspace
    1e483729c5ba4ee3b5ca63ab06bc8212, Clippy 4b28e2a861b34559a3a9e540f49b5eb3
    and WASM c6a1a0038b3147bb82688256662a5028; its runtime is pending.
    Fork-source admission retains original proofs, verifies directory homes, redacts
    private metadata and rechecks source and destination access in both-engine writes.
    It passes workspace e401f6085a944da88801854b240a525d, Clippy
    25ac9880ad294027a1ac8a783e8f92a4 and WASM 7078759ffd0242dcb456214cdb71a296.
    Bounded TLS fork exports share native framing and forward to storage nodes.
    Hostile roots/readers, revoked access and failed/oversized stream tests are written.
    Recheck passes: workspace 693c4fdac519429e8ea01446c01f4402, Clippy
    35d9047eb87840b096823571b58623a6 and WASM a5bd1ceff3ca4a118123667d3a00f703. The gh/Git cross-cell fork/PR fixture is written; public fork creation
    runtime is pending. Destination staging/strict indexing through the shared native/quorum
    publisher, token-id binding and final storage membership checks pass workspace
    eaa2326fa6a84df2b38414b5cc68bd7f, Clippy cdaa8df107424d6282023027ce9d41fb
    and WASM cd0f45142a474baf9d0e175b46f61792. The rebalance fixture starts its third
    node only after verifying its initial copy, preserving every assertion.
    Original-authentication HTTP/CSRF forwarding, native fork forms and authenticated
    foreign parent metadata pass workspace ede3384871e6430fa4ed4a476bb89d4c,
    Clippy b4712c300df94108a7c009e3cf4160f3 and WASM 351812a025d9448ab7a8bf2e3497f655.
    A real storage-node publication/interrupted-stream test and renamed foreign-fork
    discovery with retained parent metadata are the next unchecked batch. Content
    reads retain their original authority independently of permission to create a fork.
    The unused HTTP Git-source URL is removed; copies exclusively use pinned RPC.
    Metadata/publication passes workspace 3e3916aee0ac4c74a978b80be4662525,
    Clippy f49d17ec064545ae92afc1cbb4c5bf4f and WASM 9172c32344114cdd943797b7471b4ffa.
    The next source follow-up adds exact-branch snapshots for pull heads and shares
    their private staging with fork copies; hostile selector/storage tests are written.
    Both baselines exclude metadata-only stub insert/delete from native fork counters.
    This follow-up passes workspace 36669df79199436886e88c7961c5d8fd,
    Clippy 73d4ef6b6117411493559eed584420ce and WASM 37c0dde00fa04564b6cf2a44c565de95.
    Runtime 67eb28f748434d8d839860a974a0fb07 passes all 235 cluster tests without skips
    on its earlier export snapshot. CI passes 220 with 19 existing environment-dependent
    ignores; storage passes 100 with three existing S3-fixture ignores. The public cells
    workflow passes its existing clients but fails before its new fork request because
    gh rejects --remote=false with a repository argument. The invalid flag is removed;
    the fork/push/PR/maintainer/checkout/merge assertions are unchanged.
    Combined runtime 921a8af65cf34bde866370a8f67bfc22 runs public cells first, then
    changed libraries, store/write tests and database, cluster, repository, CI and storage
    targets on the current source. Its public client now forks across cells, clones and
    pushes a new fork branch, then fails at gh pr create on the pre-PR snapshot.
    The next unchecked PR batch resolves cold heads by name/ID, privately stages branch
    comparisons, retains original proofs through both-engine insertion and publishes
    retained pull refs through native/quorum storage. HTTP/GraphQL, native storage,
    revoked-peer and final-head-revocation tests are written. The first check round is
    workspace 173c17ae2add42ab8cd8ac5c2cb7e4b8, Clippy e6cb8889113d4af6b19a4fcd8dfe28be
    and WASM af93df6a493d47ac93b3da5a8bc34e1f. Workspace/Clippy found one native-form
    error-conversion issue; its actor check now runs before the form's API-result block.
    WASM passes. Push-following source adds bounded message types, source-owned consent
    rows and a transactional write-version event trigger in both baselines. Both-engine
    subscription/admission transactions and a durable per-pull refresh cursor are written;
    transport, storage delivery and maintainer grants are not wired yet. This batch is
    unchecked and has not run acceptance.
    Stage 3 items 1 to 4 pass. The head cell queues `repository.push` after a branch moves
    and the base follows it; maintainer pushes use the base cell's live grant over HTTP and
    SSH; public forks on standalone storage share `pools/<id>.git`; mentions of foreign
    accounts reach their home inbox once per event with one email. Runtime
    07c720ed82c04c5ab170002893bbe5e2 passes the public cells workflow (cross-cell fork,
    followed pushes, `gh pr checkout`, `gh pr merge`, maintainer SSH push, mention in
    `gh api notifications`, outage and revocation checks), object pools (2, one with 100
    forks sharing one pool), the concurrent
    fork and native SSH maintainer tests; 9a0617eca5484ff3b023592c0a81fb67 passes the six
    cluster `cells_` tests; 19757052dce8440bb55447cb4d7bb1b3 passes git 43, cluster 235,
    storage 100 (3 ignored), the pull-request maintainer tests and the changed libraries.
    Checks: Clippy 9acd223d78e34af3992158a6182d73d0, WASM
    d716c381b8bc42e5a8fe6a5a427448a7. Runtime 789032d15eb1405b844c84629373faa5 on the
    previous source ran database 185, pull requests 324, CI 220, storage 100,
    collaboration 230, notification polling 5, identity 166 and the changed libraries
    without failures. Item 6, cross-cell repository transfer, is not written: it needs
    the stage 6 row-ownership copy for one repository.
    Owner notes: every followed push ships the whole branch pack; a pull that keeps failing
    holds back later pulls on the same refresh event, with backoff; maintainer pushes get no CI token; a native maintainer
    credential with a foreign base is refused; the upstream owner does not read a private
    cross-cell fork (head.repo is null for them); pools are never pruned when forks go and
    are not used in cluster storage; private-repository mentions stay in their cell; a
    delivered notice stays visible if its repository later turns private, because the
    home's stub is not refreshed; a base merges a foreign head from its followed pull ref and
    does not see a head branch deleted in the fork; an SSH maintainer of a cross-cell fork
    must have the key in the base's cell; a cell keeps another cell's directory record,
    including its web addresses, for the route cache lifetime.
    The outbox now leases each claimed event instead of holding its row lock across the
    network call; only the latest claim records a result. A key release still holds its
    key lock across the directory call, so a key added again meanwhile is never released.
    Stage 4 items 1, 2 and 4 are written: PostgreSQL tables in the directory database hold
    repository, issue, pull and code documents. Each cell writes them through `cell_outbox`
    (one pending event per document) and rechecks its own hits against live access.
    Runtime 1caddf64d9b84499abfaa02634be7ba5 passes the public cells workflow (gh search
    repos/issues/prs/code across both cells within a minute, private items only to readers,
    rebuild from nothing, 503 with pages working while search is stopped) and the seven
    cluster `cells_` tests, including `cells_search` and the lease test. Job
    685e69df09084b428803679f3f1b6e09 passes cluster search 5, collaboration search 15,
    database 185, object pools 2 and the changed libraries (150). Item 3 is written:
    Explore, Trending and "Used by" read the shared index (`search_packages` holds each
    public repository's dependencies and its root npm name), and each cell matches the same
    OSV mirror itself (`cells_advisories`). Job 34215ceffd2d4150b4cc419834481210 passes the 28
    cells, directory, search, backups and quorum cluster tests.
    Item 3 notes: the Explore language list stays local; index cards show no language
    colour; Explore pages past 1,000 results and failed index calls list this cell only;
    "Used by" covers public repositories and root npm names only, and shows nothing without
    an index; every cell's admin configures the same OSV source.
    Stage 5 is partly written. (1) Signups follow the router's region and accounts keep
    their home (`cells_regions`); pinning only matters once stage 6 moves accounts, so it
    is built with them. (2) Region loss: the operator restarts cells from paired copies,
    runs `repo verify --repair` (already listing its fixes) and registers the paired
    addresses; `cells_regions` checks routers follow. The real restart is runtime pending,
    needs a second region with a PostgreSQL standby and Git copies. (3) `cell directory
    rollout` updates cells in waves and stops at a failing canary (e2e); old routers read
    new cell records. The published `49241c9c` predecessor passes four-node upgrades and the
    public two-cell round with its old router against the current directory and cells. (4) Directory shards move between clusters
    (`cell directory move-shard`): each shard keeps its own object copies, writes hold the
    map row until they commit, and the database test moves shards while lookups run without
    a miss; job 1c765ab87a7d480d8e09d95f28068432 passes all 186 database tests.
    (5) Presence is set by hand (`cell directory presence`), and routers in a present region send reads to the home cell's regional web
    nodes, falling back home; writes go home (`cells_regions`). Automatic presence: routers
    stamp their region on forwarded requests, a cell counts its authenticated requests per
    account, region and day (only from trusted proxies), and once an hour one web node
    adds or removes its organizations' presence by the site-settings policy (share, days
    held, dry run, pause; audited; hand-set presence stays; `cells_regions`, job
    4bc97cffa295491bae7a54bd6d778f06). Serving from a real replica
    and regional Git copies is runtime
    pending, needs a cell in cluster mode with a PostgreSQL streaming replica and storage
    nodes in a second region. The written parts pass in job 34215ceffd2d4150b4cc419834481210.
    Stage 5 notes: SSH Git and package paths always go home; a router learns presence
    changes within its 60-second route cache; a rollout's apply program is killed on
    timeout but its own children are not. SSH requests are not counted; counts of a minute
    whose write fails are dropped; routers must be in the cell's trusted proxies; a region
    an admin removes by hand can be added again by the next evaluation.
    Stage 4 notes: users, labels, topics, commits and GraphQL search stay local; star counts
    in the index wait for the repository's next change; an issue document keeps its old
    `repository_url` after a rename until it changes; READMEs and licenses are not
    indexed; internal repositories follow
    `is_private`; App, fine-grained, job and foreign-session credentials see no private hits
    from other cells; an org that refuses classic tokens hides its other-cell hits from every
    whole-account credential, including ones that policy would exempt.
    Stage 6, item 1: every table of both baselines has one ownership decision in
    `backend/database/src/cells/ownership.rs`, and `every_table_has_an_ownership_rule` fails
    when a table has none or names a missing column or table. A row belongs to its first
    non-null reference: an account column, a repository column (the repository's owner),
    or a parent row. The decisions, by first reference (columns in brackets, `a>b` is a
    parent row in table b):

    - **Repository first, 120:** action_runner_group_repositories (repo_id);
      action_runner_remove_tokens (repo_id, org_id); action_runner_tokens (repo_id, org_id);
      action_runners (repo_id, org_id); actions_concurrency_groups (repo_id);
      actions_job_concurrency (repo_id); actions_log_downloads (repo_id); actions_objects
      (repo_id); actions_oidc_repo_settings (repo_id); actions_policies (repo_id, org_id);
      actions_run_concurrency (repo_id); actions_workflows (repo_id); artifact_attestations
      (repo_id); artifact_metadata (repo_id); branch_protection_rules (repo_id);
      branch_redirects (repo_id); check_runs (repo_id); check_suite_preferences (repo_id);
      check_suites (repo_id); ci_accepted_pushes (repo_id); ci_deployments (repo_id); ci_runs
      (repo_id); ci_schedule_runs (repo_id); code_scanning_alert_counters (repo_id);
      code_scanning_alerts (repo_id); code_scanning_analyses (repo_id); code_scanning_uploads
      (repo_id); codeql_databases (repo_id); commit_comments (repo_id); commit_status_history
      (repo_id); commit_statuses (repo_id); dependabot_alerts (repo_id); dependency_sbom_reports
      (repo_id); dependency_snapshots (repo_id); deploy_keys (repo_id); deployment_environments
      (repo_id); discussion_categories (repo_id); discussions (repo_id); events (repo_id);
      gist_comments (repo_id); gist_stars (repo_id); gists (repo_id);
      immutable_release_publications (repo_id); immutable_release_repository_names (repo_id);
      interaction_contributors (repo_id); interaction_limits (repo_id, org_id, user_id);
      interaction_pull_bypass (repo_id); interaction_pull_caps (repo_id, org_id);
      issue_linked_branches (repo_id); issue_redirects (repo_id); issues (repo_id); labels
      (repo_id); milestones (repo_id); org_actions_value_repositories (repo_id);
      org_dependabot_access_repositories (repo_id); org_dependabot_secret_repositories
      (repo_id); organization_external_property_values (repo_id);
      organization_immutable_release_repositories (repo_id); pages_builds (id);
      pages_deployments (id); pages_sites (repo_id); pinned_issues (repo_id); project_items
      (repo_id); project_repositories (repo_id); pull_request_async_merges (repo_id);
      pull_request_stacks (repo_id); pull_requests (repo_id); reactions (repo_id); releases
      (repo_id); remote_pull_requests (base_repo_id); repo_access (repo_id); repo_actions_keys
      (repo_id); repo_actions_settings (repo_id); repo_actions_variables (repo_id);
      repo_dependabot_keys (repo_id); repo_dependabot_secrets (repo_id); repo_environment_values
      (repo_id); repo_languages (repo_id); repo_secrets (repo_id); repositories (id);
      repository_advisories (repo_id); repository_autolinks (repo_id);
      repository_dependency_security (repo_id); repository_deployments (repo_id);
      repository_discussion_counters (repo_id); repository_discussion_settings (repo_id);
      repository_features (repo_id); repository_immutable_releases (repo_id);
      repository_import_authors (repo_id); repository_imports (repo_id); repository_invitations
      (repo_id); repository_issue_suggestions (repo_id); repository_merge_epochs (repo_id);
      repository_migration_locks (repo_id); repository_private_reporting (repo_id);
      repository_property_values (repo_id); repository_pushes (repo_id);
      repository_ref_activities (repo_id); repository_rule_suites (repo_id, org_id);
      repository_rulesets (owner_repo_id, owner_org_id); repository_security_reports (repo_id);
      repository_security_settings (repo_id); repository_star_days (repo_id); repository_topics
      (repo_id); repository_traffic_counts (repo_id); repository_traffic_keys (repo_id);
      repository_traffic_visitors (repo_id); secret_scanning_alerts (repo_id);
      secret_scanning_bypasses (repo_id); secret_scanning_commits (repo_id);
      secret_scanning_jobs (repo_id); secret_scanning_locations (repo_id);
      secret_scanning_patterns (repo_id, org_id); secret_scanning_settings (repo_id); stars
      (repo_id); team_repo_access (repo_id); timeline_events (repo_id); vulnerability_jobs
      (repo_id); watches (repo_id); webhooks (repo_id, org_id).
    - **Account first, 57:** action_runner_groups (org_id); actions_oidc_org_settings (org_id);
      artifact_metadata_jobs (org_id); artifact_metadata_records (org_id); audit_log (org_id);
      cell_presence (organization_id); code_security_configurations (org_id); email_tokens
      (user_id); follows (following_id); github_app_installations (org_id, user_id);
      github_app_manifests (user_id); github_app_requests (org_id, user_id); github_apps
      (owner_id); gpg_keys (user_id); immutable_release_owner_names (owner_id); issue_attributes
      (owner_user_id, owner_org_id); notification_threads (user_id); notifications (user_id);
      oauth_accounts (user_id); oauth_app_grants (user_id); oauth_applications (owner_id);
      org_actions_keys (org_id); org_actions_settings (org_id); org_actions_values (org_id);
      org_dependabot_access (org_id); org_dependabot_keys (org_id); org_dependabot_secrets
      (org_id); org_memberships (org_id); org_private_registries (org_id); organization_blocks
      (org_id); organization_external_property_installations (org_id);
      organization_immutable_releases (org_id); organization_invitations (org_id);
      organization_member_history (org_id); organization_repository_properties (org_id);
      organization_role_assignments (org_id); organization_settings (org_id);
      organization_token_policies (org_id); organizations (id); packages (owner_user_id,
      owner_org_id); personal_access_tokens (user_id); projects (owner_user_id, owner_org_id);
      registry_namespaces (owner_user_id, owner_org_id); remote_pointers (user_id);
      repository_exports (owner_user_id, owner_org_id); secret_scanning_pattern_configs
      (org_id); sessions (user_id); ssh_keys (user_id); ssh_signing_keys (user_id); teams
      (org_id); two_factor_auth (user_id); two_factor_recovery_codes (user_id); user_blocks
      (user_id); user_emails (user_id); user_profile_settings (user_id); user_social_accounts
      (user_id); users (id).
    - **Parent row, 72:** actions_blob_grants (object_id>actions_objects); actions_blob_parts
      (object_id>actions_objects); actions_deployment_reviews (run_id>ci_runs);
      actions_github_tokens (run_id>ci_runs); actions_job_annotations (job_id>actions_jobs);
      actions_job_states (run_id>ci_runs); actions_jobs (run_id>ci_runs); actions_oidc_jobs
      (token_id>personal_access_tokens); actions_oidc_properties
      (definition_id>organization_repository_properties); actions_replays (run_id>ci_runs);
      actions_run_metadata (run_id>ci_runs); actions_runtime_tokens (run_id>ci_runs);
      actions_step_summaries (job_id>actions_jobs); actions_steps (job_id>actions_jobs);
      activity_feed_tokens (api_token_id>personal_access_tokens); attestation_subjects
      (attestation_id>artifact_attestations); check_annotations (run_id>check_runs);
      ci_log_chunks (run_id>ci_runs); code_scanning_analysis_findings
      (analysis_id>code_scanning_analyses); code_scanning_instances
      (alert_id>code_scanning_alerts); deployment_branch_policies
      (environment_id>deployment_environments); deployment_protection_gates (run_id>ci_runs);
      deployment_protection_rules (environment_id>deployment_environments); deployment_statuses
      (deployment_id>repository_deployments); discussion_comments (discussion_id>discussions);
      discussion_labels (discussion_id>discussions); fine_grained_token_repositories
      (token_id>fine_grained_tokens); fine_grained_tokens (token_id>personal_access_tokens);
      github_app_callbacks (app_id>github_apps); github_app_deliveries
      (webhook_id>github_app_hooks); github_app_hooks (app_id>github_apps);
      github_app_repositories (installation_id>github_app_installations); github_app_tokens
      (token_id>personal_access_tokens); issue_assignees (issue_id>issues);
      issue_attribute_options (attribute_id>issue_attributes); issue_comment_pins
      (issue_id>issues); issue_comments (issue_id>issues); issue_duplicates (issue_id>issues);
      issue_field_values (issue_id>issues); issue_labels (issue_id>issues); issue_relations
      (source_id>issues); issue_suggestions (issue_id>issues); issue_type_assignments
      (issue_id>issues); notification_thread_subscriptions (thread_id>notification_threads);
      oauth_app_tokens (token_id>personal_access_tokens); package_blob_grants
      (package_id>packages); package_files (package_id>packages); package_registry_tokens
      (token_id>personal_access_tokens); package_tags (package_id>packages); package_uploads
      (package_id>packages); package_version_blobs (version_id>package_versions);
      package_versions (package_id>packages); pr_assignees (pull_request_id>pull_requests);
      pr_comment_pins (pull_request_id>pull_requests); pr_comments
      (pull_request_id>pull_requests); pr_labels (pull_request_id>pull_requests); pr_reviewers
      (pull_request_id>pull_requests); pr_reviews (pull_request_id>pull_requests);
      pr_team_reviewers (pull_request_id>pull_requests); project_fields (project_id>projects);
      project_teams (project_id>projects); project_views (project_id>projects);
      pull_request_auto_merges (pull_request_id>pull_requests); pull_request_viewed_files
      (pull_request_id>pull_requests); release_assets (release_id>releases); release_discussions
      (release_id>releases); repository_ruleset_versions (ruleset_id>repository_rulesets);
      repository_security_replies (report_id>repository_security_reports); team_ancestors
      (team_id>teams); team_invitations (team_id>teams); team_memberships (team_id>teams);
      webhook_deliveries (webhook_id>webhooks).
    - **Cell (never moves), 43:** account_moves; actions_oidc_key; actions_settings; anonymous_sessions;
      auth_login_limits; cell_outbox; cell_prefixes; cell_region_policy; cell_request_counts;
      cluster_leases; cluster_scaling_policies; cluster_ssh_host_key; download_rate_limit;
      email_outbox; email_settings; file_reason_limits; github_api_limits;
      graphql_rate_limits; identity_heartbeat; issue_attribute_guard; issue_relation_guard;
      join_tokens;
      node_configuration; node_drain_repositories; nodes; notification_events; oauth_states;
      pending_object_deletions; repo_language_cache; repo_storage_history;
      repository_committed_refs; repository_move_nodes; repository_moves; repository_replicas;
      repository_voters; repository_write_control; repository_write_refs;
      repository_write_votes; repository_writes; signup_requests; storage_object_copies;
      upload_rate_limit; vulnerability_settings.
    - **Shared (copied when a moved row points at it), 12:** attestation_bundles;
      check_run_identities; files; gist_identifiers; global_advisories; organization_roles;
      osv_advisories; osv_packages; package_blobs; signature_verifications; storage_objects;
      topics.

    Stage 6 items 2 to 6, decided in restart 10: (a) every foreign key is `DEFERRABLE
    INITIALLY IMMEDIATE` in both baselines, and the move loads the target in one transaction
    with `SET CONSTRAINTS ALL DEFERRED`; SQLite never moves accounts. (b) Every trigger has a
    decision, below. (c) One object store per region with globally unique ids, so a move
    inside a region copies no file bytes; a move between regions needs a bounded, resumable
    object copy (not written). Git moves with a fetch of every ref, compared ref by ref
    before the switch. (d) Drain before the switch: running CI is asked to stop at the
    begin and must finish or record cancellation before freezing; queued runs are cancelled
    with a clear status in the target. The account's outgoing notices
    must be delivered before the copy, runners register again. (e) An `account_moves` row
    per moving account; the `account_move_guard` trigger on every owned table refuses
    writes (SQLSTATE 55000) while the account is frozen or switched, inside the writing
    transaction, so background workers are refused too. A freeze lifts by itself when it
    runs out, and every step can run again after a crash.

    Trigger decisions. The move sets `codeyard.move` to `load` in the target and to
    `release` in the source, and these 27 trigger functions return at once then:
    - counters (`update_repo_stars_count`, `update_repo_watchers_count`,
      `update_repo_forks_count`, `record_repository_star_day`): copied as values; stars and
      watchers are counted again in the target and the counted value kept; forks stay as
      copied, since forks in other cells are counted from stubs;
    - search events (`queue_search_change`): rebuilt in the target, which queues each moved
      repository, issue and pull request;
    - byte cleanup (`actions_blob_delete`, `actions_run_delete`, `actions_github_tokens_delete`,
      `ci_log_chunk_cleanup`, `codeql_database_cleanup`, `repository_exports_cleanup`):
      skipped, so the release deletes rows but no bytes the target now uses;
    - timestamps and history (`touch_label_timestamp`, `touch_milestone_updated_at`,
      `record_commit_status_history`, `record_upload_confirmation`,
      `immutable_release_capture_names`, `advance_merge_policy_epoch`): skipped, the rows
      carry their values;
    - derived rows (`refresh_team_ancestors`, `narrow_descendant_fork_visibility`,
      `dependency_security_default`, `dependency_security_reauthorize`,
      `issue_suggestion_definition_changed`, `issue_suggestion_target_deleted`,
      `register_repository_topic`, `repository_vulnerability_job`,
      `delete_commit_comment_reactions`): skipped, the derived rows are copied as rows;
    - cross-cell notices (`queue_remote_pointer`): skipped; membership and pointer notices
      are not sent again.
    Guards keep running: `cell_credential_home`, `cell_stub_credentials`,
    `actions_bot_identity`, `actions_bot_session`, `github_app_bot_identity`,
    `github_app_bot_session`, `guard_primary_email`, `immutable_release_guard_owner_name`,
    `notifications_thread_scope`, `notification_threads_provenance` and
    `enforce_repository_visibility_floor`. The account row loads first so the credential
    guards see a local owner.

    Stage 6 status. Written in `backend/db/placement/src/moves.rs` and `codeyard cell move`:
    - The manual move of a user or organization with its repositories between PostgreSQL
      cells: begin (move row, CI asked to stop, older transactions drained), freeze, Git
      copy, row copy with stubs and the shared rows they need, ref comparison, switch,
      directory `move_home` for the account and each repository, a wait for the routers'
      route cache, release (rows deleted, stubs of the target kept, source Git directories
      removed). A failed step before the switch cancels the move and removes the target
      copy; every later step repeats safely. Tests: CLI failure and retry, a directory that
      refuses or loses the switch, Git clone, stable issue, pull and node IDs, HTTP freeze
      and cancel, a thousand-member organization with a revoked member, a racing writer,
      freeze expiry, CI drain and package bytes in a shared store.
    - Repository transfer to an owner in another cell (`--kind repository --name
      owner/name --new-owner login`). Only that repository freezes, and issue and pull
      numbers stay. The former owner's grants (an organization's collaborators and teams,
      runner groups, organization values, security configuration) are dropped, as in a
      transfer inside one cell. The source keeps a stub and a `repository_redirects` row,
      so the old REST and Git HTTP URLs answer 301, or 307 for writes. Limits: no redirect
      for private repositories, GraphQL or SSH; renames and transfers inside one cell
      record no redirect; archived repositories are refused; the CLI has no acting user,
      so the new owner's default grants are not applied.
    - Tokens after a move. Before the switch the CLI publishes the hash of each live token
      of a moving user (at most 10,000) as a directory `credential` key. Routers and other
      cells send such a token to the user's current home, and the source lists the moved
      hashes in `moved_credentials` and asks the new home about them. Browser sessions do
      not move; the user signs in again. A token deleted after the move keeps its key,
      which only routes: the home refuses the token. When the directory cannot be asked,
      a router sends the token to its issuing cell.
    Account and repository moves have database and public two-cell client coverage.
    Regional restart acceptance still needs separate regional PostgreSQL and storage targets.
    - Repositories on cluster storage nodes. The command copies each from its placement
      primary at the current write term; a quorum repository must have no write in
      progress and the copy must hold exactly its committed refs. A target in cluster mode
      receives them on one of its storage nodes through `--target-state`. Release deletes
      the source's write journal, voter and replica rows.
    - Stored files between regions (one S3 store per region). Before the switch the
      command copies avatars, release assets, Actions artifacts, CI logs, CodeQL databases
      and package blobs a page at a time, reads each back and checks size and SHA-256; a
      rerun skips what the target holds. After the switch the source copies are queued
      for deletion after the restore window. Limits: objects over 100 MiB fail the copy,
      Markdown attachments and repository exports are not copied, and a cancelled move
      leaves the copied bytes in the target store.
    - Automatic user moves by the "Regions" policy. Only requests the user authenticated
      count; anonymous reads never move anyone. Pinned users (`codeyard cell pin`) and
      users in the cooldown never move; dry run only audits; the account-move lease keeps
      one move per cell; pause and the daily limit hold. An unfinished move continues at
      the next check. Limits: the daily limit counts this cell's moves to a region, not a
      global region pair; SQLite installations never move; the admin form for the move
      settings has no HTTP test.
    Job 7944965896f34ffb83da9f93d5500b49 passes the 14 move and foreign-authorization
    database tests, the library tests (13, 31, 11 and 22) and two rounds of the 90 cells
    and quorum cluster tests. A move now refuses to start while the directory cannot be
    asked for the target's region.

    Stage 7 side lane:
    - Notice withdrawal and live source checks pass runtime
      cfc7a969e5824a269d9273e69619239c: 54 library tests, both stub HTTP contracts,
      all 239 cluster tests, 47 notification collaboration tests and all 5 polling
      tests, without ignores. Both baselines add separate notice-refresh trigger/index
      blocks. Tests cover privacy, deletion, membership revocation and source outage.
      The earlier public cells notice flow passed, but unrelated maintainer-PR and
      dependency-graph failures prevented an overall e2e pass; no full pass is claimed.
    - Home credential answers now select the credential and cell-local heartbeat in
      one snapshot on the primary. Operation-owned proofs replace the shared 60-second
      cache; both baselines replace `foreign_credentials` with `identity_heartbeat`.
      The only ownership-catalog changes remove the former Cell rule and add the latter.
      `IDENTITY_MAXIMUM_AGE_MS` defaults to 2000, including the 250 ms clock allowance.
      No identity-replica routing is enabled. Tests cover stale/revoked snapshots,
      isolated operations, home loss and PostgreSQL lock-wait expiry. A native database
      operation that outlives its proof first asks the home once more; a refusal stays
      denied and the response is 503. Spawned follow-up work keeps the operation's proofs
      with their original deadlines. A foreign request whose home is down answers 503,
      so the cells acceptance run expects a refused star there instead of a queued one.
    - `backend/cache` owns the bounded local LRU, optional Redis standalone/Cluster,
      slot batches, deadlines, breaker and single-flight. Markdown authorizes before
      lookup; no access decision uses the cache. Compose Valkey is off by default.
      `scripts/with-valkey.sh` runs the remote contracts against disposable standalone
      and three-shard Cluster servers whose ACL allows only the cache's commands on `cy:*`
      keys. TLS is not covered.
    Merged into `cluster/full` on 2026-11-08. The merge keeps moved-token routing
    (`moved_credentials`) beside the operation-owned proofs and drops
    `foreign_credentials`. The token heartbeat snapshot no longer requires the issuing
    prefix, so a moved token is answered at its new home, and PostgreSQL
    `refresh_remote_notices` skips while a move loads or releases rows. Fixes from the
    merge runs:
    - A foreign proof is asked for again in the second half of its life, not after it
      ran out, so it cannot expire between the renewal and the transaction's own check
      after lock waits. This was the intermittent 503 in `cells_authorization.rs`. A
      refused proof is denied at once; when the home cannot answer, the old proof keeps
      its own deadline.
    - Quorum write packs leave out the history the voter holds: voters send their ref
      tips, as a fetch does. Packs with full history made voters miss the 10-second
      write deadline after the 40 MiB storage-loss fixture (the cluster e2e 503 on a
      `gh api` ref create after a restarted primary rejoined), and a repository over
      the 128 MiB pack limit could not be written at all.
    - A foreign pull head is read from its home again at each admission check inside
      the creation transaction, as fork creation does, so a source revoked meanwhile
      rolls the pull back (`cell_forks` on both engines).
    - A home checks its own identity answer on its own clocks, counted from the
      request's start. Comparing the database's heartbeat time with the node's wall
      clock refused every fork and pull creation on a node whose clock is off from the
      database's (the cluster e2e runs web2 five minutes ahead). A verifier in another
      cell still compares the heartbeat with its own clock, so cells need synchronized
      clocks.
    - The debug quorum e2e round runs with the cluster round's 32 MiB worker stack; its
      primary overflowed the default 2 MiB while merging a pull request.
    Stage 7 implementation: (2) packing is a geometric repack with a
    multi-pack index, a bitmap (not for pool forks) and a split commit-graph instead of
    `git gc` (`backend/git/src/maintenance.rs`). (3) Shared PostgreSQL search uses `simple`
    word vectors and GIN indexes for descriptions, issue/pull titles, bodies and comments,
    ordered by rank then stable IDs (explicit sort is secondary). Names, paths and file text
    retain `pg_trgm` substring matching; SQLite local search is unchanged. The newest 100
    comments contribute at most 1,024 characters each; pending review comments are excluded.
    Comment edits, deletion and review publication queue reindexing. Parsed input is bounded
    by directory config; totals and pages stop at 1,000. File text stays deduplicated in
    `search_blobs`. Ranked transport, directory database and search contract tests cover
    these rules.
    (1, part) `DATABASE_DIRECT_URL` carries the serving, maintenance and
    package-collection leases and migrations past a transaction-pooling PgBouncer, and the
    PgBouncer e2e round sets it. Not written: keyset paging (the GitHub API pages by
    number, and OFFSET is already capped at 1,000); read routing from the session's last
    write position (token
    clients carry no position, so a primary-free check needs shared state such as the
    optional cache). Items 4 and 5 are the side lane above.
    `scripts/e2e.sh` runs individual rounds selected through `E2E_DATABASES`. PostgreSQL
    rounds use the owned database supplied by `scripts/builder-test.sh`; completed host
    CI requires a delegated cgroup parent. The fixtures retain the debug worker stack
    budget, enable private dependency discovery explicitly, and wait for current voters
    before the next fault. Current runtime results are listed under
    [Where we are](#where-we-are).

    Owner follow-ups, outside this acceptance work:
    committed range renewal; scheduled bounded expired directory-claim collection.

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

## Cells plan

Two findings set the order of work:

- **Durability comes before cells.** Git processes set `core.fsync=all` and
  `core.fsyncMethod=batch`, libgit2 fsync is enabled, and cluster plan items 3 and 11
  provide quorum publication, copies, backups and fenced restore. Stage 0 extends
  acceptance to three voters and every ref publisher. Shared bounded barriers sync
  objects before refs and their directory entries. The single-node TCG fixture passes
  six guest hard resets across SQLite and PostgreSQL.
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
  passes six single-node TCG guest hard resets, including nested refs and symbolic HEAD.
  Operations are described in
  [Storage architecture](STORAGE_ARCHITECTURE.md#backups).
- **Cluster part:** fixed-voter ref quorum, PostgreSQL arbitration and
  ref journals, snapshot backups and fenced offline restore are implemented. Operators
  provide PostgreSQL HA. Three-voter process-loss and single-node VM hard-reset
  acceptance pass; whole-host power loss and deployment-specific measured limits
  remain unverified;
  [Where we are](#where-we-are) is the current acceptance list.

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
and merges after immediate primary loss, and majority-loss refusal. The single-node
VM hard-reset check passes on both database engines; it does not simulate simultaneous
power loss across the voting nodes.

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

A cell is a complete installation as in [Architecture](ARCHITECTURE.md#clusters), sharing nothing with other cells; an
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
- Packing is a geometric repack with a multi-pack index and split commit-graph, and never
  prunes; object pools repack in the background and keep every object a member uses.
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
- Each shard holds its keys and its own copy of the objects they belong to; the copy in the
  object's own shard is its home row. So a shard moves alone: writes to it are refused while
  it is copied, lookups keep reading the old cluster until the map switches, and the old
  copy goes once those lookups are done. A claim across clusters commits one cluster after
  another; a failure between them leaves pending keys that expire, as a failed signup does.

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
Project fields, items and drafts use the project account's root, including items linked
from another owner's repository. Issue type and field definitions use their account root;
field values, workflows, environments, gists and review requests use their repository root.
GraphQL `databaseId` is null when a scoped ID exceeds its 32-bit field; schema-supported
`fullDatabaseId` fields return the complete decimal ID. Internal lookups keep the full ID.
Computed project fields carry their owning account and project in a bounded composite key.
Review requests have stable row identities; user and team requests retain their separate
uniqueness constraints. A stored protection rule retains its App owner's ID after App removal.
Recorded issue-transfer redirects also validate former roots for issues, comments and
their events and reactions. Every read and write still checks the current repository.

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
- A cell given a foreign credential asks the home primary for the account, scopes and
  CSRF token on each admission. The home selects the credential and its cell-local
  heartbeat in one snapshot. An answer belongs only to the current operation; there is
  no shared foreign-credential cache. The published age bound defaults to 2 seconds,
  including a 250 ms clock-error allowance. Wall and monotonic deadlines are checked
  after waits; an expired or unavailable proof grants nothing. Home reads currently
  stay on primary, not an identity replica. This replacement passes source checks;
  runtime acceptance is pending in [Where we are](#where-we-are).
- Permission checks stay in the resource's cell, where members, teams and collaborators
  are rows.
- A cross-cell pull request lives in the base repository's cell, which fetches the head
  into `refs/pull/N/head` on each push event from the fork's cell. A maintainer's push to
  the fork goes to the fork's cell, which asks the base cell for permission.
- A mention resolves through the directory and creates a stub. Home cells keep pointers to
  remote memberships for "my orgs" and "my repositories".
  Grant and delete triggers queue pointer events in the granting transaction. The home
  applies ordered events idempotently after checking the live sending key and directory
  ownership. Pointers discover cells only: private REST lists and GraphQL organization lists
  call those cells' existing handlers with the original credential and merge bounded pages. A stale pointer
  never supplies permission; a resource-cell outage gives a service-unavailable error. Native
  browser lists use those cells' authorized page models with the original cookie. One shared
  row/byte budget bounds all merged lists; home credentials are rechecked after remote waits.
- Stars keep their rows and count in the repository's cell. The same transactional
  outbox sends a distinct star pointer to the actor's home; removing a collaborator does
  not remove a star pointer. GitHub and browser star lists merge authorized resource-cell
  pages in star order, including home-cell stars. Anonymous profile reads request public
  repositories only. Duplicate delivery and an older event cannot restore a removed star.

### Global features

Each cell writes an outbox table in the same transaction as the change. A worker delivers
events to other cells, at least once and idempotent by event id; global services read the
outbox by cursor. No message bus until the consumers need one.

Cross-cell mention notices carry public repository metadata only. Visibility, deletion and
direct membership changes reclaim their outbox deliveries to withdraw recipient stubs.
Inbox projections also check the source's current public state over authenticated RPC;
private, deleted or unavailable sources contribute no notice payload or unread badge.
The stub alone never authorizes an inbox read. Source checks, both-engine HTTP contracts,
two-cell privacy/deletion/member-revocation checks and notification polling pass; see
[Where we are](#where-we-are).

| Feature | Where it lives | Fed by | Lag |
|---|---|---|---|
| Notifications, email | inbox in the home cell; the resource's cell finds recipients and sends one batch per home cell | outbox delivery | seconds |
| Issue, PR, repository search | PostgreSQL tables in the directory database, served by a separate `cell directory serve`; documents carry their repository, its public flag and readers, and each cell rechecks its own hits against live access | outbox delivery | under a minute |
| Code search | the same index, holding default-branch text files (up to 2,000 files and 16 MiB per repository), rewritten after a push | outbox delivery | under a minute |
| User, label, topic, commit search | the local cell | none | none |
| Activity feed | read from the event index when viewed, no copy per follower | outbox cursor | seconds |
| Explore, trending | the search index's public repository documents, ranked by stars or creation | outbox delivery | under a minute |
| Dependency graph, "used by" | each cell parses manifests; the search index keeps each public repository's packages and answers dependents | outbox delivery | under a minute |
| Security advisories | one dataset copied to every cell and matched locally | pull | minutes |
| Stars, follows | rows and count in the target's cell, a pointer in the actor's home cell | outbox delivery | seconds |

### Disposable content cache

`backend/cache` has an always-on bounded local LRU and optional standalone or Cluster
Valkey through the Redis protocol. It is native-only and off remotely by default.
The Markdown API resolves access and references before lookup; keys include the entire
rendering input. No sessions, permissions, locks or rate limits use the cache. Remote
failure is a miss, calls have a 50 ms budget, all values expire, and slot-split batches
retain partial misses in order. Single-flight and per-family recompute limits bound
cold work. [Cache configuration and contracts](../backend/cache/README.md) describe the
opt-in service and the external tests. Source checks and runtime are pending.

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
the way `codeyard db copy` already walks the catalog. `codeyard cell move` freezes the
account (reads work; a per-account trigger refuses writes, also from background workers;
GitHub REST 503 responses include retry guidance and `Retry-After`),
copies its repositories with Git and its rows in one target transaction with deferred
foreign keys, switches the directory, waits out the routers' route cache, and turns the
source rows into stubs (`backend/db/placement/src/moves.rs`). A receiving copy stays
write-frozen until its source has durably switched; retries cannot replace an active
target. The source retains a released move row with bounded cleanup paths until its
Git directories are removed, so a crash after row release resumes cleanup rather than
recopying the destination. The CLI takes the same move lease in both cells and checks
the target before copying Git, so a refused retry preserves its commits as well as its rows. The CLI
retains the existing Git publication leases through the copy and switch
so an earlier admitted push finishes before the snapshot. A source freeze lifts by itself when it
runs out, and a failed move before the switch leaves the source serving. Ids do not change, and the source copy stays until the move is
checked. Why: rebalancing matters only once cells fill unevenly, and logical replication
cannot do it, since row filters see only a table's own columns. Cost: ownership rules for
all tables, kept complete for good, and a freeze of minutes for a normal account, hours
for a very large org. Splitting an account by repository is rejected, since its teams and
permissions would span cells; a single repository transferred to another cell uses the
same rules (stage 3).

Automatic region moves apply only to individual user accounts. Web nodes count requests
per authenticated account, source region (from the router's header) and day, including
API and CI tokens; SSH requests are not counted. Anonymous requests and outside readers
of public repositories never count. Defaults in site settings: a seven-day window, at
least 1,000 counted requests, at least 70% from one other region, held on three
consecutive days. The hourly evaluator moves the account to the least full open cell in
that region using the move above. It reuses scaling-policy database leases, dry run and
pause: one leader, a 30-day account cooldown, one move per cell, a daily cap on this
cell's moves to each region, no moves for pinned accounts, and no evaluation with only
one region. Every move is audited; dry run only logs and audits "would move". All
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
