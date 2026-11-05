# CI API (Actions)

Workflows in `.github/workflows/` run with `codeyard-actions` (the default) or `act` on nodes that have the
`runner` role. Runs, jobs and log metadata use the selected database
(PostgreSQL or SQLite); compressed log bytes use the object store.
Code: `backend/ci-engine/` (admission, queue and workers), `backend/ci/` (configuration), `backend/api/rest/src/ci/` (JSON and Actions page/native-form adapters).
The native CI and GitHub Actions adapters register their own routes. `CiRuns` owns admission and
control; separate runtime-owned `CiWorkers` owns execution/stale-sweep tasks.

## Concurrency and timing

Workflow-level `concurrency` groups are repository-scoped and case-insensitive. One run
holds the group and one waits; a newer waiting run replaces the previous waiter.
`cancel-in-progress` also asks the holder to stop, and its replacement waits for that
worker to finish. Group expressions use `github`, typed `inputs`, and `vars`. A grouped
run must contain one workflow. Job-level `concurrency` follows the same rules per job:
the runner reports the job's group before it starts and waits until the server grants it,
and a newer job's `cancel-in-progress` cancels the holder. Job groups queue only against
other job groups.
The run page shows its group and whether it is waiting; the three REST concurrency-group
operations expose active leases and retained run configuration.

The REST workflow and run timing endpoints use recorded job milliseconds, including
re-runs, without rounding or OS multipliers. Workflow totals cover the current UTC
calendar month. `UBUNTU`, `WINDOWS`, and `MACOS` describe standard runner labels, not
charges; explicitly self-hosted or unrecognized labels do not enter `billable`.

An organization can limit Actions to all, none or selected repositories
(`/orgs/{org}/actions/permissions`); runs in other repositories are refused at
admission. The self-hosted runner switch works the same way: registered runners skip
runs from repositories it leaves out, while the server's own workers still take them.
Custom deployment protection rules wait for their GitHub App, which decides through
`POST /repos/{owner}/{repo}/actions/runs/{run_id}/deployment_protection_rule` with its
installation token.

## OpenID Connect

Jobs granting `permissions: {id-token: write}` receive `ACTIONS_ID_TOKEN_REQUEST_URL`
and `ACTIONS_ID_TOKEN_REQUEST_TOKEN`. A bearer-authenticated GET to the URL (optionally
appending `&audience=...`) returns a five-minute RS256 JWT in `value`. The instance base
URL is the issuer; public discovery is at `/.well-known/openid-configuration`. The
signing key persists in the database. Requests are refused after job completion or
run cancellation, and fork jobs do not receive OIDC credentials.

Repository and organization Actions settings customize the subject's ordered claim
keys. Repositories use the default unless explicitly opting in; a blank custom
repository template inherits the organization's. Organization settings can include
existing repository properties as `repo_property_NAME` claims. Missing claims required
by a subject template refuse issuance. Configure cloud trust conditions before changing
a template. Immutable subjects are not supported. With `act`, OIDC requires one workflow,
uniform job permission and no job environments; use the native runner for job identities.

## Starting a run

`POST /api/ci/trigger` requires a live signed-in session cookie. First load the
same-origin `/{owner}/{repo}/actions` page using that session and read its dispatch
form's hidden `csrf_token`. Submit that token in `X-CSRF-Token`, with
`Content-Type: application/json` and the instance's `Origin` header. Keep the same
session cookie; omitting the synchronizer returns 403.

Its JSON body takes:

- `repo_owner`, `repo_name` and `branch` (also accepted as `ref`): the repository and ref
  to run. For `pull_request`, use the base repository and base branch, not the source branch.
- `event`: `push`, `pull_request` or `workflow_dispatch`. Other values return 400.
- `pipeline` (optional, default `ci`): `ci`, `cd` or `full`. Other values return 400.
- `head_sha`: a full 40-character hexadecimal commit ID; required for `pull_request`,
  optional for the other events.
- `workflow_path` and `environment` (optional).
- `inputs` (optional): the values for the workflow's `workflow_dispatch` inputs, keyed by
  input name. Strings, numbers and booleans are accepted.

It returns `{job_id, mode, pipeline, workflow_path}`. A workflow runs only when its
`on:` section declares the event.

### Workflow dispatch inputs

A workflow declares its manual inputs under `on.workflow_dispatch.inputs`, as GitHub does.
Each input has a `type` (`string` by default, `boolean`, `number`, `choice` or
`environment`), an optional `description`, `required`, `default` and, for `choice` and
`environment`, `options`. A workflow may declare at most 25 inputs and a choice at most 25
options; a file that breaks these rules is not listed and cannot run.

