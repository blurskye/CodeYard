# Trusted local Actions runner

The server runs repository workflows with `act`, which starts job containers
through a rootless Podman API. That API must be reachable **inside** the server
container. Installing Podman in the container alone is not enough.

**Trusted workflows only.** Access to the Podman socket lets a workflow run code
and mount host paths with your user's privileges. Separate storage is not a
sandbox. Do not expose the socket over TCP and do not run untrusted workflows.

## Start the engine

On a Linux host with Podman, curl and a systemd user session:

```sh
scripts/start-ci-runner.sh
```

This starts the transient user service `codeyard-ci-engine` with a private socket at
`$XDG_RUNTIME_DIR/codeyard-ci/podman.sock`, using its own image storage under
`~/.cache/codeyard-ci/storage`. The script is safe to run again while the service
is active. Run it again after a reboot.

## Configure the server

```sh
export CI_EXECUTION_MODE=podman
export CI_PODMAN_SOCKET="$XDG_RUNTIME_DIR/codeyard-ci/podman.sock"
```

`CI_PODMAN_SOCKET` accepts an absolute socket path or a `unix:///absolute/path`
URL; the worker normalizes either form and gives `act` one `unix://` prefix. Use the
plain path with `compose.yaml`, which mounts it read-only at the same path inside the
container. The path must match on both sides because `act` passes it back to the
host daemon. A read-only mount does not make the Podman API read-only. Recreate the
server container if the daemon restarts and replaces the socket.

In Podman mode the worker runs `act` with one job at a time. Job containers have no CPU or
memory cap unless an administrator sets one under Administration → Actions; a repository
can set lower caps and a shorter timeout in its own settings. Every container of a run carries a per-run label.
After act exits, the worker uses that exact label on the configured Podman socket to
remove containers left by that run; it never prunes or matches container names. Before act starts, the worker writes an ownership record for
the run to `ci-containers/` in the state directory and holds a lock on it while the run
executes. When the server starts, and every 30 seconds after, it removes the containers of
each record whose lock is free, then deletes the record. Containers left by a crashed
server are removed once the server runs again and the Podman socket is reachable. A node
runs up to `CI_MAX_ACTIVE_RUNS` runs at once
(default 4). Workflow container options can override the resource limits. They are
limits, not protection against hostile workflows.

A run waits in the queue until one of the node's slots is free. Repository maintenance
never prunes, so the commit of a queued run stays readable after its branch moves on.

The first run downloads the runner image, normally `ghcr.io/catthehacker/ubuntu:act-latest`.
Later runs use the copy on the node and do not check the registry, so update the image on
purpose when you want a newer one. Pull it through the runner's own engine, because a plain
`podman pull` updates your default image store instead:

```sh
podman --url "unix://$XDG_RUNTIME_DIR/codeyard-ci/podman.sock" pull ghcr.io/catthehacker/ubuntu:act-latest
```

Its tools are not identical to GitHub's hosted runners.

## CI engines and workflow tokens

Administration → Actions chooses the default engine and enables each engine; repositories can
choose an enabled engine or follow the site default. Disabled repository choices fall back to
that default. `codeyard-actions` is the default and is shipped beside the server; override its
path with `CI_ACTIONS_BINARY`. Each job gets a repository-scoped `GITHUB_TOKEN` with its effective
workflow permissions. Fork pull requests are read-only. act shares the intersection of all job
permissions and logs a warning when this restricts a job. Credentials expire one minute after
the run or in-house job ends. REST, GraphQL, and smart HTTP Git enforce the same repository
and resource permissions; a bot's grants from other runs do not widen a job's credential.

## Workflow run history

