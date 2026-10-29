# Pages, dependency graphs and security advisories

These repository features use the same SQLite/PostgreSQL records from native pages
and the GitHub-compatible `/api/v3` adapters. Their tables are part of the `0001`
baseline schema.

## Pages is a control plane, not a website host

**This server does not publish or serve Pages websites.** No Pages publisher,
isolated site origin, certificate issuer or OIDC verifier is configured.

The twelve Pages operations persist settings and expose build/deployment attempt
history. A branch build request records an `errored` build with an explicit
publisher-unavailable message. A workflow deployment request records a terminal
`deployment_failed` attempt, never a successful or unverified queued deployment.
Its `page_url` is the intended target, not evidence that files were published.
The site's `html_url` is not advertised as a hosted site. OIDC credentials are
required by the request shape but are neither stored nor trusted; artifact URLs
are not fetched. A supplied local artifact ID must identify a finalized,
unexpired artifact in the same repository.

- Native settings: `/{owner}/{repo}/settings/pages`, linked from repository settings.
- REST settings: `/repos/{owner}/{repo}/pages` (`GET`, `POST`, `PUT`, `DELETE`).
- Builds: `/pages/builds` (`GET`, `POST`), `/pages/builds/latest` and
  `/pages/builds/{build_id}` (`GET`).
- Deployments: `/pages/deployments` (`POST`),
  `/pages/deployments/{pages_deployment_id}` (`GET`) and its `/cancel` (`POST`).
- DNS health: `/pages/health` (`GET`). It performs bounded DNS resolution only;
  it does not make caller-directed HTTP/HTTPS requests or claim domain ownership.

Settings and deletion require current repository administration. Build/deployment
requests and cancellation require current write authority. All reads follow the
repository's visibility. Source branches and source paths are validated; supported
paths are `/` and `/docs`. CNAMEs are validated, normalized and unique. Terminal
deployments cannot be cancelled; cancelling an active attempt is idempotent.
Deleting settings retains the repository's attempt audit history.

A future publisher must provide isolated hosting, artifact validation, verified
OIDC claims, environment/deployment policy and truthful state transitions before
any attempt can succeed. Failed, unverified attempts must not be repurposed as
publishable jobs.

## Pinned dependency inventories and SPDX

The five dependency-graph operations compare `BASE...HEAD`, export SPDX, generate
and fetch a pinned report, and accept dependency snapshots. Git reads resolve a
commit before reading its manifest and lockfile; moving a branch cannot change an
already generated report. Private repository reports always require repository
read access, including at their download URL.

The inventory reuses the npm lockfile parser from the repository dependency-scan
lane. It supports root `package.json` plus npm lockfile versions 1–3 and registry
packages; unsupported sources produce an error rather than an incomplete success.
Other ecosystems can supply validated dependency snapshots at an existing commit.
Snapshots are bounded to 4 MiB, 100 manifests and 2,000 dependency nodes, with
validated dependency references. Replacement is keyed by repository, commit, ref,
detector and job correlator. Inventories are bounded to 10,000 packages.

Generated reports expire after one day, with at most 100 retained reports per
repository. `HEAD` does not create a report. Fetch redirects to a repository-scoped
report download. SPDX uses `NOASSERTION` for unobserved licensing and download
metadata. Dependency comparison currently does not enrich results with vulnerability
scan findings; empty vulnerability arrays are not a clean-security attestation.

Native inventory and download:
`/{owner}/{repo}/dependency-graph` and `/dependency-graph/sbom`, also linked from
Insights and Security. The native download is raw SPDX JSON; the REST export keeps
GitHub's `sbom` response envelope.

## Local Dependabot alerts

`GET /api/v3/advisories` and `/advisories/{ghsa_id}` expose imported advisories and
published advisories from public repositories; drafts and private publications stay
out of the global catalogue. Instance administrators can create or replace a local
advisory with `POST /api/v3/advisories` using GitHub's global advisory JSON shape.
Omit `ghsa_id` to create a local identifier. Advisory reads never contact a feed.

Site administrators can enable OSV ecosystems and configure the mirror base URL at
`/admin/settings`. Nothing downloads until ecosystems are enabled. The mirror serves
`<ecosystem>/all.zip` for npm, PyPI, crates.io, Go, Maven, RubyGems, Packagist, NuGet,
Pub and Hex. Bounded synchronization runs every six hours, accepts only newer
`modified` records and updates the same global catalogue, including withdrawals.
Records without a GHSA alias receive a stable local catalogue identifier.

Repository administrators use `/{owner}/{repo}/security/dependabot` to list alerts,
dismiss or reopen them with plain forms, and import advisory JSON if they are also
instance administrators. REST offers repository/organization lists and single-alert
reads and updates. Default-branch publications queue lockfile parsing into saved
dependency snapshots; a background worker also scans every five minutes. Matches
use each detector/correlator's latest snapshot on the default branch. Reads and
background scans reconcile matches with stable repository-local alert
numbers; upgrades close open findings and a vulnerable version returning reopens
fixed findings. Withdrawals close open findings. Dismissals retain their reason,
comment, actor and time. Explicitly disabling Dependabot alerts in a repository's
security configuration hides its alert list and refuses updates. No fix pull
requests are opened.

Admin-imported advisories match concrete SemVer versions and comma-separated
comparison ranges (including `||` alternatives). OSV advisories use ecosystem-aware
version ordering and OSV intervals, including prereleases and exact affected
versions; unsupported versions and Git ranges are not guessed.
EPSS, assignment, runtime-risk and artifact-registry query controls are not supported
and are rejected, rather than silently ignored. Enterprise Dependabot operations
return 404 because Codeyard has no enterprise tenancy.

## Confidential disclosure and advisories

Native Security provides private report threads and advisory review:

- `/{owner}/{repo}/security` and `/security/reports/{report_id}`;
- `/{owner}/{repo}/security/advisories` and `/security/advisories/{ghsa_id}`.

Repository administrators can enable private reporting on a **public** repository.
A signed-in reporter can then create a confidential report; reports are separate
from ordinary issues and public activity. Both native and REST creation atomically
create the private thread and a `triage` advisory. The reporter and current
administrators can read it, subject to current repository/account authority. Five
reports per reporter/repository per day are accepted. Disabling reporting does not
delete earlier reports or threads.

The eight advisory operations list repository/organization advisories, create and
read advisories, update them, accept private reports, record CVE requests and create
private remediation forks. Organization lists filter each repository and each
advisory by the caller's live access; organization names confer no extra access.
Drafts and triage remain confidential. Publication is explicit and cannot silently
be reversed into a draft. Publishing within a private repository does not make its
contents public. Native forms use CSRF tokens and immutable repository identities.
Reviewers can add missing affected-package metadata before publication. Existing
vulnerability lists and detailed ranges/patch versions can be replaced through REST;
the native editor preserves them when saving other advisory fields.

CVE requests are **durable local requests for administrator follow-up**, not
submissions to a CVE registrar. No CVE is fabricated or assigned by this server.
An externally assigned identifier can be recorded through advisory updates.
CVSS 3.0/3.1 base vectors are scored; unsupported metric sets are rejected. Nonempty
advisory collaboration-user/team changes are rejected rather than pretending to
grant permissions. Unobserved CWE names and detailed credit enrichment are not
invented.

Remediation forks contain real Git objects and are kept private with a locked
visibility floor. They reuse the checked repository fork lifecycle and storage
placement. The current normal-fork limit permits one personal fork per user/fork
network; another advisory fork in that network may therefore conflict. There is no
automatic fork expiry or external CVE-registration worker.