A run with inputs takes exactly the declared names. The server refuses unknown names, a
missing or empty required input, a value that is not one of the options, a boolean that is
not `true` or `false`, and a number that is not a finite number. Inputs left out take their
default; without a default, optional booleans become `false`, numbers `0` and strings
`""`. The serialized input payload is limited to 65,535 bytes. The run's inputs are stored with it and shown on its page. A run of several
workflows at once takes no inputs, so a workflow that declares inputs must be started on
its own, with `workflow_path`.

Inside the workflow, `github.event.inputs.NAME` holds the value as text, as on GitHub, and
`inputs.NAME` holds it typed by its declared type.

For example, to run a pull request targeting `main` (replace the SHA with its current
immutable head commit, and choose a workflow that declares `pull_request`):

```json
{
  "repo_owner": "alice",
  "repo_name": "project",
  "branch": "main",
  "event": "pull_request",
  "head_sha": "0123456789abcdef0123456789abcdef01234567",
  "workflow_path": ".github/workflows/ci.yml"
}
```

Omitting the pull request head SHA returns 400. The repository, head SHA and base branch
must match exactly one open, non-draft pull request; no match or multiple matches return
409. Fork approval for that exact head is still required.

A pull request run sees its pull request as `github.event.pull_request` (`number`, `title`,
`head.ref`, `head.sha`, `base.ref`, `base.sha`), and `github.ref` is `refs/pull/<number>/merge`.
Automatic runs retain the webhook's payload and activity. Without `types`,
`pull_request` and `pull_request_target` select `opened`, `synchronize` and `reopened`.
`pull_request_target` uses the base branch's code, secrets and token permissions;
approved fork `pull_request` runs have no secrets and a read-only token.

Starting a CI-only run needs repository Write access; deployment runs need Admin
access. Both also require host-execution approval: the actor must be a site
administrator or be listed in `CI_TRUSTED_ACTOR_IDS`. Anyone else gets 403
"CI host execution is not approved for this actor". Site administrators bypass the
repository permission requirement only on user-owned repositories, not organization
repositories.

When `workflow_path` is supplied with the default `ci` pipeline, its inferred pipeline
can upgrade the request: case-insensitive substring `full` selects `full`; otherwise `cd`, `deploy`
or `release` selects `cd`; all other paths select `ci`. Without a path, the pipeline
selects `CI_WORKFLOW_CI`, `CI_WORKFLOW_CD`, or both for `full`. Admission upgrades `ci`
to `full` if any selected path implies deployment, is `CI_WORKFLOW_CD`, or has a job
`environment`; a nonempty request `environment` also upgrades it. Thus `pipeline: "ci"`
does not guarantee Write access is enough. Both `cd` and `full` are deployment runs;
they default to the `production` environment when no target is supplied. Workflow job
environments must be literal names and agree with the run's target.

A push over SSH or smart HTTP with a `write` token, and the opening or head movement of a
same-repository pull request, also queue runs for matching workflows. Pull request workflows queue once per head
commit automatically. A writer may run them again at that commit: "Run workflows", or Re-run on a finished pull
request run, starts a new attempt once the latest attempt has finished, and returns the attempt that is still
queued or running. Path filtering reads names only, with at most 8,192 paths, 4 KiB per path
and 1 MiB across path names; if a comparison exceeds a bound, its workflows are
skipped. Ref and path globs share one aggregate match-work budget per event;
exhausting it stops workflow admission. Fork pull requests wait for a writer on the
base repository to use "Run workflows" (or
`POST /api/repos/{owner}/{repo}/pulls/{number}/workflows` with `expected_head`). Automatic discovery retains a waiting run for that exact head; workers cannot claim it, and it takes no concurrency lease until approved. The REST run approval endpoint uses the same trusted-writer and head checks. Each new head needs approval. Fork runs never receive repository
or organization secrets, including after their head repository is deleted.

Workflow discovery reads only direct regular `.yml` and `.yaml` files in
`.github/workflows/`. The directory may have at most 64 direct entries, including
non-workflow files and subdirectories. Exceeding that bound fails the entire listing:
`GET /api/ci/workflows` returns 503, and automatic discovery queues no runs for that
revision even if the push was accepted. Each workflow is limited to 256 KiB; discovery
skips an oversized file while still considering the others. Explicitly triggering an
oversized workflow is rejected, not run.

The Actions native dispatch form calls the shared `CiRuns::dispatch_run` operation,
not the JSON handler. It offers supported events and preserves a rejected draft.
Repository workflow buttons dispatch workflows that enable `workflow_dispatch`. The
repository's "Run workflow" menu shows one field per declared input of each such workflow,
and works without JavaScript. A refused run shows the reason but does not keep the typed
input values; start it again from the repository's Actions page.

