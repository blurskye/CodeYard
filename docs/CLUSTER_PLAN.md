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
    automated durability acceptance passes. Hard reset is runtime pending, needs a
    disposable VM. Stage 1's 51 PgBouncer/replica/quorum e2e checks pass without skips;
    finished-CI shutdown and capacity measurements need a writable delegated cgroup.
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
    new cell records. Running the previous release beside this one is runtime pending,
    needs the previous release binary. (4) Directory shards move between clusters
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
    `backend/db/core/src/cells/ownership.rs`, and `every_table_has_an_ownership_rule` fails
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

    Stage 6 status. Written in `backend/db/cluster/src/moves.rs` and `codeyard cell move`:
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
    Job df0177cea8d847309db8022760c6f687 passes the 10 move tests, the 11 cells cluster
    tests (with `cells_moves`), the other 189 database tests and the library tests.
    End-to-end rounds on this branch: PgBouncer 17 passed (228a251a), replica 18 (03370c95),
    quorum 20 (9b813575), cells 5 (8f11fc48). Cluster (9bbbc657): 48 passed, 7 skipped
    because the builder has no delegated cgroup v2 for CI runs, and 1 failed: a `gh api` ref
    create got 503 right after a restarted primary rejoined; the same step passed in 695ea386.
    Not written: the bounded, resumable object copy between regions (a move between
    regions copies no package or artifact bytes yet), moves of repositories placed on
    cluster storage nodes (refused before the freeze), and the automatic region move.

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
    Check, Clippy and WASM aa256156a7a8454c97c33fccd79a22c3 pass. Runtime
    bf65e0b8d276407ca766fc295d1cf9b6 passes 255 library tests, the store, both-engine
    identity, Valkey and repository targets, and 238 of 239 cluster tests. Cross-cell pull
    creation (`cells_authorization.rs:1051`) passed in 4255851c2bd643b4bd8b13f8d3a889e2
    but answered 503 here: the row committed and the fork head fetch failed. The cells e2e
    still fails on the maintainer pull head ("Head repository storage is unavailable")
    and the dependency-graph snapshot (422).

    Stage 7, written so far (job 1c765ab87a7d480d8e09d95f28068432 passes maintenance,
    database and the cluster search tests): (2) packing is a geometric repack with a
    multi-pack index, a bitmap (not for pool forks) and a split commit-graph instead of
    `git gc` (`backend/git/src/maintenance.rs`). (3) Shared PostgreSQL search uses `simple`
    word vectors and GIN indexes for descriptions, issue/pull titles, bodies and comments,
    ordered by rank then stable IDs (explicit sort is secondary). Names, paths and file text
    retain `pg_trgm` substring matching; SQLite local search is unchanged. The newest 100
    comments contribute at most 1,024 characters each; pending review comments are excluded.
    Comment edits, deletion and review publication queue reindexing. Parsed input is bounded
    by directory config; totals and pages stop at 1,000. File text stays deduplicated in
    `search_blobs`. Ranking passes workspace `8eaf9092`, Clippy `0b73be53` and WASM
    `f4727b35`. Job `c930fb55` passes the ranked cells transport test, seven directory
    database tests and 15 existing search contracts without skips; library checks continue.
    (1, part) `DATABASE_DIRECT_URL` carries the serving, maintenance and
    package-collection leases and migrations past a transaction-pooling PgBouncer, and the
    PgBouncer e2e round sets it. Not written: keyset paging (the GitHub API pages by
    number, and OFFSET is already capped at 1,000); read routing from the session's last
    write position (token
    clients carry no position, so a primary-free check needs shared state such as the
    optional cache). Items 4 and 5 are the side lane above.
    The e2e run (job 34215ceffd2d4150b4cc419834481210) stopped at its 140-minute cap in
    the PostgreSQL round's `gh auth login` check, which waited for a device code without
    the cell prefix; that check and a profile 503 for signed-out visitors are fixed, and the
    SQLite round then passes 55 checks with 6 skipped (job
    51adf28cea65421eb9fe79b685ad5f44; the builder has no delegated cgroup for CI runs).
    Each remaining round has its own `scripts/test-cell-e2e-<round>.sh` entry point
    with a 25-minute timeout; `scripts/test-cell-e2e.sh` runs all five, or named rounds,
    without stopping at the first failure. Runtime 52f1aef2 finishes without stalls:
    PgBouncer 17, replica 18 and quorum 20 pass without skips. Cluster has 46 passes,
    five failures and five skips: web2 overflows its debug worker stack, CI needs a
    delegated cgroup, and the editor-survival write gets 503. Cells reaches global
    discovery and fails because its private dependency graph was not enabled. The
    fixture now enables it through REST without weakening the privacy assertions, and
    the debug cluster runner uses the integration tests' 32 MiB worker-stack budget.
    Runtime 4edbb000 passes all five cells checks without skips, including discovery,
    search rebuild/outage, cross-cell gh/Git and rollout waves. Cluster repeats its
    failures: node startup cleared the requested stack budget. The fixture now retains
    that explicit setting. Checks 3b3abdba (workspace), 97efb73b (Clippy) and a2761a58
    (WASM) pass. Runtime de1c7469 completes cluster in 435 seconds: 48 passes,
    two failures and six skips, with no stack overflow. Runner drain/resume reaches
    execution but fails without a delegated cgroup. The editor-survival write still
    returns 503 after the interrupted-storage checks; this is not a pass.
    The interrupted-storage fixture now waits for three recovered voters and the
    frontend's failed-dial backoff before starting another publisher, as the other
    fault fixtures already do. Workspace 5d0a566e, Clippy 141a5cf3 and WASM 70f94efc
    pass. Runtime 7363a215 completes in 536 seconds: 49 passes, one failure and six
    skips. Editor, gh branch and gh merge durability now pass; only finished-CI
    drain/resume fails because the builder has no delegated cgroup. No assertion
    was removed or weakened.

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
