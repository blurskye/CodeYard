# Codeyard

![Codeyard](branding/banner.png)

Website: https://codeyard.run

Codeyard is a self-hosted Git forge written in Rust. It serves repositories over SSH
and HTTP, and provides issues, pull requests with reviews and merges, a web file
editor, organizations, notifications, and CI runs in the style of GitHub Actions
(workflows run through `act`).

![From the home page to a repository, then a workflow run from start to finish](branding/demo.gif)

![The admin panel: activity charts, users, site settings, email and the audit log](branding/admin.gif)

## Quick start

You need a Rust toolchain, the Dioxus CLI 0.7.10 and the standalone Tailwind CSS 4.1.18.
[scripts/build-webui.sh](scripts/build-webui.sh) reads the locations of both tools from
the `DX` and `TAILWIND` variables.

```bash
DX=/path/to/dx TAILWIND=/path/to/tailwindcss scripts/build-webui.sh release
cargo build --release -p codeyard -p codeyard-actions --bin codeyard --bin codeyard-actions

WEBUI_DIST="$PWD/dist/webui" PUBLIC_URL=http://localhost:3000 \
  ./target/release/codeyard serve
```

Open <http://localhost:3000/setup> and create the first administrator. By default
`serve` keeps the SQLite database and uploads in `./data` and the repositories in
`./repos`. It creates the database and runs its migrations on every start. The first
administrator can also be created locally with
`codeyard user bootstrap <username> --password-stdin [--mode public|private]`.

## Storage

| What | Default | Change it with |
| --- | --- | --- |
| Database | SQLite file `<state dir>/gitserv.db` | `DATABASE_URL=postgres://user:password@host:5432/name` |
| Uploads | `<state dir>/objects` | `S3_ENDPOINT`, `S3_ACCESS_KEY`, `S3_SECRET_KEY`, `S3_BUCKET` (optional: `S3_REGION`, `S3_PUBLIC_ENDPOINT`) |
| Repositories | `<REPOS_PATH>/<uuid>.git`, `REPOS_PATH` defaults to `./repos` | `REPOS_PATH` |
| State directory | `./data` | `GITSERV_STATE_DIR` |

Setting any `S3_` variable turns S3 on, and then `S3_ENDPOINT`, `S3_ACCESS_KEY`,
`S3_SECRET_KEY` and `S3_BUCKET` are all required. A web node using S3 also requires
`PUBLIC_URL`. `S3_PUBLIC_ENDPOINT` must use a hostname outside the session cookie scope;
another port on the forge hostname is not isolated. Host-only cookies allow sibling
hosts, while `COOKIE_DOMAIN` also covers its subdomains.

Runners on other machines and cluster mode need PostgreSQL. SQLite runs a single node,
and `codeyard db copy` moves a database between the two while the server is stopped.

`GITSERV_STATE_DIR` chooses where `gitserv.db` and uploads are kept. The container default
is `/data/state`; set `GITSERV_STATE_DIR=/data` to put `gitserv.db` directly in `/data`.

## Node roles and CI

`GITSERV_NODE_ROLES` says what a machine runs: `server` (web pages, API, Git storage
and SSH) and `runner` (Actions jobs). The default is both, a complete single machine.
A runner can also run alone on another machine with no database: create a ticket with
`codeyard cluster invite --roles runner`, put it in `CLUSTER_JOIN_TOKEN` on that machine
and start `codeyard serve --roles runner` (see
[docs/CI_LOCAL_RUNNER.md](docs/CI_LOCAL_RUNNER.md)). Splitting the server itself across
machines is cluster mode, described in [docs/CLUSTER.md](docs/CLUSTER.md).

CI jobs run on the in-house runner `codeyard-actions`, which is the default engine. It must
sit next to `codeyard`, be on `PATH`, or be named by `CI_ACTIONS_BINARY`. It starts job
containers with the `podman` command, so a Podman client must be on `PATH` too. The `act`
engine needs `act` (`CI_ACT_BINARY`, found on `PATH` by default). Both use a Podman socket
(`CI_PODMAN_SOCKET`) in the default `CI_EXECUTION_MODE=podman`. Let only trusted
users run workflows (`CI_TRUSTED_ACTOR_IDS`, or site administrators). Anyone who can use
the Podman socket can run code as your host user, and `CI_EXECUTION_MODE=local` runs jobs
on the host with no container isolation.

## Git access

- **SSH.** The default `SSH_MODE=auto` uses host OpenSSH on a host that runs sshd and
  has Codeyard's OpenSSH setup installed (`scripts/install-openssh.py`). Without
  a host sshd it runs the built-in server on `SSH_ADDR` (default `0.0.0.0:2222`).
  Inside containers SSH stays off unless `SSH_MODE` is set explicitly. Host OpenSSH serves
  only repositories stored on its own node; for remote repositories in a cluster, use
  built-in SSH or HTTP(S), which reach any serving node.

  ```bash
  git clone ssh://git@your-host:2222/owner/repo.git
  ```