The GitHub REST API exposes repository/workflow run lists, run documents, and individual
attempts under `/api/v3/repos/{owner}/{repo}/actions`. Workflow-local run numbers are assigned
atomically at admission and passed to both engines. Pull-request re-runs retain their logical
REST run ID and run number while keeping earlier attempts readable. Deleting a run or its
logs refuses any active attempt. Deleted workflow files retain numeric identities for reading
run history; dispatch still requires a real workflow file. Native multi-workflow aggregate
pipelines remain available natively, but do not invent comma-named GitHub workflows; a run
document without a single workflow identity returns a metadata-unavailable error.

The deprecated workflow billing endpoints are absent: these self-hosted engines have no
GitHub-hosted runner billing. Native elapsed times are not fabricated hosted-OS charges.
Custom deployment-protection provider reviews are also unavailable: there is no registered
provider integration, and an owner PAT cannot stand in for a provider's App identity. The
endpoint returns a documented not-found error without creating approvals or deployments.

## Deliberate re-runs

Whole-workflow re-runs use the stored commit, workflow blobs, event and inputs, not today's
branch contents. Native fork/PR approval and execution authority are still checked. New
attempts keep the logical run ID and workflow number; `github.actor` remains the original
actor and `github.triggering_actor` identifies the person requesting the new attempt.

Failed-job and single-job re-runs execute the selected matrix variants and all downstream
jobs. Successful dependencies supply their observed private results and outputs without
being executed or reported as new jobs. Matrix outputs retain the actual aggregation order;
results from a re-executed variant are never borrowed from its old execution. Unknown,
redacted, incompatible or missing source contexts require a whole-workflow re-run instead.
Partial re-runs require the native engine; act does not provide private dependency snapshots.
A changed engine policy cannot silently turn a partial re-run into a whole act execution.

