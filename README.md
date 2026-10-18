# Codeyard

![Codeyard](branding/banner.png)

Website: https://codeyard.run

Codeyard is a self-hosted Git forge written in Rust. It serves repositories over SSH
and HTTP, and provides issues, pull requests with reviews and merges, a web file
editor, organizations, notifications, and CI runs in the style of GitHub Actions
(workflows run through `act`).

The application and executable are `codeyard`. The Cargo package is `codeyard`,
because Cargo does not accept package names that start with a digit.

## Quick start

You need a Rust toolchain, the Dioxus CLI 0.7.10 and the standalone Tailwind CSS 4.1.18.
[scripts/build-webui.sh](scripts/build-webui.sh) reads the locations of both tools from
the `DX` and `TAILWIND` variables.

```bash
DX=/path/to/dx TAILWIND=/path/to/tailwindcss scripts/build-webui.sh release
cargo build --release -p codeyard --bin codeyard

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

Cluster mode needs PostgreSQL. SQLite runs a single node, and `codeyard db copy` moves a
database between the two while the server is stopped. Node roles, below, split web, SSH,
storage and CI across separate machines.

`GITSERV_STATE_DIR` chooses where `gitserv.db` and uploads are kept. The container default
is `/data/state`; set `GITSERV_STATE_DIR=/data` to put `gitserv.db` directly in `/data`.

## Node roles and CI

`GITSERV_NODE_ROLES` lists the services a node runs: `web`, `storage`, `ssh` and
`actions-worker`. The default is all four, which is a complete single machine. In a
cluster nodes take separate roles: web and SSH
servers, machines that only hold repositories, and CI workers that need no database.

CI jobs need `act` (`CI_ACT_BINARY`, found on `PATH` by default) and, in the default
`CI_EXECUTION_MODE=podman`, a Podman socket (`CI_PODMAN_SOCKET`). Let only trusted
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
  or pushes, create a personal access token in **Settings → Access tokens**. Choose
  `read` for clone/fetch or `write` for clone/fetch/push. Git asks for a username
  (any value) and password: enter the token as the password, not your account password.
  Tokens are shown once and can be given an expiry or deleted in settings.

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
        proxy_set_header Upgrade $http_upgrade;
        proxy_set_header Connection $connection_upgrade;
    }
}
```

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

## Docker Compose

`compose.yaml` starts PostgreSQL, an S3-compatible store (RustFS) and the server. The
server image is built from [Containerfile](Containerfile),
which packages a release prepared in `dist/release/` (see the top of that file);
`scripts/release.sh` builds the release and the image. Set `PUBLIC_URL`,
`SECRETS_MASTER_KEY`, `S3_ACCESS_KEY` and `S3_SECRET_KEY` in `.env` before starting it.
The server creates the bucket on first start.

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

Artwork and its provenance are in `src/webui/assets/` and
[branding/README.md](branding/README.md).

## License

Codeyard is free software under the GNU Lesser General Public License, version 3 or (at your
option) any later version. The license text is in [LICENSE.md](LICENSE.md). You may use,
change, fork and redistribute it under those terms.

### Contribution

Unless you explicitly state otherwise, any contribution you intentionally submit for inclusion
in this project is licensed under the same terms (LGPL-3.0-or-later), without additional terms
or conditions.
