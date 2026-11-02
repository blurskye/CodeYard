# Codeyard HTTP API

This is a short reference for the JSON API. The router is the authority:
`src/backend/api/mod.rs` composes resource routers under `api/{auth,user,users,
admin,repos,files,ci,notifications,discovery}`. Each resource registers its JSON,
HTML and native-form paths; Git HTTP, health and assets have separate adapters. Routes that are not described here exist too; see [Other routes](#other-routes).
Server-rendered HTML pages are not part of this API.

- Base URL: `http://localhost:3000` by default (`HTTP_ADDR`).
- Version: the crate version. The API is not versioned.
- Bodies are JSON unless noted. Unknown request fields are ignored.
- Timestamps are RFC 3339 with a numeric offset, for example `2026-01-01T00:00:00+00:00`.
- Most lists are not paginated. Repositories, issues, pull requests, releases, commit
  history, forks, stargazers, and the comment and review lists take `page` and `per_page`.
  See [Limits](#limits).

## Authentication

Browser sessions use cookies. Loading a sign-in or setup form creates an anonymous
session; submitting it signs in and rotates `session_id`:

- `HttpOnly`, `SameSite=Lax`, `Path=/`.
- `Max-Age` is `SESSION_DURATION_DAYS` (default 30; values outside 1 to 365 use 30).
- `Secure` when `COOKIE_SECURE` is true or `PUBLIC_URL` is an `https://` address.
  `Domain` is set from `COOKIE_DOMAIN` when it is configured.

The native API accepts session cookies only, not bearer or personal access
tokens. The separate [GitHub REST subset](#github-rest-subset) accepts personal tokens. Git over SSH authenticates with SSH keys; Git over HTTPS uses personal
access tokens as the Basic-auth password (any username).

**CSRF.** Native forms carry `csrf_token`. Cookie-bearing unsafe JSON resource
requests, including repositories, issues, pull requests, profiles, files, notifications
and CI, require `X-CSRF-Token` and same-origin submission through their route layers.
Authentication forms require the page's existing session, synchronizer and same-origin
submission. SameSite is not a substitute for these checks.

**Sessions and accounts**

The legacy `/api/auth/*` account operations are removed. Use the native forms on
`/login`, `/register`, `/setup` and `/logout`; password changes post from settings to
`/settings/password`. Setup creates the first administrator and requires `ADMIN_SECRET`
when configured. Private-node registrations wait for approval. Usernames and organization
names cannot be site pages such as `login`, `settings` or `explore`, in any letter case.
REST clients read the authenticated account through `/api/v3/user` with a personal token.
- `GET /login/oauth/{provider}` and `GET /login/oauth/{provider}/callback` (web routes;
  update provider callback registrations when upgrading):
  sign in with GitHub, Google, GitLab, Discord or Bitbucket, when the provider's
  client ID and secret are configured (see `.env.example`).

Account names have 3 bytes or more, at most 255 characters, Unicode letters and digits,
`-` and `_`. Passwords have 8 to 1024 bytes.

**Public user object** (`PublicUserResponse`, used everywhere else):
`id, username, avatar_url, display_name, bio, location, website, is_admin, created_at`.
`avatar_url` is `/api/files/{id}/download` or `null`.

## Errors

Errors have the body `{"error": "<code>", "message": "<text>"}`.

| Status | Meaning |
| --- | --- |
| 400 | Invalid input, or an operation that cannot be done (for example a branch that already exists). |
| 401 | No session where one is needed, or the account is banned. |
| 403 | The action is not allowed: an archived repository, a protected branch, or a password change is required. |
| 404 | Not found, or not readable by you. A private repository looks the same as a missing one. |
| 408 | A request body did not arrive within 30 seconds. |
| 409 | Conflict: the branch or record changed since you read it (see the `expected_*` fields). |
| 413 | Content is too large. A POST, PUT, PATCH or DELETE body above 4 MiB is refused, except the streamed file upload. |
| 422 | The JSON body does not match the expected types. |
| 428 | Setup or a password change is required first. |
| 429 | Throttled: password attempts or an upload quota. |
| 500 | Internal error. |
| 503 | A service is unavailable, for example saving an Actions secret without `SECRETS_MASTER_KEY`, or the node is busy reading request bodies or serving repository downloads. Retry later. |

Some responses are plain text rather than JSON: Git over HTTP, the CI socket, framework
rejections, and the body limits (413, 408, and the busy 503 of the body gate). Touched
operation failures use safe public messages. A 503
can describe an already-applied effect: saved file/ref with failed PR refresh,
merged branch with failed PR marking, deleted repository row with remaining Git
directory, retained upload reservation, or removed object with remaining metadata.
Read the message and current state before retrying; Git, SQL and object storage do
not share a transaction.

## GitHub REST subset

`/api/v3` accepts `Authorization: token TOKEN` or `Bearer TOKEN` with personal
access tokens. Read tokens cannot mutate anything; write tokens retain the account's
existing repository and branch-protection rules. Browser sessions are not accepted.
Configure `PUBLIC_URL` for response links. `X-GitHub-Api-Version: 2022-11-28` is accepted.
Errors use `{message, documentation_url}`; paginated lists use `page`, `per_page`
(default 30, at most 100), and `Link` headers.

See [GITHUB_API.md](GITHUB_API.md) for generated, operation-level coverage against
the vendored specification. Issue conversations, relationships, attributes, suggestions,
reactions, and all seven search endpoints share the native authorized stores. Private
repositories are excluded from search counts and pages unless the viewer can read them.
Code search requires authentication; other searches permit public anonymous reads.
Creating an issue with `parent_issue_id` requires triage authority in both repositories;
the issue and parent link commit together, including graph validation. Repository-scoped
CI tokens cannot follow or mutate cross-repository relationship or suggestion targets.
Issue-to-pull conversion carries conversation pins, including their author and timestamp.
Issue types, custom-field values, relationships and suggestion history have no PR equivalent;
conversion refuses these cases rather than cascade-deleting their data.

Catalog and issue searches use bounded expressions, bound SQL values, admission limits,
and database deadlines. They support quoted literals, boolean expressions and validated
qualifiers; unsupported qualifiers return 422 rather than being silently ignored. Search
only exposes the first 1,000 results. Repository searches omit forks unless requested.
Topics are local catalog records, visible only while attached to a readable repository.
Their timestamps record catalog creation, not a fabricated historical first use.

Code and commit search scan readable repositories' default branches through native Git
or storage RPC. There is no separate index. Each scan has an eight-second budget, at most
250 repositories, a 512 MiB stored-repository-size admission budget (the first repository
is always admitted), and bounded memory. Code searches keep at most 5,000 matching lines
per term and, when needed, 5,000 enumerated paths; commit walks inspect at most 2,000
commits per repository. Code results group matching lines by file and report real blob
IDs. `indexed` ordering uses the repository update timestamp. Responses identify
live lexical scanning; unavailable or truncated shards set `incomplete_results`, and
`total_count` then counts observed matches, not an invented estimate. Issue semantic or
hybrid requests explicitly advertise lexical fallback when no semantic index exists.

### Organization rulesets

`/api/v3/orgs/{org}/rulesets` supports owner-managed definitions and immutable history.
Organization members can read definitions without bypass actors; rule-suite reads also
require access to the current repository. Lists support GitHub's pagination and filters.
Nonempty rulesets must remain `disabled`: activation and evaluation return 422 until native
ref admission consumes these policies. Security-product bulk changes likewise return 422
because this build has no native scanners; they never report that a job was started.
Owners review fine-grained tokens through `/api/v3/orgs/{org}/personal-access-token-requests`
and `/api/v3/orgs/{org}/personal-access-tokens` (list, approve or deny, revoke). Apps and
fine-grained tokens cannot call them. GitHub App installations and external app properties
are not implemented in this build.

### Teams

`/api/v3/orgs/{org}/teams`, legacy `/api/v3/teams/{id}`, and `/api/v3/user/teams`
share native team membership and repository permissions. Visible teams can be nested:
child members join the parent's member lists and inherit its repository access. Only
organization owners and admins manage teams; a maintainer membership does not grant
organization management authority. Reparenting conservatively cancels active organization
CI runs. Team invitations use the existing organization invitation acceptance page.

### Actions runners

Organization runner groups and organization/repository runner endpoints manage scoped
Codeyard `runner` nodes, not GitHub's runner executable. Organization owners
and admins manage groups and organization runners; repository administrators manage
repository runners. Group repository access, public-repository permission, workflow
restrictions and custom labels are enforced by the native claim transaction. Changes
conservatively cancel running work in that scope. Default groups cannot be deleted.

Registration credentials are real one-hour cluster pairing tickets. JIT configurations
reserve a named, single-run worker and are consumed through `CODEYARD_RUNNER_JIT_CONFIG`;
removal credentials redeem at `POST /api/actions/runners/{id}/remove`. Credentials use
`Cache-Control: no-store` and never appear in page models. Native forms are linked from
organization and repository settings. See [runner setup](CI_LOCAL_RUNNER.md#scoped-runner-administration).
Existing cluster deployment still requires PostgreSQL; the administration data and
contracts support both database engines. Hosted-runner lists are empty. Downloads
advertise the serving Codeyard executable, and version-deprecation dates are null
because Codeyard has no scheduled runner-version retirement policy.

## Users and profiles

`GET /api/users` is not available. Account listings require a site-administrator
session on `/admin/users`; usage charts are on `/admin`. There is no public collection.

The legacy `/api/users/{username}` profile, repositories, activity and follow routes
are removed. Profiles use `/{username}` with `repo_page=N` for repositories and
`page=N` for recent activity; unreadable repositories and activity stay hidden.
Follow and unfollow use the profile's CSRF-protected forms. REST clients use the
v3 user, repository, event and following operations (see [coverage](GITHUB_API.md)).

- Profile settings post to `/settings/profile`. REST clients update `/api/v3/user`
  with GitHub's `name`, `bio`, `location` and `blog` fields.
- The legacy avatar API is removed. Settings upload the picture to `/settings/avatar`
  and remove it through `/settings/avatar/remove`, using CSRF-protected forms. PNG, JPEG,
  GIF and WebP are accepted up to 2 MB, with the type read from the bytes. Refusals answer
  422, 413 or 429. Replacing or removing a picture deletes its file. Organization owners
  and admins manage their organization's picture through its settings forms.

Git metadata is not account data. `author_email`, `committer_email` and a contributor's
`email` contain the address git recorded in the commit. Commits made in the web editor
and merge commits record the account's email address.

## SSH keys (signed in)

The legacy SSH key API is removed. `/settings?tab=keys` lists keys; its CSRF-protected
forms add and delete them. Malformed or private keys are refused with 400, and duplicate
fingerprints with 409. REST clients use `/api/v3/user/keys`.

## Personal access tokens (signed in)

The legacy token API is removed. Use the CSRF-protected forms on `/settings?tab=tokens`
to create or revoke a token. Names are 1–100 bytes; scope is `read` (clone/fetch) or
`write` (clone/fetch/push). Choose a future expiry day or Never expires. The value is
shown only once, on creation; later settings reads show neither it nor its hash.

Token values are stored only as SHA-256 hashes. Bans and password resets revoke tokens
along with sessions; account suspension, required password changes and expiry deny Git
authentication. Creation and deletion require an active session. Token settings are
not cached.

### Fine-grained tokens

`/settings/personal-access-tokens` creates fine-grained tokens (`cy_fgpat_` plus 64 hex
digits). Each has a resource owner (you or one of your organizations), an expiry of at most
a year (30 days by default), a repository selection (public only, all of the owner's
repositories, or up to 100 selected ones) and GitHub's permission names with `read`,
`write` or `admin`. A request is allowed only when both the token and your current access
allow it, so leaving a repository or organization takes effect at once. Responses carry
`X-Accepted-GitHub-Permissions`, naming the permission an endpoint needs.

Organization owners set the policy on the organization's **Personal access tokens** tab:
whether fine-grained tokens need approval, whether classic tokens may reach the
organization, and a maximum lifetime. A token waiting for approval reads public data only.

## Repositories

- `GET /api/repos`: the 100 newest public repositories. Not paginated; the login
  state of the caller is not used.
- `POST /api/repos` (signed in): body `name` (1 to 100 bytes: letters, digits,
  `-`, `_`, `.`), `description` (optional, at most 2,000 bytes), `is_private` (default
  `false`) and `owner_org_id` (optional). Returns 201 with the repository.
- `GET /api/repos/{owner}/{repo}`: the repository object, or 404 when you cannot read it.
  It includes `allow_merge_commit`, `allow_squash_merge` and `allow_rebase_merge`, which
  are true unless an administrator has turned one off.
- `PATCH /api/repos/{owner}/{repo}` (admin): body `is_private` (required),
  `description` (optional, at most 2,000 bytes; leaving it out clears the description)
  and the optional merge settings `allow_merge_commit`, `allow_squash_merge` and
  `allow_rebase_merge`. A merge setting left out keeps its current value. At least one
  merge method must stay allowed: a request that would turn all three off returns 400
  `validation_error` and changes nothing.
- `DELETE /api/repos/{owner}/{repo}` (admin).
- `POST /api/repos/{owner}/{repo}/rename` (admin): body `new_name`. Returns the repository.
- `POST /api/repos/{owner}/{repo}/transfer` (admin): body `new_owner`, the username or
  organization that will own the repository. You need permission to create repositories
  for that owner. The repository keeps its ID and storage; its path becomes
  `{new_owner}/{repo}`. Returns the repository. An unknown owner is 400, a name the new
  owner already has is 409.
- `GET /api/repos/{owner}/{repo}/permissions`: your permissions on the repository.
- `GET /api/repos/{owner}/{repo}/languages`: items `{language, bytes, percentage, color}`.
  Language calculation accepts at most 100,000 committed tree entries and 16 MiB of tree listing.
- `GET /api/repos/{owner}/{repo}/contributors`: items `{name, email, commits, user_id, avatar_url}`.
  At most 10,000 complete contributor records and 1 MiB of identity metadata are returned;
  generated shortlog output is also capped at 1 MiB.
- Stars: `GET`, `PUT`, `DELETE /api/repos/{owner}/{repo}/star` return
  `{starred, stars_count}`. `GET` works without signing in.
- `GET /api/repos/{owner}/{repo}/stargazers?page=&per_page=`: public user objects,
  newest stars first, then descending user ID for equal timestamps.
- Watching: `GET`, `PUT`, `DELETE /api/repos/{owner}/{repo}/watch`.
- Forks: `POST /api/repos/{owner}/{repo}/forks` returns 201 `{repo, message}`. Forking
  your own repository returns 400. `GET /api/repos/{owner}/{repo}/forks?page=&per_page=`
  lists forks by creation time descending, then repository ID descending. Each page
  examines at most `per_page` candidates and omits unreadable private forks without
  refilling. A short or empty page does not mean later pages are empty.
  `GET /api/repos/{owner}/{repo}/fork` returns `{has_fork, fork}`.
- Collaborators (repositories you own personally): `GET` and `POST
  /api/repos/{owner}/{repo}/collaborators`. The POST body is `username` and
  `access_level`: `admin` or `write`; any other value means read.
  `DELETE /api/repos/{owner}/{repo}/collaborators/{username}`.
- Actions secrets (admin). `PUT /api/repos/{owner}/{repo}/secrets/{name}` takes body
  `value` and returns 204; it needs `SECRETS_MASTER_KEY` and otherwise returns 503.
  `GET /api/repos/{owner}/{repo}/secrets` lists names and timestamps, never values,
  and `DELETE` works without the key.
  Names are 1 to 255 ASCII letters, digits or underscores, start with a letter or
  underscore, and must not begin with `GITHUB_` in any letter case. Values cannot
  contain NUL and are limited to 48 KiB. A repository holds at most 100 secrets
  totalling 1 MiB of names and values. Invalid input returns 400
  `validation_error`. `DELETE /api/repos/{owner}/{repo}/secrets/{name}` remains
  available for invalid names stored by older versions.

## Secret scanning

Enable secret scanning and push protection under repository Settings > Code security.
Scanning requires `SECRETS_MASTER_KEY`; alert values are encrypted with that key and
visible only to repository administrators through `/api/v3/repos/{owner}/{repo}/secret-scanning`.
The Security tab shows alert locations and resolution forms without exposing credential values.
Webhook deliveries also omit credential values; use the authenticated alert API to inspect them.
Scans skip binary blobs and blobs over 1 MiB; over 1,000 detections in one blob fails the scan.
Push protection examines newly added blobs,
with a 64 MiB push budget; split pushes that exceed it. Its rejection links to an actor-bound,
three-hour bypass requiring a reason. Custom patterns created through REST remain drafts
until published in Code security. Newly issued personal tokens have a `cy_pat_` prefix;
older unprefixed tokens remain valid but cannot be reliably distinguished from arbitrary hex.

## Releases

A release names a tag and carries notes and uploaded files. Readers see published
releases of the repositories they can read; writers also see drafts and create, change
and delete releases. When the tag does not exist, publishing creates it at
`target_commitish` (a branch or commit). Deleting a release keeps its tag.

- `GET /api/repos/{owner}/{repo}/releases?page=&per_page=` (default 30, at most 100): newest first.
- `POST /api/repos/{owner}/{repo}/releases` (writer): `tag_name`, `target_commitish`, `name`,
  `body`, `draft`, `prerelease`, `make_latest` (default true). Returns 201.
- `GET /api/repos/{owner}/{repo}/releases/{id}` and `.../releases/tags/{tag}`.
- `PATCH /api/repos/{owner}/{repo}/releases/{id}` (writer): sends only the fields to change.
  The tag and target of a release cannot change; sending different values returns 422.
- `DELETE /api/repos/{owner}/{repo}/releases/{id}` (writer): 204. Its uploaded files are removed.

Each release lists its `assets` with `browser_download_url` and `download_count`. The
HTML pages upload assets (up to 100 MiB) and serve them from
`/{owner}/{repo}/releases/download/{tag}/{name}`, which counts each download.

## Git content

- `GET /api/repos/{owner}/{repo}/tree/{ref}` and `.../tree/{ref}/{path}`: a JSON array
  of `{name, path, type, size, last_commit}`. `type` is `file`, `dir` or `unknown`.
  An empty repository gives `[]`. A directory is returned only when its complete listing has at
  most 1,000 entries and its path metadata fits the response budget; otherwise it returns 413
  rather than a partial listing. `last_commit` is optional: when the history needed to fill it
  exceeds its budget, the entries are still listed and their `last_commit` is `null`.
- `GET /api/repos/{owner}/{repo}/blob/{ref}/{path}`: `{path, content, encoding, size,
  is_binary, truncated, ...}`. Text is returned as UTF-8 with `encoding: "utf-8"`, in
  a preview of 256 KiB by default. The `offset` and `limit` query parameters page
  through it; the response gives `next_offset` and `prev_offset`. Binary files are
  base64-encoded. Binary files over 8 MiB return 413.
- `GET /api/repos/{owner}/{repo}/raw/{ref}/{path}`: the file bytes, up to 32 MiB per
  response. Single `Range: bytes=...` requests return 206 and can read larger blobs.
- `GET /api/repos/{owner}/{repo}/archive`: a source archive of the default branch, capped at
  32 MiB. The browser pages download `/{owner}/{repo}/archive/{ref}.zip` or
  `{ref}.tar.gz` for any branch, tag or commit; an unknown ref returns 404.
- `POST /api/repos/{owner}/{repo}/contents/{path}` (create a file): body `message`,
  `content` (base64, at most 1 MiB), `branch` and `expected_revision` (the branch's
  current commit ID, or `unborn` for an empty branch). Returns 201 `{sha, message, path}`,
  where `sha` is the new commit. An existing path returns 400. A branch that does not
  exist returns 400 unless `expected_revision` is `unborn`.
- `PUT /api/repos/{owner}/{repo}/contents/{path}` (update a file): the same body.
  Returns `{commit}`. A branch that moved since `expected_revision` returns 409. A
  missing file or branch returns 400.
- Branches: `GET /api/repos/{owner}/{repo}/branches` returns `[{name, is_default}]`.
  Repository branch/tag ref materialization is limited to 10,000 complete refs and 1 MiB of names
  and object IDs. A new branch or tag past either limit is refused, whether it comes from this
  API, a file save on a new branch or a push. Concurrent writers are checked without a lock, so
  they can pass a limit by a few refs; listings still succeed up to 256 refs and 64 KiB beyond it.
  `POST` takes `name` and `sha` (a commit ID) and returns `{name, sha}`.
  `DELETE /api/repos/{owner}/{repo}/branches/{branch}`. The default branch cannot be deleted.
  A branch or tag name that begins with `refs/` is accepted, but it cannot be told apart from
  a full ref name in URLs and revision parameters, so avoid such names.
- Tags: `GET /api/repos/{owner}/{repo}/tags` returns `[{name, sha, message}]`, with
  at most 10,000 tags, 256 KiB per annotated-tag object and 1 MiB of tag metadata.
  `POST` takes `name`, `sha` and `message` (optional, at most 252 KiB). `DELETE
  /api/repos/{owner}/{repo}/tags/{name}`.
- Commits: `GET /api/repos/{owner}/{repo}/commits?ref=&page=&per_page=` returns items
  `{sha, message, author_name, author_email, author_date, committer_name,
  committer_email, committer_date, parents, author}`. `parents` is a list of commit
  IDs. `author` is `{id, username, avatar_url}` when the author is a known account.
  `page` is 1 through 1,000. `per_page` defaults to 30 and is clamped to 1 through
  100. Commit objects are limited to 256 KiB each and a page to 1 MiB of commit
  metadata; oversized pages return 413 rather than partial results.
- `GET /api/repos/{owner}/{repo}/commits/{sha}`: `{commit, diff}`. `diff` is
  `{files, additions, deletions, changed_files}`. Each file is
  `{filename, status, additions, deletions, patch, blob_sha}`, where `blob_sha` is the
  id of the file's new blob (all zeros when it is deleted), and `status` is `added`,
  `removed`, `modified`, `renamed` (using the new path) or `type_changed`. A
  `type_changed` file is a regular file replaced by a symlink or the reverse; it is
  one file whose `patch` holds both the deletion and the addition sections. Diffs are
  limited to 1,000 files, 256 KiB of patch per file and 1 MiB of patch in total;
  exceeding a limit returns 413 rather than a partial diff.
- Commit statuses: `GET .../commits/{sha}/status` (combined) and
  `GET`, `POST .../commits/{sha}/statuses`. The combined state is failure when any
  context is failure or error. Actions own the reserved `gitserv/` contexts:
  `.github/workflows/test.yml` writes `gitserv/workflow/.github/workflows/test.yml`.
  Multi-workflow runs use `gitserv/pipeline/<pipeline>`. Older attempts cannot replace
  newer ones for the same commit/source. Protection should name full workflow
  contexts. Released `gitserv/<stem>` and `gitserv/<pipeline>` requirements inspect
  every matching source's latest run; ambiguous stored success does not authorize
  a merge.

### Git over HTTP

- `GET /{owner}/{repo}.git/info/refs?service=git-upload-pack` and
  `POST /{owner}/{repo}.git/git-upload-pack` serve clone and fetch.
- Public repositories clone anonymously on public nodes. Private repositories and
  every push require Basic auth with a personal access token as the password.
  Account passwords and session cookies are not Git credentials. Missing, expired,
  revoked or bad credentials return 401 with `WWW-Authenticate: Basic`; valid
  credentials without the required scope or repository access return 403.
- `service=git-receive-pack` advertises write refs;
  `POST /{owner}/{repo}.git/git-receive-pack` accepts pushes with a `write` token and
  repository write access. Shared Git-session pre/post-receive hooks enforce branch protection
  (with the refusal reason shown to the pusher) and update metadata, pull requests and CI.
- RPC requests use `application/x-git-{upload,receive}-pack-request`. Upload-pack
  requests support gzip, with a 16 MiB compressed/decompressed limit. Receive-pack
  requires identity encoding (otherwise 415) and streams chunked pushes without a
  pack-size limit. Responses are streamed. HTTP and SSH share a limit on concurrent sessions
  per process (`GIT_SESSION_LIMIT`, default four per core, at least 16; a request waits up to
  `GIT_SESSION_WAIT_SECONDS`, default 30, for a slot, then gets 503 "Git sessions busy"), a five-minute idle timeout and a one-hour
  total timeout. Failed token logins use the failed-login budgets (429 when
  exhausted). Use HTTPS for credentials.

## Issues

- `GET /api/repos/{owner}/{repo}/issues?state=open|closed|all`: the default is `all`;
  any other value returns 400. Returns issues newest first, 100 per page (`page` and
  `per_page`).
- `POST /api/repos/{owner}/{repo}/issues`: body `title` (1 to 255 bytes, not only
  whitespace) and `body` (optional, at most 65,536 bytes).
- `GET /api/repos/{owner}/{repo}/issues/{number}`.
- `PUT /api/repos/{owner}/{repo}/issues/{number}`: body `expected_state` (`open` or
  `closed`) and `expected_updated` (the `updated_at` you read), both required, plus the
  fields to change. A record that changed returns 409.
- Comments: `GET` and `POST /api/repos/{owner}/{repo}/issues/{number}/comments`. `GET`
  takes `page` and `per_page` and returns the comments oldest first. The POST body is
  `body`, which must contain text other than whitespace and at most 65,536 bytes.
- Labels (admin): `GET` and `POST /api/repos/{owner}/{repo}/labels`,
  `DELETE /api/repos/{owner}/{repo}/labels/{label_id}`,
  `POST /api/repos/{owner}/{repo}/issues/{number}/labels`, and
  `DELETE /api/repos/{owner}/{repo}/issues/{number}/labels/{label_id}`.

### Issue suggestions

Repository administrators enable suggestions on a native issue page. The same page lets
readers propose changes and see their own history; triage collaborators can review all
visible proposals. Applying field values additionally requires push access. Approval and
its issue change and timeline event commit together. Dismissal does not change the issue.

Agents can submit `POST /api/repos/{owner}/{repo}/issues/{number}/suggestions` with a
write-scoped Bearer token (not cookie-only authentication). Job tokens must belong to
this repository and have `issues: write`. The JSON body has `action` (`set_type`,
`add_label`, `add_field`, `add_assignee`, `close_issue`), optional `target_id`,
`target_value`, `rationale` (at most 16,384 bytes), and `confidence` (`LOW`, `MEDIUM`,
`HIGH`). Targets are type, label, field, or user IDs; a close target is a readable duplicate
issue. A null type target removes the type. Field values follow the field's data type,
including arrays of option names for multi-select. Closing accepts `completed`,
`not_planned`, or `duplicate`, defaulting to completed (or duplicate when a target is set).
The response is 201 with a GitHub-shaped issue suggestion, not a direct issue mutation.
Already-satisfied changes are recorded as applied; refreshing one's pending proposal
replaces the previous proposal in the same change slot. At most 100 proposals may be
pending on an issue. Proposals cannot be submitted to archived or locked discussions.

The GitHub review API is `GET /api/v3/repos/{owner}/{repo}/issues/{issue_number}/suggestions`
and `POST .../suggestions/{suggestion_id}/approve` or `/dismiss`. It requires triage
access and hides organization-member-only field proposals from other collaborators.

## Pull requests

- `GET /api/repos/{owner}/{repo}/pulls?state=open|closed|all`: the default is `all`.
  Returns pull requests newest first, 100 per page (`page` and `per_page`). Every item
  has `stack_id`: the stack the pull request is in, or null (see Stacks below).
- `POST /api/repos/{owner}/{repo}/pulls`: body `title` (1 to 255 bytes, not only
  whitespace), `body` (optional, at most 65,536 bytes),
  `head_branch`, `base_branch`, `head_repo_id` (optional, defaults to the base repository)
  and `is_draft` (optional). The head must be the base repository or a direct fork of
  it, and the author needs read access to both. Unknown branches and identical
  branches within the same repository return 400. Fork heads may use the same branch
  name as the base. Responses include `head_repo_id`; null means the head repository
  was deleted. The create page defaults to the author's own fork when one exists.
  Its `/pulls/new` native form uses an internal read-only comparison helper, and so does
  the HTML compare page at `/{owner}/{repo}/compare/{base}...{head}`; there is no JSON
  comparison endpoint.
- `GET /api/repos/{owner}/{repo}/pulls/{number}`. While a pull request is open its
  `head_sha` and `base_sha` follow the two branches: a push, a commit saved in the web
  editor, a merge into the base branch and creating or deleting a branch update them
  together with `updated_at`. Merged and closed pull requests keep the commits they had,
  and reopening one catches it up. A branch that is deleted leaves the last commit in place.
  Single pull request answers (`GET`, `POST` create, `PUT`, `ready` and `workflows`) add
  `stack`: null, or `{id, base_branch, size, position, entries}`. `base_branch` is the base
  of the bottom layer, `position` is the pull request's place (1 is the bottom), and
  `entries` lists every layer in the order described under Stacks, as `{number, title, state,
  merged, draft, head_branch, base_branch}`. A stack with one layer left is shown as null.
- `PUT /api/repos/{owner}/{repo}/pulls/{number}`: body `expected_state` (`open`,
  `closed` or `merged`) and `expected_updated`, both required, plus the fields to change
  (`title`, `body`, `state` set to `open` or `closed`, and `base`). `base` moves an open
  pull request onto another branch of this repository and records that branch's commit as
  `base_sha`; it needs write access, the branch must exist (400), and it cannot be its own head
  branch (400). Another open pull request for the same
  head and new base returns 409. Merged and closed pull requests cannot change base (400
  `invalid_state`). A pull request in a stack cannot change its base (409); unstack it first.
- `POST /api/repos/{owner}/{repo}/pulls/{number}/stack` (write access): adds the open pull
  request to a stack. Its parent is the lowest-numbered open pull request in this repository
  whose head is this pull request's base branch; the pull request joins the parent's stack,
  or starts one with the parent. Asking again when it is already in that stack returns it
  unchanged. Refusals: a head in a fork (400 `validation_error`); no open parent, a parent
  that already has a layer on top of it, a pull request already in another stack, branches
  that would depend on each other, a stack that would hold more than 25 pull requests, or a
  stack that keeps changing under the join (409 `stack_join_refused`, each with a plain
  message). Returns the pull request with its stack.
- `DELETE /api/repos/{owner}/{repo}/pulls/{number}/stack` (write access): takes every
  unmerged pull request of this pull request's stack out of it. Each keeps its base branch;
  merged ones stay in the stack. Returns the pull request. Nothing to unstack returns the
  pull request unchanged.
- `POST /api/repos/{owner}/{repo}/pulls/{number}/stack/rebase` (write access; the native
  "Rebase stack" button posts the same to `/{owner}/{repo}/pulls/{number}/stack/rebase`):
  rebases the open line of the stack that the pull request is in. The line starts at its lowest
  layer that is not on top of its base branch and includes every open layer above it, so any
  layer of the stack can ask for it. Each layer in the line has its own commits replayed onto the
  head of the layer below it, bottom first, keeping authors, author dates and messages. The first
  layer is put on the current tip of its base branch. Layers below that layer do not move. Each head branch is then
  rewritten in one ref transaction that also checks the base tip and each head's old commit,
  and each head branch must allow a force update under its branch protection rule. Returns
  `{"rebased": n}`, the number of layers whose head moved; when nothing needs to move, it is `0`
  and no ref changes. Each moved pull request is refreshed and its workflows run as after a
  push, for at most five seconds; a longer refresh is caught up by the next one. The replays of
  all layers share one 120-second budget. Past it the rebase refuses with 503 `git_unavailable`
  and moves nothing, so rebase fewer layers, or push a rebased branch and rebase the rest. Refusals, which change nothing: 409 `not_stacked` for a pull request outside a stack,
  409 `stack_layer_closed` or `stack_broken` as for merges, 409 `stack_fork_head` for a layer
  whose head is in a fork, 409 `rebase_conflict` naming the pull request and its conflicting
  files, 403 `branch_protection` with the rule's own message when a head branch's rule refuses the
  update, including a linear-history rule that the restacked history would break, 409
  `revision_conflict` when a branch moved while the rebase was prepared, 403 `archived` for an archived repository, and 409 `invalid_state` for a pull request that
  is not open.
  If a lower layer's branch was rewritten, the layers above still contain its old commits and
  Rebase stack replays them; rebase the upper branch onto the rewritten one first.

Stacks: a pull request is never added to a stack by opening it or by changing its base; only
the join above adds one. The stack id is the id of the stack's bottom pull request when the
stack started, and is stored. Pull requests from forks never join, and nothing joins on top of
a pull request whose head is in a fork. The order inside a stack, which is for display, puts merged layers first, in the order they
merged; then the open layers, following their branches from the bottom upward (in number order
when the branches do not form one line); then closed layers, in number order. A merge depends
only on the layer directly below it: the member whose head branch is this pull request's base
branch. When several members share that head, an open one counts before a merged one, and a
merged one before a closed one.

Merging a layer merges the open layers below it too, bottom first, as GitHub's stacked pull
requests do. The chain is the layers from the bottom of the stack up to the merged pull request,
stopping at the first merged layer. Each layer in the chain must pass the protection rules of
the trunk (the base branch of the bottom layer) against the trunk's current tip: its approvals,
checks and conversations are judged at its own head. A draft layer, a layer whose head branch no
longer exists, or a closed layer in the chain refuses the whole merge, and the message names the
layer as `#N`. A closed layer returns 409 `stack_layer_closed`. A layer whose head does not contain
the head of the layer below it returns 409 `stack_needs_restack`. So does a layer left behind a
squash or rebase of the layer below it, until it contains the current tip of its base branch. Rebase stack (above) moves such a stack, and the message names it.
A stored loop returns 409 `stack_broken`. Only a merge commit can merge a layer with open layers
below it: squash and rebase return 409 `stack_needs_merge_commit`. The chain lands on the trunk in
one ref update, and every layer in it is marked merged with the same `sha`. If the trunk or any
layer in the chain moved after the merge was prepared, the publish returns 409 `revision_conflict`
and nothing is marked. A layer with nothing below it, or with a merged layer below it, merges as
usual. The stack is read again when the merge is published, so a join made after the merge was
prepared cannot bypass it. After a merge, the open members of the stack whose base was the head
branch of a merged layer move to the branch the merge landed in, with that branch's new commit as
`base_sha`. Pull requests outside the stack are never
moved, even when they target the merged head branch. A member that would duplicate another
open pull request for its head and base stays where it is.
- `POST /api/repos/{owner}/{repo}/pulls/{number}/ready` (the author or someone with write
  access): takes an open draft out of draft, brings it up to date with its branches and
  queues its `pull_request` workflows like a new pull request. Returns the pull request.
  A pull request that is not open returns 400 `invalid_state`, one that is not a draft
  409 `invalid_state`, anyone else 403.
- `POST /api/repos/{owner}/{repo}/pulls/{number}/merge`: body `expected_head` and
  `expected_base`, the commit IDs the pull request currently has (read them again with
  `GET` after a push or after the base branch moved). A mismatch returns 409
  `revision_conflict`. The optional `expected_base_branch` is the base branch the client read;
  when it differs from the pull request's current base branch, the merge is refused with the
  same 409 `revision_conflict` before anything is published, so commits that match on two
  branches cannot merge into the wrong one. Clients that omit it are checked on the commits
  alone. The optional `expected_target` is the branch the client confirmed the merge lands in
  (for a stack layer, its trunk, see Stacks); when the merge would land elsewhere now, it is
  refused with the same 409 `revision_conflict`. A deleted branch returns 409 `head_branch_missing` or
  `base_branch_missing` while its repository exists; a deleted fork keeps its last
  head commit and can still be merged. A draft returns 409 `unsupported_merge`. A merge always creates a
  merge commit of the base and the head, even when the head already contains the base.
  Returns `{sha, merged, message}`, where `sha` is the commit the base now
  points to. Conflicts and branch protection return 409; unresolved conversations
  return 409 `unresolved_conversations` with the count in the message.
  Approvals count for the current head commit only while `dismiss_stale_reviews` is true
  (the default); while it is false, each reviewer's latest approval counts for any commit
  (see [Branch protection](#branch-protection)). Status checks are bound to the current head
  commit.
  The optional `merge_method` is `merge` (the default), `squash` or `rebase`. A squash
  merge adds one commit on the base: its author is the pull request's name with a
  `<username>@users.noreply` address, its
  committer is the merger, its message is the title and `(#number)`, and its body lists
  the squashed commit subjects within 8192 bytes. The squash commit is attributed to the
  author's name with a no-reply address, so commit lists do not link it to the account yet. A rebase merge writes each commit of the
  pull request again on the base, keeping its author, author date and message, and skips a
  commit whose change the base already has. A method the repository does not allow returns
  409 `merge_method_not_allowed`; this is checked when the merge is prepared and again when
  it is published. A rebase with a merge commit in the pull request or more than 100 commits,
  and a squash or rebase that would add no change, return 409 `unsupported_merge`. A
  conflict returns 409 `merge_conflict`, and its message names the conflicting files. While
  open pull requests are stacked above this one, a squash or rebase lands it on the trunk and
  moves them onto the trunk too; each then needs Rebase stack before it can merge (see Stacks).
  A layer with open layers below it accepts only the merge
  commit: squash or rebase returns 409 `stack_needs_merge_commit`. The merge commit lands the
  whole stack below the layer too (see Stacks above).
  Squash and rebase use `git merge-tree --write-tree`, which needs Git 2.40 or later.
- `POST /api/repos/{owner}/{repo}/pulls/{number}/workflows` (write access): body
  `expected_head`, the current head commit. Approves and queues fork workflows for
  that commit; a stale head returns 409 `revision_conflict`. The PR must be open and
  not a draft. The native "Run workflows" form calls the same operation. Existing CI
  host and deployment permissions still apply. Fork runs never receive secrets.
  If a matching workflow cannot be queued (for example a deployment workflow for a
  writer without deployment rights, or CI that is temporarily unavailable), the
  approval stays recorded and the response is an error with code `ci_not_queued`:
  403 when the workflow is denied, 503 when CI is unavailable, 409 otherwise. Sending
  the same request again returns the workflows still queued or running for that commit,
  and queues a new attempt of each one whose latest attempt has finished.
  Same-repository PR workflows queue automatically when opened, readied or their
  head moves. Each workflow queues once per head commit automatically.
- `GET /api/repos/{owner}/{repo}/pulls/{number}/diff`: the same diff object as commits.
- Comments: `GET` and `POST /api/repos/{owner}/{repo}/pulls/{number}/comments`. `GET`
  takes `page` and `per_page` and returns the comments oldest first. The POST body is
  `body` (text other than whitespace, at most 65,536 bytes), with optional
  `path` (at most 4,096 bytes, including inherited reply paths), `line`,
  `side` (`LEFT` or `RIGHT`), `commit_sha` (40 hex characters) and
  `in_reply_to_comment_id`. A reply to a reply is stored under its thread root, so the
  response names the root. Listings return `path: null` for oversized legacy paths;
  the stored value and comment body are preserved.
- Reviews: `POST .../pulls/{number}/reviews` takes `state` (`commented`, the default,
  `approved` or `changes_requested`), `body`, and `commit_sha` (default: the published
  head). A `commented` review needs read access. `approved` and `changes_requested` need
  write access, and the pull request author cannot send either. A body is required unless
  the state is `approved`. A push never rewrites a review: whether an approval still counts
  is decided when the branch rule is checked for a merge (see `dismiss_stale_reviews` under
  [Branch protection](#branch-protection)).
- Review requests and branch protection: see [Other routes](#other-routes). The pull request
  page's Reviewers panel offers the same request, re-request and removal to writers, through
  native forms that follow these routes' rules.

## Discovery

The legacy explore and search APIs are removed. Browse `/explore` and `/search` with
GET forms and paginated, authorized page models. Search shows only the selected tab's
results, with bounded counts for the others; private repositories and issues require
current read access. REST clients use `/api/v3/search/repositories`, `/api/v3/search/issues`
and `/api/v3/search/users` (see [coverage](GITHUB_API.md)).

## Notifications (signed in)

- `GET /api/notifications?limit=`: `limit` is 1 to 100, default 30. Optional
  `unread_only=true` filters unread rows. Pass the response's `next_cursor` as
  `before` to read the next older page; null means no older visible rows. The
  cursor preserves `(created_at,id)` ordering; each page checks current access.
- `PATCH /api/notifications/{notification_id}/read` and `POST /api/notifications/read-all`:
  send `X-CSRF-Token`.
- `GET /api/v3/notifications/ws`: a documented WebSocket extension. It requires a same-origin `Origin` header.
  An account may hold at most 8 open sockets; the ninth is refused with 429 `rate_limited`.
- Kind `mention`: a user that new issue, pull request, comment or review text `@mentions` is
  notified, and so is a user that an edited issue or pull request body newly mentions. Only
  users who can read the repository are notified, and one event notifies at most 20 users.
  An event looks up at most 100 new names, in the order they appear; later names are not
  notified.

## CI

Runs, logs and cancellation are described in [CI API](CI_API.md). A run's `branch` is
the short name (`main`), whether a person or a push started it; a pushed tag shows
`refs/tags/NAME`. In summary:
`POST /api/ci/trigger` (a `workflow_dispatch` run takes `inputs`, see
[CI API](CI_API.md#workflow-dispatch-inputs)), `GET /api/ci/workflows`, `GET /api/ci/jobs`,
`GET /api/ci/jobs/{job_id}`, `.../logs`, `.../history`, `.../log-download`,
`POST .../cancel` (the session's `X-CSRF-Token` header, no body) and `.../deployment`.

## Files

- `POST /api/files` (signed in, `X-CSRF-Token`): creates an upload. Body: `filename`,
  `content_type`, `file_size`, `reason` (the kind of file; each kind has its own size
  limit), `visibility` (`owner` or `public`; `public` is for avatars), and `sha256_hex`
  (64 lowercase hex characters). Returns `upload_url`, `headers` (send them with the
  upload), `method`, `file_id`, `s3_key` (S3 only), `expires_in` (900 seconds) and `capability`.
- `POST /api/files/confirm` (`X-CSRF-Token`): body `file_id`. Call it after the bytes are
  uploaded. The server checks size, checksum and file signature. Returns `{success}`.
  An upload that expired or was already confirmed returns 409. A mismatch returns 422.
- `PUT /api/files/{file_id}/content`: uploads the bytes for an upload created with
  local storage.
- `GET /api/files/{file_id}/content` and `GET /api/files/{file_id}/download`: redirect to
  S3 (307), or stream the file from local storage.
- `GET /api/files/{file_id}/dl`: `{download_url, filename, content_type, file_size,
  expires_in, capability}`. `GET /api/files/public/{file_id}` returns the same shape
  for public files.
- `DELETE /api/files/{file_id}` (owner, `X-CSRF-Token`). Other users get 404. A file
  currently selected as an organization picture returns 409 `file_in_use`; use the
  authority-checked organization picture action to replace or remove it. A file that is
  a release asset also returns 409 `file_in_use`; delete the asset from its release.
- `GET /api/files/stats`: your storage use, `{total_files, total_bytes, by_reason,
  last_24h}`.

Uploads count against per-kind size limits and against hourly and daily quotas.
A refused upload returns 400 `invalid_upload`, and a quota returns 429 `rate_limited`.
Signing failure leaves a charged pending reservation until deletion/expiry. Confirmation
distinguishes invalid/missing bytes (422) from unavailable storage (503). Deletion removes
metadata immediately and atomically records object cleanup. Object removal is attempted at
once and retried by the bounded upload sweep. The bytes are due one hour after the file's
900-second PUT capability expires, so a transfer already in flight, or a replayed PUT,
cannot leave an orphan. A key that cannot be removed is retried 15 minutes later.

## Site configuration and health

- `GET /health`: `{"status": "ok"}` while the process is up.
- `GET /ready`: `{"status": "ready"}`, or 503 while this node cannot reach its database.
- The legacy configuration API is removed. Repository pages show their available clone
  URLs; the node's SSH transport is configured at startup.
- `GET /robots.txt` and `GET /ai.txt` (plain text, no authentication) follow the site's AI
  crawler setting, `block` or `allow`, which administrators change on the admin page. `block`
  is the default, and so is a missing setting. While it is `block`, `robots.txt` lists the AI
  training crawlers (GPTBot, ChatGPT-User, CCBot, ClaudeBot, anthropic-ai, Google-Extended,
  PerplexityBot, Bytespider, Applebot-Extended, cohere-ai, Meta-ExternalAgent, Amazonbot,
  Diffbot, ImagesiftBot, omgili, FacebookBot) with `Disallow: /`, leaves other agents allowed,
  and `ai.txt` is `User-Agent: *` with `Disallow: /`. While it is `block`, every HTML response
  carries `X-Robots-Tag: noai, noimageai`. When it is `allow`, both files are `User-agent: *`
  with an empty `Disallow:` and HTML responses carry no such header. If the setting cannot be
  read, these files answer 503 and HTML responses carry the header.

## Limits

There is no general pagination; the lists named above are the exceptions. The limits are
these:

- Repositories, issues and pull requests: `page` and `per_page`; both default to 100 items
  and are clamped to 1–100, newest first.
- Explore: 50 by default. Search: up to 50 of each kind, and on SQLite at most 50 million
  matching steps per kind (see [Discovery](#discovery)).
- Commits, forks, stargazers, releases, issue and pull request comments, and pull request
  reviews: pages 1–1,000;
  `per_page` defaults to 30 and is clamped to 1–100.
- Git reads: tree directories and diffs have at most 1,000 files, contributor lists
  have at most 10,000 entries, and language scans accept at most 100,000 files.
- Lists with a `limit` parameter: notifications (1 to 100, default 30), CI jobs
  (1 to 200, default 50), CI logs (1 to 5000, default 2000), CI history (1 to 1000,
  default 500).
- Users, collaborators and labels are not capped.
- Repository descriptions are at most 2,000 bytes on creation and update. Review bodies
  and dismissal reasons are at most 65,536 bytes, like comments. Longer values that
  already exist stay readable.
- Request bodies on POST, PUT, PATCH and DELETE are read in full before the handler runs:
  at most 4 MiB and 30 seconds to read, and at most 8 reads at once per node. The streamed file
  upload `PUT /api/files/{id}/content` and Git over HTTP are not covered.
- Raw file and archive downloads are built in memory: at most 4 at once per node, and a
  request that finds none free gets 503 `downloads_busy`. A transfer whose client stops
  reading for 30 seconds ends.
- Cleanup removes expired anonymous browser sessions, then the oldest by expiry beyond
  100,000, across every node that uses the database. That is not a hard maximum: the count
  can exceed 100,000 between runs, and a flood of anonymous requests can evict an unused
  login or signup form, which the visitor must reload the page to replace. Rate limiting
  anonymous traffic belongs to the reverse proxy. Authenticated sessions are not capped, and
  no session is refused for this reason.

There is no general API rate limit and no `X-RateLimit-*` headers. Failed password attempts
are throttled: 10 per 300 seconds per account and 30 per 300 seconds per client address,
after which the request returns 429 `rate_limited`. Upload quotas return 429. Over SSH, five
failed attempts within 60 seconds block the client address for five minutes.

## Other routes

These routes exist. Check the router for the method and body before you use them.

- Site administration uses native forms, not `/api/auth/admin/*`. `/admin/users`
  lists accounts and pending signup requests; `/admin/users/new` creates managed
  accounts. Each account's ban form and each request's decision form carry CSRF
  protection. `/admin/settings` controls signup and AI crawlers (blocked by default);
  `/admin/email` configures mail and offers a test-mail form. The Audit log page (`/admin/audit`) lists
  recorded administrative changes, newest first, 50 per page, filtered by the `action` and `actor`
  query parameters.
  Each account's `pushes_total` counts CI runs that push events started,
  one per matching workflow. It is not a count of pushes: a push that matches no
  workflow counts zero, and `contributions_total` includes these runs.
  `commits_total` counts commits recorded in accepted branch updates, attributed to the
  pushing account; the activity chart places those commits on the update's date.
  Reintroducing commits in a later update can count them again.
- Repositories: `GET /api/repos/{owner}/{repo}/archive` (listed above),
  `GET`, `PUT /api/repos/{owner}/{repo}/branch-protection` and
  `DELETE /api/repos/{owner}/{repo}/branch-protection/{pattern}` (see
  [Branch protection](#branch-protection)),
  `POST .../pulls/{number}/comments/{comment_id}/resolve` and `/unresolve`
  (the pull request author or write access; only the first comment of a conversation on
  a file; returns the comment with `resolved_at` and `resolved_by_id`),
  `GET /api/repos/{owner}/{repo}/pulls/{number}/reviews` (`page` and `per_page`, as for
  comments; and `POST`),
  `POST .../reviews/{review_id}/dismiss`,
  `GET` and `POST .../pulls/{number}/requested_reviewers`,
  `DELETE .../pulls/{number}/requested_reviewers/{username}`.
- Files: `GET` and `PUT /api/files/{file_id}/content` (listed above).

### Branch protection

`PUT /api/repos/{owner}/{repo}/branch-protection` (Admin) saves one rule per `pattern`,
and `GET` (Admin) lists them. Several rules can match one branch; every matching rule has to
allow a write, so the strictest wins. A rule applies to Git pushes, the editor, the
contents API, the branch API and merges.

- `pattern`: a branch name or a glob. `*` and `?` stay inside one path segment and `**`
  crosses `/`, so `release/*` covers `release/1.0` but not `release/1.0/rc`; use
  `release/**` for both. A new pattern is at most 254 characters. A rule saved before that
  limit keeps its pattern and can be saved again; a 255-character pattern that ends in `*`
  matches like `**` at its end.
- `require_pull_request`: no direct writes. Required approvals, required status checks and
  `require_conversation_resolution` also leave no direct path.
- `required_approving_reviews`, `require_status_checks`, `required_status_checks`,
  `require_up_to_date_branch`: checked at merge, as before. `required_approving_reviews` is 0
  to 10. A check name may not contain a control character such as a line break or tab, so
  the settings form can always show it.
- `require_conversation_resolution`: a merge is refused while a conversation on a file is
  unresolved.
- `allow_force_pushes`, `allow_deletions` (both default false): Git pushes that rewrite
  the branch or delete it are refused. The default branch cannot be deleted whatever the
  rules say. A refused force push is refused for administrators and the repository owner
  too, unless `force_push_allowances` lists them.
- `force_push_allowances` (usernames, each with write access; same rules and 100-entry limit
  as `push_allowances`): users who may force push although `allow_force_pushes` is false.
  Everyone else is refused, administrators included. A branch with no matching rule accepts
  force pushes from writers.
- `require_linear_history`: pushes may not add merge commits, and a pull request merge with the
  merge commit method is refused. Squash and rebase stay allowed.
- `restrict_pushes` and `push_allowances` (usernames, each with write access): only the
  listed users may push or merge. An allowance does not skip the pull request requirement.
  A request may list at most 100 entries, repeats included; a repeated name is stored once.
- `enforce_admins` (default true): while true, repository administrators are held to the
  rule like everyone else. When false they skip it for direct writes and for the merge
  gates.
- `dismiss_stale_reviews` (default true): while true, an approval counts only when it names
  the current head commit. While false, each reviewer's latest approval counts for any
  commit. A `changes_requested` review blocks the merge until its reviewer posts a newer
  `approved` or `changes_requested` review, or a repository administrator dismisses it,
  whatever commit it names.
- Omitting `require_pull_request`, `dismiss_stale_reviews` or `enforce_admins` means true,
  so a script written before these settings existed keeps the strictness it had.
- `require_code_owner_reviews` makes a merge wait for an approval from one owner of each
  changed file that has owners in the base branch's `CODEOWNERS`. Owners without write access
  do not count.
- `require_signed_commits` is not enforceable. A request that sets it returns 400
  `unsupported_protection`. A rule that was saved with it keeps refusing merges until it is
  deleted and created again.

`DELETE` takes the pattern percent-encoded, for example `release%2F**`.

Classic protection under `/api/v3/repos/{owner}/{repo}/branches/{branch}/protection`
shares these rules. Signature requirements reject commits without verified signatures,
including smart-HTTP pushes; no signature verifier is configured, so enforcement fails
closed. App restrictions check the app's current installation and repository write access.
App-bound checks, review dismissal/bypass allowances, last-push approval, branch locking,
creation blocking and fork-sync settings are stored and returned without extra enforcement.
Branch rename follows exact protection, open pull requests and the default branch, and
redirects old branch API URLs. Merge-upstream fast-forwards or merges a fork from its
readable parent, preserves ahead-only forks, returns 409 on conflicts and 422 for missing
or invalid branches.

Administrators can also manage the same rules on the repository settings page, in its
Branch protection section. The forms work without JavaScript and check the same input as
this API, so a refused rule gets the same message in both places.

## Projects

User and organization profiles link to Projects. Create projects there, then use the
project settings for visibility, fields, repository links, closing and deletion. Table
and single-select board views support field filters (`status:"In progress"`, `is:issue`,
`is:open`) and sorting. All changes use native forms and work without JavaScript.
Project access never grants access to its items' repositories: private issue and pull
request content remains hidden from outsiders, even in public projects. User owners and
organization owners/admins can write; organization members can read private projects.
The same domain operations back `/api/v3` Projects v2. Charts, insights, roadmap layouts
and workflow automations are not available.

## GitHub GraphQL

`POST /api/graphql` accepts the same Bearer or token credentials as `/api/v3` and
exposes the vendored GitHub schema through introspection. The compile-time types and
resolver traits live in `crates/codeyard-github-graphql`; regenerate them with
`python3 scripts/github-graphql-types.py` after changing the SDL, never by hand.
The generator's `--check` mode is part of the release gate. Resolvers delegate writes
to shared native operations, including their live permissions, notifications, hooks
and CI behavior. Implemented mutation groups are:

- Issue/PR creation, editing, close/reopen, ready-for-review, merge and branch update.
  Issue close reasons preserve completed, not-planned and duplicate reasons.
- Label CRUD, labels/assignees/milestones on issues and PRs, discussion lock/unlock,
  conversation comment edit/delete and reaction add/remove.
- Review creation, submission, editing, pending deletion, dismissal and user review
  requests. Pending reviews/comments are visible only to their author.
- Repository creation (personal or organization, optional native team grant), About
  metadata/rename, archive/unarchive, stars, repository subscriptions and user follows.

PR creation accepts the default `maintainerCanModify: true`. Only a head writer can
issue a fork grant; the author can revoke it. The grant authorizes updates of that
open PR's head through `updatePullRequestBranch`, with live storage authorization
and branch protection, not arbitrary writes to the fork. Merge author overrides,
rebase branch updates, team/bot review requests and inline review creation are not
implemented. Other unbound schema fields/mutations and unsupported filters return
explicit errors rather than simulated success.

Global nodes share REST IDs for users, organizations, repositories, issues, PRs,
labels, milestones, teams, conversation/commit comments, reactions (`REA`, distinct from release
`RE`), releases/assets, reviews/comments, stored status contexts and supported
timeline events. Team reads retain native active-membership and secret-team visibility.
Git commits, blobs, trees, tags and refs resolve only through
readable repositories. Codeyard stacks (`S`) and Actions artifacts (`ART`) have no
GitHub-schema object equivalent and return explicit errors after authorization;
unsupported timeline event kinds do likewise.

CLI query fixtures are copied from `cli/cli` commit
`5056c0a8ef34512be1d5b4120d4e4b30412ada8b`. Repository information and owner listings
include viewer permissions, readable private repositories and count-only totals.
Issue/PR lookup supports the CLI's `issueOrPullRequest` union, collaboration metadata,
PR files/diff statistics, commits/authors, stored statuses/rollups, review decisions,
mergeability and policy blockers. Filtered issue/PR search, repository issue filters,
milestones, releases/assets and Git refs/objects use shared native readers and counts.
Semantic/hybrid search is unavailable without a semantic index; repository/user
qualifiers are limited to the existing native name/owner search. Projects, issue
hierarchies/dependencies, pins and closing-reference relationships have no backing
product: their reads are empty/null/false, and their mutations remain unsupported.
Minimized comments and automatic merge requests are not supported. Codeyard has no
wiki/template repository product or per-repository issue/PR disable switches: use
`gh repo create --disable-wiki`; unsupported configuration requests are rejected
before provisioning. Repository edits retain native settings transactions; a failure
following an already committed creation/edit is reported as a partial update.

Actions job tokens retain REST's permission families and are confined to their own
repository, including node, parent and fork/head lookups. Cross-repository resources
are masked as missing; insufficient scopes report `Resource not accessible by token`.

Connection pages require `first` or `last` between 1 and 100. Count-only selections
(such as the `gh repo` issue and watcher totals) do not require a page size. Each token has a database-backed
5,000-query hourly allowance; every admitted query costs one, including failed queries.
`rateLimit` reports that allowance. Queries are also limited to depth 20, complexity
1,000, 10,000 resolved fields and 30 seconds. Subscriptions are not supported.