The REST debug flag enables runner/step debug environment flags and native debug logging
(or act's verbose mode). Private replay files are inaccessible to workflow containers. These
are deliberate new attempts, not automatic retries of interrupted external side effects.

## Durable job observations

The native engine reports authenticated job, step, and job-scoped output events. Workflow
stdout is wrapped as log data, never interpreted as a lifecycle message. Private nonces and
runtime generations keep observations scoped to the current worker claim; the database-free
worker uses the same protocol and authorization as an in-process worker.

The Actions job endpoints return actual observed executions and steps, including retained
attempts. Queued plans without a real start time are not presented as executed jobs. Older
executions and act runs without job telemetry return an explicit metadata-unavailable error,
not an invented empty execution history. Interrupted observed jobs close without a fabricated
successful conclusion. Private replay snapshots exclude masked outputs and are never API
payloads. Runner binaries must be rebuilt together with the server when this protocol changes.

Run/attempt log downloads are real ZIP archives; job/step downloads are plain text. Redirects
grant one minute of access to a bounded log snapshot and its observed file manifest. Only the
capability hash is stored. Expiry, run deletion, and log deletion invalidate downloads, and
later output cannot appear in an already issued snapshot.

## Job identities and log commands

Jobs granting `permissions: {id-token: write}` can use `actions/core.getIDToken()`.
The issuer is `PUBLIC_URL`; its discovery document is `/.well-known/openid-configuration`
and its keys are at `/.well-known/jwks`. The issuer generates its RS256 key automatically
and stores it in the database. Tokens last five minutes; completed or cancelled jobs cannot
request new ones. Fork runs cannot request tokens. Subject customization uses the repository
and organization Actions settings. act requires every job to grant the permission, and supports
OIDC only for runs of one non-reusable workflow without job environments, since it shares one
credential across jobs.

The in-house engine supports problem matcher JSON, including multiline loops, and shows
matcher diagnostics and explicit error/warning/notice commands as annotations. act 0.2.89
only logs matcher registration; it does not execute matchers. Both engines' log groups
fold on the run page without JavaScript. macOS jobs and GitHub-hosted services remain out of scope.

## Caches and artifacts

`actions/cache@v4`, `actions/upload-artifact@v4` and `actions/download-artifact@v4`
use Codeyard's `/twirp` Results endpoints and object store. In Administration → Actions,
set the server URL job containers can reach (an HTTP or HTTPS origin, default `PUBLIC_URL`).
Workers receive a run-scoped bearer token, valid through five minutes after completion,
plus `ACTIONS_RESULTS_URL` and `ACTIONS_CACHE_SERVICE_V2=True`.

Caches default to 10 GiB per repository and expire after seven days without use;
least-recently-used entries are evicted first. Organization owners and admins can lower
organization ceilings through the GitHub cache-policy endpoints. The runtime applies the
minimum of the site, organization, and repository limits. Organization reductions evict
old caches or shorten expiry atomically without rewriting repository overrides; repository
settings show the effective limits. Lookups use the run's branch, then the default branch. Pull request caches
have their own scope; fork runs can restore caches but cannot publish base-repository caches.
Artifacts default to 90 days of retention, configurable on the admin page. A workflow can
request shorter retention. Download artifact ZIPs or delete them with repository write access
from the run page. Block requests are limited to 100 MiB, single-shot PUTs to 128 MiB,
and each artifact to 10 GiB.
Expired and deleted objects are removed by the existing upload sweep; physical deletion
waits an hour to cover in-flight uploads. The Results protocol is run-scoped; repository-scoped
GitHub REST artifact and cache APIs are served at `/api/v3`.

## Code scanning with open scanners

Save this as `.github/workflows/security.yml`. Both engines accept `security-events: write`;
keep permissions identical across jobs when using act. SARIF uploads are processed in the
background and appear under **Security > Code scanning**. Default CodeQL setup is not available;
use an open scanner or upload your own CodeQL SARIF. Fork jobs cannot upload with read-only tokens.
The upload action also needs `actions: read` to identify its workflow. Its private
`PUT .../code-scanning/analysis` transport shares the durable REST SARIF processor; no
GitHub CodeQL service or proprietary feature-flag service is emulated. Upload one SARIF file
per step, as below. Workflow-run metadata comes from native CI records, each run attempt has
its own check suite, and the upload action uses the stored workflow path.

Uploads allow 10 MiB compressed and 100 MiB expanded SARIF, with up to 20 runs and
25,000 results per run; the highest-priority 5,000 findings are published. Native storage
also bounds projected finding metadata to 100 MiB per upload: tool fields, categories
and URI base IDs to 1 KiB, paths and refs to 4 KiB, checkout/artifact URIs to 16 KiB,
and individual result messages/rule metadata to 1 MiB.
Oversized metadata fails processing rather than producing unbounded API or page payloads.
Results must be inlined and scanner invocations successful: failed/externalized reports do
not masquerade as clean scans or close previously recorded alerts.

```yaml
name: Security
on: [push, workflow_dispatch]
permissions:
  contents: read
  actions: read
  security-events: write
jobs:
  semgrep:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - run: pip install semgrep
      - run: semgrep scan --config auto --sarif --output semgrep.sarif
      - uses: github/codeql-action/upload-sarif@v3
        with:
          sarif_file: semgrep.sarif
          category: semgrep
  trivy:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - uses: aquasecurity/setup-trivy@v0.2.5
      - run: trivy fs --format sarif --output trivy.sarif .
      - uses: github/codeql-action/upload-sarif@v3
        with:
          sarif_file: trivy.sarif
          category: trivy
```

## Scoped runner administration

Organization settings link to **Manage Actions runners and groups**; repository Actions
settings link to **Manage self-hosted runners**. These native forms and the GitHub REST
runner endpoints share the same scoped worker records and cluster pairing path. They do
not provision GitHub-hosted machines or accept GitHub's runner executable. Downloaded
Codeyard executables need the existing engine dependencies described above, including
`codeyard-actions` beside the executable when using that engine.

An organization owner/admin or repository administrator can download a registration
credential, load its `token` into `CLUSTER_JOIN_TOKEN`, and follow the
[runner instructions below](#runner-on-another-machine). Tokens last one hour and admit only `runner` nodes;
they cannot grant web or storage roles. Redeeming a credential rechecks the issuer's
current authority. JIT configuration downloads instead provide `encoded_jit_config`:
load that value into `CODEYARD_RUNNER_JIT_CONFIG`, run `cluster join` with a fresh
state directory, then unset the variable. Keep that directory for this worker's lifetime;
use a new directory and node key for each subsequent JIT worker. Do not set `CLUSTER_JOIN_TOKEN` at the same
time. The configuration sets the node name and a validated relative work folder beneath
`runner-workspaces/` in that state directory, separate from node credentials and settings. Its
worker is revoked after its first claimed run finishes; the daemon cannot claim more
work. Never put either credential in argv or source control.

Groups restrict organization workers to all, private or selected organization
repositories. Public repositories also require explicit public access. Workflow
restrictions use `owner/repository/.github/workflows/file.yml@ref`; a commit SHA pins
an exact revision. Custom labels are case-insensitive literal selectors, not software
installation or container-image configuration. Podman labels still need the normal
`CI_ACT_PLATFORM_MAP` configuration. `self-hosted` is immutable.

Use `runs-on: {group: Builders, labels: [self-hosted, linux]}` to select an organization
group. Scheduling remains whole-workflow: a run must use a single group and the worker
must satisfy the union of its literal labels. Operator-invited, unscoped workers retain
the native global queue; an explicit group excludes those workers. Scope, repository,
workflow and label checks happen on the core before every new scoped claim. Removing
access, moving groups or changing labels conservatively cancels active runs in the
scope. Removing a runner revokes its actual node; a remove-token download authorizes
`POST /api/actions/runners/{id}/remove` with JSON `{"token":"..."}` in that exact scope.
The Podman socket remains trusted host-user execution, not a tenant sandbox.

## Runner on another machine

A `runner` runs Actions jobs on another machine with no database. Single-node mode
allows it; cluster mode is not needed. The server needs PostgreSQL and its node
connection open: start it with `IROH_BIND=0.0.0.0:7444` (publish that UDP port) and a
`NODE_NAME`. Then two commands pair a runner. On the server:

```sh
codeyard cluster invite --roles runner
```

Add `--reusable --expires 30d` to pair several runners with one ticket. On the runner,
keep a persistent state directory for its node key and the server's address. Load the
ticket into `CLUSTER_JOIN_TOKEN` from a secret manager or no-echo prompt, not argv, set
the same CI execution settings as above (no `DATABASE_URL` or `SECRETS_MASTER_KEY`) and
start it:

```sh
export CLUSTER_JOIN_TOKEN
codeyard --state-dir ./runner-state serve --roles runner
```

It joins on its first start and reuses the saved key afterwards, so the variable can be
unset after that. The runner appears on **Administration → Cluster nodes**, where it
can be revoked.

For declarative membership, put the runner's public key with the `runner`
role in the server's `CLUSTER_NODES`, and set `NODE_KEY` plus
`CLUSTER_PEERS=<core-pubkey>@<host:port>,...` on the worker. A reusable
`CLUSTER_JOIN_TOKEN` also pairs the worker on first boot. Tickets and tokens save
only the core public key and address; subsequent boots reuse the saved node key.

A database-free worker opens no HTTP, SSH or inbound iroh service and creates no
SQLite database. It dials cores in order using an ephemeral UDP socket; NAT runners
may need `IROH_RELAY`. The core checks membership and the worker role on every call,
claims runs and decrypts secrets over the encrypted connection. Fork runs receive
no secrets. Repository checkout also uses iroh, from the owning storage node.
Only core nodes need `CI_TRUSTED_ACTOR_IDS` and the secret master key; execution
mode, `act`, concurrency and run timeout are worker settings.

Failed claims retry with backoff. If the core disappears, an executing workflow
finishes or times out; logs that cannot be delivered are dropped. Core nodes fail
runs that stop heartbeating. SIGTERM gives act a bounded graceful stop, force-stops
remaining local processes, removes only that run's labeled Podman containers, and
then records the result. Revoking the worker refuses its next request.

The Podman trust warning still applies: pairing does not sandbox workflows.

## Repository secrets and variables

Repository administrators manage values in **Settings > Secrets and variables > Actions**
or through `/api/v3/repos/{owner}/{repo}/actions/secrets` and `/actions/variables`.
Set `SECRETS_MASTER_KEY` on core nodes to a persistent 32-byte key encoded as 64 hex
characters before saving secrets. Back up this key together with the database; replacing
or losing it makes existing secrets and their API encryption keys unreadable.

Secrets use GitHub's public-key/sealed-box API (including `gh secret set`), are encrypted
at rest, never returned by reads, and masked in both engines' logs. Workflows read
`secrets.NAME` and `vars.NAME`; fork pull requests receive variables but no secrets.
Variables are plain text and readable by repository readers, so do not store passwords
in them. Names are case-insensitive and cannot start with `GITHUB_`.

Organization owners and administrators manage inherited values in the organization's
**Actions secrets and variables** settings. Visibility may cover all owned repositories,
private repositories, or selected owned repositories. Repository values override inherited
organization values; transferring a repository does not retain the former organization's
access grant.

### Environment values

The repository Actions values page has a repository/environment picker and native forms
for creating an environment and managing its values. Names are case-insensitive. The picker
and values use the same environment records as the deployment-environment REST API;
creating a value scope preserves existing protection settings, and deleting an environment
also deletes its values. Each environment allows 100 secrets (1 MiB total) and 100 variables (256 KiB total); individual
values are limited to 48 KiB. Environment values override repository and organization values
only in jobs declaring that environment. They populate `secrets` and `vars`, not the process
environment. Forks receive no environment secrets, including after workflow approval.

Both engines consume a private file bound to admitted job IDs. The packaged
`act 0.2.89-codeyard.1` adds `--environment-values-file`; unmodified upstream act cannot
safely provide this context. Do not merge these values into a workflow-wide secret file.
The existing admission limit of one environment per run still applies; jobs without an
environment receive only repository/organization values. The host-execution trust warning
above also applies: context scoping is not an operating-system sandbox between trusted jobs.
Remote claims retain the existing 8 MiB serialized RPC limit (JSON escaping counts). A claim
that exceeds it fails with a redacted size error rather than remaining leased to a worker
that could not receive it.

## Attestation evidence

The repository attestation REST endpoints store signed Sigstore DSSE bundles and index
SHA-256 subjects. Creation requires repository write authority; job credentials also need
`attestations: write`. Listing and bundle downloads follow repository visibility and token
permissions. Cursor pagination and predicate-type filters apply to the stored evidence.
The API preserves signed payloads but does not issue a cryptographic trust verdict: consumers
must verify signatures, verification material, signer identity and their own trust policy.

## Smoke test

1. Use a test repository and an account that is a site administrator or is listed in
   `CI_TRUSTED_ACTOR_IDS`, with write access to the repository. Other accounts get 403.
2. Copy `examples/workflows/live-ci.yml` to `.github/workflows/live-ci.yml`.
3. Commit and push `main` over SSH, or over smart HTTP with a `write` token.
4. Open **Actions**, choose **Live CI smoke test** and run it.
5. Open the running row. The steps and logs update about every two seconds and
   the run ends with `LIVE_CI_PASSED`.

`scripts/test-actions-live.py` is an older browser script for the same check. It has
not been run against the current interface, so treat its result as unverified:

```sh
uv run --with playwright python scripts/test-actions-live.py \
  --base-url http://localhost:3000 \
  --owner YOUR_TEST_USER --repo YOUR_TEST_REPO \
  --username YOUR_TEST_USER --password-file /path/to/password-file
```

The password file must live outside the repository. The script needs Chromium
installed for Playwright.