### Additional triggers

`schedule` accepts five-field POSIX cron expressions in UTC, at least five minutes
apart, and runs the latest default-branch commit. `workflow_run` supports `workflows`,
`types` and base-independent source `branches` filters, with three downstream levels.
`requested`, worker `in_progress` and worker `completed` events run default-branch code;
stale-worker sweeps do not yet emit `completed`. `repository_dispatch` supports `types`
and `POST /api/v3/repos/{owner}/{repo}/dispatches` with `event_type` and `client_payload`.
Live job tokens may dispatch repositories or workflows, retaining the original starter's
host-execution authority. Other job-token events cannot start runs.

## Routes

| Method | Path | Notes |
| --- | --- | --- |
| `GET` | `/api/ci/workflows?owner={owner}&repo={repo}` | Workflows that can be run. Both `owner` and `repo` are required. Optional `git_ref` selects the discovery ref; otherwise the default branch is used. |
| `POST` | `/api/ci/trigger` | Starts a run (see above). |
| `GET` | `/api/ci/jobs` | Runs. `owner` and `repo` select one repository and must be given together; one alone returns 400. Without them the list is the caller's own runs. `limit` is 1 to 200, default 50. Visibility is applied in the database before the limit. |
| `GET` | `/api/ci/jobs/{job_id}` | One run. |

Logs use `GET /api/v3/repos/{owner}/{repo}/actions/runs/{run_id}/logs`, with the
numeric REST run ID; `/runs/{run_id}/attempts/{attempt}/logs` pins an attempt.
Normally this redirects to a short-lived ZIP download. The browser uses its
session cookie on this read-only operation; explicit token
credentials take precedence. Adding `after=0` returns cursor-based JSON with
`logs`, `next_cursor`, `has_more`, run progress and deployment state. Continue with
`after=next_cursor`; `limit` is 1 to 1000, default 500, with a 2 MiB raw log budget
per page. Other REST operations remain token-only. Cancellation uses the native
run page's form or v3's workflow-run cancel operation. The old log, history,
log-download, cancel and deployment routes are removed.

Logs use compressed incremental objects with keys, checksums and line ranges in
`ci_log_chunks`. The local, S3 or storage-node backend stores their bytes; SQL no longer
stores log payloads. The existing 100,000-line and 32 MiB limits and retention settings
still apply. Uploads reserve cleanup before storing bytes and recheck the worker and
active jobs before committing a pointer. A failed upload leaves totals unchanged;
retries use the upload sequence to prevent duplicate lines. Deleting a chunk schedules
its object for deletion after the seven-day restore window.
ZIP and text downloads are generated from those chunks. `CI_LOG_IO_TIMEOUT_SECONDS`
bounds the entire log read or batch write (default 30, range 1–300), including object I/O. A job's final log batch is
stored before success is recorded; upload failure fails the run. A killed runner's
stored chunks remain readable after the stale-worker sweep fails the run. Upgrade
cores and runners together: sequenced log uploads use `codeyard/events/7`.

Run statuses are `queued`, `running`, `success`, `failed` and `cancelled`. `mode` is
`unassigned` until a worker claims the run, and stays `unassigned` if the run never
starts. After a claim it is the claiming worker's execution mode, `local` or `podman`.

Each workflow writes `gitserv/workflow/<full path including extension>`, for example
`gitserv/workflow/.github/workflows/test.yml`. Multi-workflow runs use
`gitserv/pipeline/<pipeline>`. Pending/terminal states and newer-attempt precedence
are unchanged. Protection should name every required full workflow context.
Released `gitserv/<stem>`/`gitserv/<pipeline>` requirements require all matching
sources' latest attempts to succeed; ambiguous stored statuses cannot authorize a
merge. Actions metadata exposes the context.

Errors: 401 for a missing or ended session, and for banned accounts; 403 when a run is
not approved or not permitted; 404 when the repository cannot be read; 422
`legacy_log_too_large`; 428 `password_change_required`; 503 when admission cannot be
checked (`session_unavailable` and similar). Other 500 responses are internal faults.

## How a run executes

- Runs are rows in the `ci_runs` table, and that table is the queue. A node with the
  `runner` role claims queued runs. It executes up to `CI_MAX_ACTIVE_RUNS`
  runs at the same time (default 4).
- Each job runs under the selected engine: in Podman containers by default (`CI_EXECUTION_MODE=podman`),
  or on the host with `CI_EXECUTION_MODE=local`. There is no Kubernetes mode.