- **HTTPS.** Public repositories can be cloned anonymously. For private repositories
  or pushes, create a personal access token in **Settings → Tokens (classic)**. Choose
  `read` for clone/fetch or `write` for clone/fetch/push. Git asks for a username
  (any value) and password: enter the token as the password, not your account password.
  Tokens are shown once and can be given an expiry or deleted in settings. **Settings →
  Fine-grained tokens** makes tokens limited to chosen repositories and permissions.

  ```bash
  git clone https://your-host/owner/repo.git
  git push origin main
  ```

  Serve HTTPS through your reverse proxy; do not send tokens over unencrypted HTTP.
  HTTP pushes obey the same repository permissions and branch protection as SSH.

## Reverse proxy and HTTPS

The server speaks plain HTTP. Put a proxy in front for HTTPS, and set `PUBLIC_URL` to the
public address. Git pushes and clones are large, long requests and the live pages use
WebSockets, so the proxy must not buffer, cap or time out either. With Caddy this is the
default:

```caddyfile
git.example.com {
    reverse_proxy 127.0.0.1:3000
}
```

With nginx:

```nginx
map $http_upgrade $connection_upgrade { default upgrade; '' close; }

server {
    listen 443 ssl;
    server_name git.example.com;
    client_max_body_size 0;          # no limit on pushes and uploads
    proxy_request_buffering off;     # stream pushes to the server
    proxy_buffering off;             # stream clones to the client
    proxy_read_timeout 3600s;
    proxy_send_timeout 3600s;
    location / {
        proxy_pass http://127.0.0.1:3000;
        proxy_http_version 1.1;
        proxy_set_header Host $host;
        proxy_set_header X-Forwarded-For $proxy_add_x_forwarded_for;
        proxy_set_header Upgrade $http_upgrade;
        proxy_set_header Connection $connection_upgrade;
    }
}
```

Failed sign-ins are limited per client address. Set `HTTP_TRUSTED_PROXY_IPS` to the proxy's
address (for example `127.0.0.1`) so the server reads the client from `X-Forwarded-For`;
without it every visitor shares the proxy's address and its limit.

### Packages

Packages use the same host: OCI at `/v2/`, npm at `/api/packages/OWNER/npm/`,
Maven/Gradle at `/OWNER/REPOSITORY/`, NuGet v3 at
`/api/packages/OWNER/nuget/index.json`, and RubyGems at
`/api/packages/OWNER/rubygems/`.
Use a personal access token (or an Actions `GITHUB_TOKEN` with `packages` permission)
for registry authentication. For example, after `docker login HOST`, push with
`docker push HOST/OWNER/IMAGE:TAG`. OCI namespaces must use lowercase ASCII account
or organization names. For npm, set
`@OWNER:registry=https://HOST/api/packages/OWNER/npm/` and
`//HOST/api/packages/OWNER/npm/:_authToken=${CODEYARD_TOKEN}` in `.npmrc`.

The reverse proxy must allow large request bodies and long uploads on registry paths,
including Maven's repository-layout PUTs, not just Git routes; the nginx configuration
above already does. Objects and package archives have a 100 MiB limit; XML and metadata
extracted from archives are limited to 1 MiB. Upload requests time out after 120 seconds.
Packages start private; their settings
control visibility and the linked repository. Deletion retains versions for REST
recovery for 30 days, then the upload sweep collects unreferenced objects.

Maven and Gradle use HTTP Basic authentication: put the account name and token in
Maven's `settings.xml` server entry (with the same id as `distributionManagement`),
or in Gradle's repository `credentials`. Set the repository URL to
`https://HOST/OWNER/REPOSITORY`. Coordinates are `groupId:artifactId`; the URL's
repository must belong to the owner and the publishing credential must have write
access. Releases cannot overwrite an existing artifact with different bytes;
SNAPSHOT artifacts and `maven-metadata.xml` can be updated. POMs, JARs, classifiers,
Gradle module metadata and MD5/SHA-1/SHA-256/SHA-512 sidecars use repository layout.
Plugin-prefix group metadata is accepted after its plugin artifact is published.
Metadata is publisher-supplied; after manual version deletion, re-publish repository
metadata when needed.

