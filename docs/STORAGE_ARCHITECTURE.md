# Storage

Codeyard keeps its data in four places.

| Data | Where | Notes |
| --- | --- | --- |
| Accounts, sessions, repository metadata, issues, CI runs and logs, file records | SQLite file `<state dir>/gitserv.db`, or PostgreSQL when `DATABASE_URL` is set | One baseline schema per engine in `schema/sqlite/0001.schema.sql` and `schema/postgres/0001.schema.sql`. |
| Git repositories | Bare repositories at `<REPOS_PATH>/<uuid>.git` on their owning storage node | `repositories.storage_node_id` selects the node. NULL uses the stored local absolute path; clustered storage derives the path from its own root and the UUID. |
| Uploads (avatars, attachments, release assets) | `<state dir>/objects/uploads/<uuid>`, or the S3-compatible bucket when any `S3_*` variable is set | One object store serves every kind of file. With S3 the server creates the bucket at start-up when it is missing and gives it a CORS rule that lets `PUBLIC_URL` upload from the browser. An existing bucket needs that rule set by hand. |
| Built-in SSH host key | The database, and `HOST_KEY_PATH` (default `./host_key`) | Written with owner-only permissions. |

The state directory defaults to `./data` (`GITSERV_STATE_DIR`). It holds the SQLite file
and the local `objects/` directory.

## Choosing the database

- `DATABASE_URL` unset: SQLite at `<state dir>/gitserv.db`, created when missing.
- `postgres://` or `postgresql://`: PostgreSQL.
- `sqlite:`: a SQLite URL.

The setting is read on every start and nothing records the choice. Starting with a
different database does not move data. A missing database is created empty. For example,
an installation whose `gitserv.db` is directly in `/data` that starts with
`GITSERV_STATE_DIR` set to `/data/state` gets a new, empty database.

## Repository storage across nodes

A PostgreSQL installation can pair storage nodes over iroh. New repositories
use the online storage node with the most reported free space, which may be this
node; forks stay on their source's node. In a cluster the owner is always recorded,
so creation fails when no online storage node reports free space. A single-node
installation records no storage node and uses local storage, also when its node
connection is open for runners.
Pages, file saves, merges and streamed clone/fetch/push sessions use the same
`RepoStore` interface on either node. Host OpenSSH serves only local repositories;
HTTP and built-in SSH can serve remote repositories. See [Architecture](ARCHITECTURE.md) for RPC framing and pairing.

## Repository maintenance