- Output is stored as it arrives. Ordinary lines over 16 KiB are omitted with a warning,
  not truncated before secret masking. Authenticated native progress has a separate
  bounded envelope. Log storage/transport failures warn and drop unsent lines rather
  than cancelling execution. The ordinary run deadline still applies.
- A worker heartbeats every 10 seconds. A running run with no heartbeat for 90 seconds
  is marked `failed`. Runs are never retried automatically.
- The effective repository/site Actions timeout stops the run. `CI_RUN_TIMEOUT_SECONDS`
  (default 3600) is the node fallback only when no effective policy timeout is supplied;
  it does not cap an explicitly configured Actions timeout.
- Finishing a run writes its status, deployment record, commit status and, for a failed run,
  the starter's notification in one database transaction.

### Reusable workflows

The default `codeyard-actions` engine runs local `jobs.<id>.uses` workflows and
`owner/repo/.github/workflows/file.yml@ref` calls to this Codeyard server. Admission
pins remote refs and snapshots the complete call tree; workers never fetch called
workflows from moving branches. Calls support typed `workflow_call` inputs,
explicit or inherited secrets, outputs in the caller's `needs`, matrices, and
`caller / called` job names. There are at most four levels including the caller
and twenty distinct called workflows. Called token permissions cannot exceed the
caller's. Called workflows declaring workflow- or job-level concurrency are rejected;
act job-level concurrency is unsupported and refused rather than silently ignored.
Private cross-repository calls require a private caller in the same
owner namespace and the actor's read access, checked again at claim time.

The patched `act` engine uses the same pinned files and inherits secrets from the
immediate caller, preserving its workflow token. act still uses
one token with the intersection of all job permissions, not per-job credentials.
In act 0.2.89, a workflow containing only another workflow call does not propagate
its result or outputs to the outer caller: dependent jobs can be skipped even
when the nested jobs succeeded. Use `codeyard-actions` for those workflows.
act also chooses its own called-job log names instead of the native caller chain.

## Live updates

Hydrated Actions refreshes the authorized page model every two seconds while the
selected run is queued/running; no-JS users have a Refresh link. This is not CI
WebSocket streaming. The inbox separately polls its page model every 15 seconds.

Native steps publish masked `GITHUB_STEP_SUMMARY` Markdown to the run's Summary
view. Persistence is best effort, capped at 64 KiB per step and 256 KiB per attempt;
the page projects the first 128 summaries in execution order. Truncation is logged.
The Markdown uses the normal sanitized renderer and remains available after runner
temporary files are removed.

Worker completion and queued cancellation announce notification IDs after commit;
database-free worker finish goes through core queue RPC. PostgreSQL propagates
hints to other nodes, and socket delivery rereads live authority/content. Hints
can be missed, so they are not a durable delivery guarantee or proof of browser
execution. Runtime explicitly starts/joins the broadcaster listener.

## Cache and artifact services

The server provides the Results API used by cache v2 and artifact v4 actions; the
standalone `codeyard-actions` CLI does not provide these services. Bytes use the
configured object store, so separate web nodes need the same S3 bucket rather than
node-local storage. The Results URL must be reachable from every runner.

The pinned upstream toolkits classify other forge hosts as GHES: cache falls back
to v1 and artifact v4 refuses to run. `E2E_ACTIONS_ONLY=1 bash scripts/e2e.sh` checks
SQLite and PostgreSQL using their supported `.localhost` test origin for workflow
metadata and the real server listener for Results requests. This does not establish
upstream action support for arbitrary production hostnames.

## Known gaps

- The run page has a Cancel run button for queued and running runs, for users with write
  access. The API has the same cancel route.
- Interrupted runs are not automatically retried.
- Finalized, unexpired artifacts can be downloaded from the run page by repository
  readers, or through `/api/v3/repos/{owner}/{repo}/actions/artifacts/{artifact_id}/zip`.
  Artifact deletion requires repository write access and is unavailable when archived.

## Configuration

The settings are in `.env.example`: `CI_EXECUTION_MODE`, `CI_TRUSTED_ACTOR_IDS`,
`CI_MAX_ACTIVE_RUNS`, `CI_RUN_TIMEOUT_SECONDS`, `CI_PODMAN_SOCKET`, `CI_ACT_BINARY`,
`CI_ACT_RUNNER_IMAGE`, `CI_ACT_PLATFORM_MAP`, `CI_WORKFLOW_CI`, `CI_WORKFLOW_CD` and
`NODE_NAME` (the label that CI logs show for the worker).

For running jobs on a machine you control, see [CI local runner](CI_LOCAL_RUNNER.md).