For NuGet, register `https://HOST/api/packages/OWNER/nuget/index.json` with
`dotnet nuget add source https://HOST/api/packages/OWNER/nuget/index.json --name codeyard --username USERNAME --password "$CODEYARD_TOKEN"`
(add `--store-password-in-clear-text` only when the client cannot encrypt its config).
Publish with `dotnet nuget push PACKAGE.nupkg --source codeyard --api-key "$CODEYARD_TOKEN"`.
The API key is the PAT or job token, not an independent registry secret. The v3 index
advertises publish, flat-container, registration and search resources. Symbol servers
and `.snupkg` publication are not implemented. NuGet unlisting is unsupported; use
Packages REST/pages for deletion and restoration. Search download counts record authorized
package-download responses, not metadata probes or completed installations.

For RubyGems, set a named API key in `~/.gem/credentials` (mode `0600`), for example
`:codeyard: TOKEN`, then use
`gem push PACKAGE.gem --key codeyard --host https://HOST/api/packages/OWNER/rubygems`.
Configure Bundler's credentials for that same source URL as `USERNAME:TOKEN`, and use
`source "https://HOST/api/packages/OWNER/rubygems/"` in the Gemfile. Bundler uses the
compact `/versions` and `/info/NAME` indexes, including platform variants, dependencies,
archive checksums, conditional requests and byte ranges. The legacy Marshal indexes,
yank API, and gem-signing verification are not implemented; archive bytes and signatures
are retained unchanged for client-side verification.

### Artifact attestations and inventory

The GitHub-compatible API stores Sigstore DSSE bundles once and indexes their SHA-256/SHA-512
subjects across repository associations. Repository and organization reads filter current
repository visibility before cursor pagination. Organization deletion removes only associations
on repositories the caller may write; unreferenced bundles are collected in bounded sweeps.
Browse `/OWNER/REPOSITORY/attestations` (also linked from Insights) or `/orgs/ORG/attestations`
for native, JavaScript-optional filters, bundle downloads and CSRF-protected deletion.
Personal repository attestations are also available at `/users/USERNAME/attestations`,
with the matching GitHub REST list and deletion operations. Docker conflict listings return
an empty collection: Codeyard publishes directly to its container registry, without a legacy
Docker registry migration.

Bundles are **stored, not cryptographically verified**. Structural validation is bounded to 1 MiB,
100 subjects and 32 signatures. Validate signatures, trusted roots and the expected signer separately.
The API returns inline `bundle` values without `bundle_url`: current `gh attestation verify` falls
back to that field, while its unauthenticated external-URL transport expects Snappy-compressed data.
Downloaded JSON can also be supplied with `--bundle` and an explicit expected repository/owner.
Serving a bundle does not make Codeyard its Sigstore issuer or establish a verification verdict.
Pages budget 16 MiB of bundle data; an oversized single bulk expansion returns 413 and requires
fewer requested digests. No real `gh` signature/trust-policy verification is claimed by the fixtures.

Organization artifact deployment/storage metadata uses the same live provenance repository
associations. Explicit `github_repository` is a fallback only when no SLSA provenance is stored;
only currently writable associations are eligible, and stable deployment identities also recheck
any prior repository. Digest identities retain their original artifact name/version. Cluster batches
create or update the named deployments; omitted records are unchanged. Partial batches leave rejected
records unchanged, and empty or all-rejected batches are no-ops. Requests are bounded
to 16 MiB, synchronous batches to 1,000 deployments and durable jobs to 5,000. Every web node may
resume pending jobs; inventory changes and terminal job results commit together, with the original
credential and current repository authority checked again. Storage writes support at most 1,000 eligible
repository associations. Unpaged inventory reads over 16 MiB/10,000 records return 413, not a silently
truncated inventory. Repository transfers discard inventory
from the former namespace rather than transferring it or permanently reserving its deployment names.
Actions tokens need `attestations` or `artifact-metadata` permissions and remain scoped to their own
repository. REST requires a token, and read credentials cannot mutate this state.

## Configuration

All settings are environment variables. The server also reads a `.env` file at startup.
[.env.example](.env.example) lists every setting the server reads, with its default.
Credential-bearing database URLs are accepted only through `DATABASE_URL`, never a CLI
argument.

- `PUBLIC_URL`: the public address, for example `https://git.example.com`. With an
  `https://` address, session cookies are always marked `Secure`.
- `PUBLIC_HTTP_CLONE_BASE`: optional. The HTTP clone address shown in the UI. It
  defaults to `PUBLIC_URL`, and to `http://localhost:3000` when neither is set.
- `SECRETS_MASTER_KEY`: 64 hexadecimal characters that encrypt Actions secrets.
  Generate it once (for example with `openssl rand -hex 32`) and keep it. A different
  key makes stored secrets unreadable. Without it the secrets feature is off.
- `ADMIN_SECRET`: optional. When set, the `/setup` form requires this exact value.

## Email