Git's automatic `gc` is off for the Git commands the server runs, so the server packs
repositories itself. After accepted pushes and server-side ref publications (including
editor saves, uploads and merges), it counts the loose objects in the
repository, stopping at 6,700 (Git's default `gc.auto`). Above that, it starts
`git gc --auto` for the repository in the background, after the push has been answered.
A replica copy is checked after each transfer into it, and packed the same way when it
has more than 50 packs (Git's `gc.autoPackLimit`). That run happens inside the copy's own
transfer job, so no other transfer writes the copy meanwhile. A run that takes longer than
10 minutes is killed. At most one run per repository is queued or running, and two runs go
at a time; a replica copy that finds both busy is checked again after its next transfer.
A failed run is logged and does not change the push or the copy. A run only packs: it
never prunes, so an unreachable commit stays readable for closed pull requests and queued
CI runs, and disk space is not reclaimed automatically. Do not run pruning as routine
maintenance: database-referenced commits are not all protected by Git refs, including
closed, unmerged same-repository pull requests after branch deletion. Pruning can
permanently destroy their historical diffs even with no open pull request or queued run.
Before pruning, protect every database-referenced commit with a Git ref and stop writes;
the application does not currently automate this.

## Uploads

1. **Reserve.** `POST /api/files` checks the quota and records a pending file. The
   response has `file_id`, `upload_url`, `headers` and `capability`.
2. **Upload.** With local storage, `PUT /api/files/{file_id}/content` with the session
   cookie and `X-CSRF-Token`. With S3, a presigned `PUT` to the bucket. It is signed over
   the SHA-256 checksum, the length, the content type and `If-None-Match: *`.
3. **Confirm.** `POST /api/files/confirm` checks the size, the SHA-256 checksum and the
   file signature, then marks the file active.

Pending uploads expire after 900 seconds. A sweep runs every 300 seconds on nodes with the
`web` role and removes them. The same sweep removes partial uploads in `objects/.staging`
that a crash left and that have not been written to for a day. Each object is limited to
100 MiB.

On both databases, each sweep also deletes upload-accounting rows older than 730 days
and download-accounting rows older than 30 days, oldest first. Each statement deletes at
most 500 rows, and a table gets at most 20 such batches per sweep (10,000 rows), so a
larger backlog drains over successive sweeps. Upload charges survive file deletion until
this retention expires. This preserves the full 730-day admin storage report,
hourly/daily quotas and 24-hour file stats. File metadata and objects are not removed by
accounting cleanup.

For S3, `S3_PUBLIC_ENDPOINT` is the browser-facing address, and web nodes require
`PUBLIC_URL` so startup can determine the host-only cookie scope. The endpoint hostname
must be outside the session cookie scope: it cannot equal the `PUBLIC_URL` hostname for a
host-only cookie or equal/be below `COOKIE_DOMAIN`. Schemes and ports do not isolate
cookies. Sibling hosts such as `git.example.com` and `objects.example.com` are valid when
`COOKIE_DOMAIN` is unset
or limited to `git.example.com`.

## Backups

On a single-node installation, `codeyard backup <dir>` copies everything while the server
keeps serving, in this order:

1. the database: SQLite with `VACUUM INTO`, PostgreSQL with `pg_dump --format=custom`
   (the PostgreSQL client tools must be on `PATH`);
2. every repository under `REPOS_PATH`, and every kept deleted one under `deleted/`, as
   bare copies brought up to date with `git fetch`, hidden `refs/codeyard/*` included;
3. the local uploads in `objects/` (a bucket is backed up with its own tools), and
   `node.key` when the server has one.

The database comes first, so it is never newer than the Git copies. Running the command
into the same directory again copies only new objects and changed files. The directory
is private: the database holds session credentials. Keep `SECRETS_MASTER_KEY`, the rest
of `.env` and the SSH host key with it; without the key, stored Actions secrets cannot be
decrypted.

`codeyard restore <dir>` needs an empty database (a new SQLite file, or a PostgreSQL
database without tables), an empty `REPOS_PATH` and an empty upload directory. It loads
the database, migrates it, copies the repositories and uploads, points the repository rows
at the new `REPOS_PATH`, then runs `codeyard repo verify --repair` and
`codeyard repo rebuild`.

`codeyard repo verify` checks each repository with `git fsck --connectivity-only` and
compares every pull request with its `refs/pull/N/head`. With `--repair`, run while the
server is stopped, it follows Git where the database is older: an open pull request whose
ref moved records the new head, a missing ref is recreated, refs of pull requests the
database does not have are removed, and a repository whose directory is missing comes
back from `deleted/`. A commit the database names that Git does not have is listed and
makes the command fail. Run it after restoring the database alone, for example from
PostgreSQL point-in-time recovery.

`codeyard repo rebuild`, also with the server stopped, makes derived data again: missing
pack indexes, one pack with its commit graph, and each default branch's language rows.

Deleted repositories move to `REPOS_PATH/deleted/<time>-<directory>` and deleted uploads
stay queued; both are removed after 7 days, so a backup up to a week old still finds them.

Cluster backup fetches repositories and the storage-node object inventory from a stopped,
enrolled coordinator while other cores serve. Restore installs each storage identity and
keeps startup fenced until verification and the required copies finish. Follow
[Cluster backup and restore](CLUSTER.md#backup-and-restore); keep node private keys,
peer settings and `SECRETS_MASTER_KEY` in a separate secret backup. Never copy only a
live SQLite file: committed writes may still be in its WAL.

### Crash durability check

Use a disposable single-node VM with a persistent virtual disk and no unrelated data.
Push a new commit from outside the VM and record its object id there. Immediately after
the push returns successfully, hard-reset the guest through its console with
`echo b > /proc/sysrq-trigger`, without a preceding `sync` or orderly shutdown.
After boot, clone the repository from outside the VM, check that the recorded commit is
present, and run `git fsck --full` on the clone and the server's bare repository. Repeat
with SQLite and PostgreSQL on the guest. Record the filesystem, virtual disk cache mode,
database engine and commit id with the result. A process kill does not satisfy this
check; it leaves the guest's disk cache intact.

## Moving data

Changing `DATABASE_URL`, `REPOS_PATH`, `GITSERV_STATE_DIR` or the `S3_*` settings does
not copy anything. Copy the data first, then point the server at the copy. Repositories
with no storage node (every repository on a standalone install) keep the absolute path
stored in their row, so changing `REPOS_PATH` does not redirect them. Keep the same
absolute path: move the directory back to it, or bind-mount the new location there.

`codeyard db copy` copies the database between SQLite and PostgreSQL, in either direction,
except that PostgreSQL to SQLite is refused while any repository has cluster placement.
Follow the offline steps in [Running a cluster](CLUSTER.md#start-the-first-node): stop the
server, copy into an empty destination, and move repository directories, uploads and key
files separately. There is no built-in migration between local and S3 storage.

### Importing Git repositories

Use **Import an existing repository** on the new repository page (`/new/migration`) to
create a repository and start a background Git import. The repository page shows progress
and links to retry or cancel a failed attempt; source credentials must be entered again.

Repository administrators can also start a Git import with `PUT /api/v3/repos/{owner}/{repo}/import`
using a write token and `{"vcs_url":"https://host/owner/repository.git","vcs":"git"}`.
The destination must be empty. `GET` reports progress, `PATCH` retries a failed attempt,
and `DELETE` cancels the attempt without removing Git data already published. Optional
`vcs_username` and `vcs_password` authenticate to the source; they are never stored.
Branches, tags, other advertised refs and their objects are preserved, with a 128 MiB
pack limit and a fifteen-minute deadline. Issues, releases, wikis and other forge metadata
are not imported yet. After a server restart, an interrupted attempt becomes retryable
within twenty minutes.

Sources must use HTTPS and resolve only to public addresses. Connections are pinned to
those addresses and redirects are refused. For a trusted private source, an administrator
may set `MIGRATIONS_ALLOW_PRIVATE=true`; this also permits HTTP. Do not enable it on an
untrusted installation: users who administer repositories could reach internal services.

### Exporting repositories

Repository owners can create an export with `POST /api/v3/user/migrations` and
`{"repositories":["repository-name"],"lock_repositories":true}`. Organization owners use
`POST /api/v3/orgs/{org}/migrations`. List and status endpoints report the durable job;
the archive endpoint redirects to an authenticated tar.gz download when it is ready.
Read tokens can inspect and download exports, but cannot create them, delete archives
or unlock repositories. Organization membership alone does not grant export authority.

Archives contain bare Git repositories, repository records, issues and comments,
labels, milestones, pull requests and submitted reviews, releases and release assets.
Owner-scoped projects with their field configurations, items (content links, draft text,
field values, order and archive state) and saved views are included unless
`exclude_owner_projects` or `exclude_metadata` is set. Organization exports also include organization
details, memberships, team parents, and direct team memberships and repository grants;
inherited access is not flattened into additional grants.
The exclusion options omit their respective data; `org_metadata_only` requires an empty
repository selection. Archives are limited to 100 MiB compressed and 512 MiB unpacked,
with a fifteen-minute worker deadline. Source URLs and credentials are not copied into
Git configuration. Webhook secrets and private account profile fields are not exported;
Git author identities are preserved.

Migration locks block Git writes and repository-content mutations until explicitly
removed with `DELETE .../migrations/{id}/repos/{name}/lock`, even after archive deletion
or export failure. Deleting another selected repository does not prevent unlocking a
surviving repository. Downloads recheck scope and repository ownership; migration
history remains manageable when a repository is deleted or transferred. Failed upload
cleanup is durable and delayed to outlive an in-flight upload.

## Compose

In `compose.yaml` the state directory is `/data/state` and repositories are in
`/data/repos`. An installation whose `gitserv.db` is directly in `/data` must set
`GITSERV_STATE_DIR=/data`. Nothing is moved automatically.