Codeyard can send mail through an SMTP server: notification emails to users who have a
verified address and the setting on (Settings, Email), and the approval or rejection of
a signup request. Set the server under Admin, Email, which also sends a test message.
The settings are stored in the database, so every node uses them. Links in the mail are
built from `PUBLIC_URL`; without it, mail has no links. A saved password is encrypted with
`SECRETS_MASTER_KEY` and is never shown again; leaving the field empty keeps it. Mail is
queued and retried for a few attempts, and its text is deleted once it is sent or given up.

With mail set up, a new or changed email address gets a confirmation code (good for 24
hours), to paste under Settings, Email, which also has a resend button. "Forgot password?"
mails a reset code (good for one hour, once) to an account with a verified address, and
the next page takes that code and the new password. With `PUBLIC_URL` set, the same mail
also has a link that does the same. A new request within a minute of the last one sends
nothing, and changing the password in Settings cancels a pending reset. Without a mail
server neither is sent, and the pages say so.

On a fresh install the settings can come from the environment instead:
`CODEYARD_SMTP_HOST`, `CODEYARD_SMTP_PORT` (default by security), `CODEYARD_SMTP_SECURITY`
(`starttls` by default, `tls` or `none`), `CODEYARD_SMTP_USERNAME`, `CODEYARD_SMTP_PASSWORD`,
`CODEYARD_SMTP_FROM` (an address or `Name <address>`) and `CODEYARD_SMTP_REPLY_TO`. Once
settings are saved from the admin page, the database wins.

## Docker Compose

`compose.yaml` starts PostgreSQL, an S3-compatible store (RustFS) and the server. The
server image is built from [Containerfile](Containerfile),
which packages a release prepared in `dist/release/` (see the top of that file);
`scripts/release.sh` builds the release and the image. Set `PUBLIC_URL`,
`SECRETS_MASTER_KEY`, `S3_ACCESS_KEY`, `S3_SECRET_KEY`, `DATABASE_URL` and
`S3_PUBLIC_ENDPOINT` in `.env` before starting it. Set PostgreSQL's `POSTGRES_USER`,
`POSTGRES_PASSWORD` and `POSTGRES_DB` as needed, and supply the matching `DATABASE_URL`
using hostname `postgres` and percent-encoded credentials (literal password `p%40ss`
becomes `p%2540ss` in the URL). The server creates the bucket on first start.

For production, use separate HTTPS hostnames for the forge and object API, such as
`PUBLIC_URL=https://forge.example.com` and `S3_PUBLIC_ENDPOINT=https://objects.example.net`.
Proxy the latter to RustFS's API port (`RUSTFS_API_PORT`, default 11002), not its console.
The object hostname must be outside the session cookie scope and reachable by browsers;
`http://objects.localhost:11002` is only suitable for local development.

Container HTTP and SSH listeners stay on ports 3000 and 2222. Change the published host
ports with `API_HTTP_PORT` and `SSH_PORT` (and bind interfaces with `API_HTTP_BIND` and
`SSH_BIND`), not `HTTP_ADDR` or `SSH_ADDR`.

The file mounts the Podman socket that runs CI jobs (`CI_PODMAN_SOCKET`, default
`/run/user/1000/podman/podman.sock`), so the socket must exist even when you use
`CI_EXECUTION_MODE=local`. Start it with `systemctl --user enable --now podman.socket`
and set `CI_PODMAN_SOCKET` if your user id is not 1000.

## Development

- Build and test with Cargo on Linux (Rust, Git, OpenSSH and PostgreSQL are needed):
  `cargo test --locked --all-targets`, with `GITSERV_TEST_DATABASE_URL` set to a PostgreSQL
  URL. `bash scripts/e2e.sh` is the end-to-end check, and `bash scripts/s3-tests.sh` runs
  the S3 tests against a throwaway RustFS. `scripts/e2e.sh` also needs `act` 0.2.89
  (`E2E_ACT_BINARY`) and the Dioxus CLI (`DX`); `scripts/s3-tests.sh` needs a RustFS
  binary (`RUSTFS_BIN`).

## Status

Early-stage software. It has not had an independent security audit. Read the known limits in
[CHANGELOG.md](CHANGELOG.md) before you expose an installation to people you do not trust.

Artwork and its provenance are in `webui/src/assets/` and
[branding/README.md](branding/README.md).

## License

Codeyard is free software under the GNU Lesser General Public License, version 3 or (at your
option) any later version. The license text is in [LICENSE.md](LICENSE.md). You may use,
change, fork and redistribute it under those terms.

### Contribution

Unless you explicitly state otherwise, any contribution you intentionally submit for inclusion
in this project is licensed under the same terms (LGPL-3.0-or-later), without additional terms
or conditions.
