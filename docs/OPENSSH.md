# Opt-in host OpenSSH integration

Native OpenSSH integration uses
**the existing host sshd**, not a second listener on port 22. Normal root/administrator
SSH remains host policy.
Installing this integration is an explicit deployment operation, never an app
startup side effect. It does not change SSR, hydration, branding or HTTP routing.

## Runtime selection and setup

The deployment contract is `SSH_MODE=auto|openssh|builtin|disabled`, default
`auto`. Auto detects an active host `ssh`/`sshd` systemd service or sshd process;
when OpenSSH is detected it does **not** start the built-in listener. Missing
`/etc/codeyard/openssh.json` means **setup required**, not permission to rewrite
sshd. `openssh` explicitly selects the broker integration; `builtin` explicitly
selects the built-in SSH server (default port **2222**); `disabled` disables SSH.
Without an active daemon, native auto mode selects built-in SSH on 2222. When
host detection is uncertain (including containers), auto opens no SSH listener
and asks for an explicit override. Stock Compose selects `builtin` explicitly.
Detection/wiring is separate from the installer. No mode automatically binds port 22,
elevates privileges or installs/reloads host configuration.

FIDO security keys (`sk-*`) require host OpenSSH. The built-in SSH verifier cannot
enforce user presence and refuses these keys; enrollment remains available for host mode.

Host OpenSSH serves only repositories stored on its own node. It refuses remote
repositories with "Host OpenSSH cannot serve remote repositories; use HTTP or
built-in SSH". Use smart HTTP or `SSH_MODE=builtin` to push, clone and fetch through
any serving node; those transports stream Git sessions to the storage node.

Set the app's `SSH_SOCKET_PATH` to the **same absolute socket path** used by the
installer, and `SSH_GIT_USER` to its `--git-user` (default: `git`). `SSH_INSTALL_MANIFEST` defaults to `/etc/codeyard/openssh.json`.
Restart the app explicitly after installation or changing these settings: mode
selection happens once at startup. Set `PUBLIC_SSH_CLONE_BASE` to the real public
endpoint, e.g. `ssh://git@git.example.com:22`; host mode does not derive public
ports/hostnames from `SSH_ADDR`, which only controls the built-in listener. The native app must run as the configured `--service-user`, own the
broker socket, and recreate its private 0700 socket directory after reboot
(`/run` is transient). Arrange that in the app's own service deployment, for
example systemd `RuntimeDirectory=codeyard`, `RuntimeDirectoryMode=0700` with
`User=gitserv`. The installer does not start, restart or change the app's mode.
Publishing a manifest proves configuration validation/reload, not that a broker
or Git transport has been live-tested. Existing PAM/account-lock/SELinux policy
must also allow the intended login; the installer never weakens those controls.

## Trust boundary and prerequisites

- Existing, distinct, **non-root** service and Git OS accounts; root and the
  administrative identities must not be the Git/service accounts. No users are
  created. The Git account needs an executable working login shell for sshd's
  forced command, not `nologin`/`false`; make any shell/account changes yourself.
- An installed root-owned, world-executable `codeyard` binary, with root-owned,
  non-group/world-writable and world-traversable parents, no symlink components,
  and no setid bits. The service and Git users must not be able to replace it
  or the helpers. “Immutable” here
  means immutable to those users, not a requirement for a filesystem `chattr` flag.
- Linux native host, one active, reloadable `ssh.service` or `sshd.service`, direct
  sshd ExecStart, and regular root-owned config files. The installer never starts
  or restarts sshd. Command-line auth overrides, wrappers, ambiguous services,
  pending systemd daemon-reload, or an executable replaced since daemon startup
  require manual integration first. Common `$SSHD_OPTS` expansion is supported
  only as simple words from the running daemon environment, then checked against
  the narrow allowed flags (`-D`, `-e`, `-q`, `-f CONFIG`). A missing/unverifiable
  running `SSHD_OPTS` value causes refusal, not an assumption that it was empty.
  No env file is sourced.
- Existing `/etc/sudoers.d` inclusion in `/etc/sudoers`, and **sudo >= 1.9.10**
  (anchored command-argument regex support), `visudo`, sshd and Python 3.8 or newer. System tool symlinks are allowed only with root-safe
  links/targets; operator-selected binary/config/helper paths reject symlinks.
- A private socket directory owned by the service UID with mode **0700**; an
  existing socket must be service-owned with no group/world permissions. Only a
  missing leaf directory under an already-safe root-owned parent may be created,
  after config validation. No unrelated directory is chowned/chmodded.

The socket broker authorizes same-UID clients. **A trusted local OS service user
can connect to it**; other processes running under that UID share its authority.
This is not an untrusted-workflow sandbox. In particular, access to a rootless
Podman socket is host-user execution, not a confinement guarantee. Use separate
OS identities/execution boundaries when workflows are untrusted.

Database credentials belong only in the app daemon. Neither helper stores,
sources or forwards them. Helpers use Python `-I`, fixed absolute executables,
fixed arguments, an explicit `/` working directory and cleaned environments;
they do not load a user `.env` or search a caller-controlled PATH.

## Dry-run, review, apply

Keep an existing root/admin session open, verify another admin can log in, and
have console/recovery access **before** applying. This document is not permission
to change an existing production server. Deploy a reviewed binary separately first.

```sh
# Read-only plan + static preflight; no subprocesses or validation/reload commands.
# Root may be needed merely to read sshd/sudoers policy.
/usr/bin/python3 -I scripts/install-openssh.py --admin-user YOUR_EXISTING_ADMIN

# Explicit authorization, only after reviewing the exact plan and host conflicts.
sudo /usr/bin/python3 -I scripts/install-openssh.py --admin-user YOUR_EXISTING_ADMIN --apply
```

Defaults (all configurable options shown):

```text
--binary       /usr/local/bin/codeyard
--service-user gitserv
--git-user     git
--socket       /run/codeyard/ssh.sock
--helper-dir   /etc/codeyard
--sshd-config  /etc/ssh/sshd_config
--sshd         /usr/sbin/sshd
--admin-user   NAME                 repeatable; root always checked
--apply                            absent means dry-run
```

`SUDO_USER`, if present, is also preserved/checked. Names and paths are strictly
validated; paths must be absolute, canonical and contain only simple safe ASCII
path characters. A custom helper directory needs an existing root-safe parent.
Managed config and manifest locations remain fixed at `/etc/codeyard`.
Dry-run prints all snippets even if static preflight then refuses a prerequisite;
a refusal exits nonzero and makes no changes. It never runs sshd, sudo, visudo or
systemctl. Apply requires root and validates the active service/config binding.

### Existing Forgejo or access policy: stop

The real host may already use `git` for Forgejo or another application. The
installer refuses nonempty Git authorized-keys files, another AuthorizedKeysCommand,
ForceCommand, certificate/principal integration, **any prior Match block**, and
AllowUsers/AllowGroups/DenyUsers/DenyGroups restrictions. Global
`PermitUserEnvironment` must already be off (the default); permissive settings
are refused, not rewritten. The installer does not weaken these controls
or silently disable another application's login. Choose an **existing separate
Git username** with `--git-user`, or have an operator integrate policies manually.
Even a benign admin-only prior Match needs manual review; this deliberate
conservatism avoids pretending a partial sshd parser can prove every context.

## Managed files and acceptance

Default files, all root-owned and non-group/world-writable:

| Path | Mode | Purpose |
| --- | --- | --- |
| `/etc/codeyard/ssh-keys` | 0755 | AuthorizedKeysCommand helper, run directly as `gitserv` |
| `/etc/codeyard/ssh-serve` | 0755 | Forced helper, validates exactly one positive signed-64-bit decimal key ID, or `deploy:` and a deploy key ID |
| `/etc/codeyard/sshd.conf` | 0644 | Git-user-only Match block |
| `/etc/sudoers.d/codeyard` | 0440 | Narrow, anchored `ssh serve` permission |
| `/etc/codeyard/openssh.json` | 0644 | Successful-setup manifest, written last |
| `/etc/ssh/sshd_config.codeyard.bak` | 0600 | Exact original main-config backup, retained on rollback |

The keys helper executes the frozen contract:

```text
codeyard ssh keys --socket ABS --expected-user GITUSER --serve-command ABS_SERVE_HELPER %u %t %k
```

That CLI prints restricted authorized keys forcing `ABS_SERVE_HELPER KEY_ID`.
The serve helper accepts **one** argument (no extra options/arguments): a decimal key ID,
or `deploy:` followed by a decimal deploy key ID. It uses
`sudo -n -u SERVICEUSER --preserve-env=SSH_ORIGINAL_COMMAND,GIT_PROTOCOL --`
to execute only:

```text
codeyard ssh serve --socket ABS --key-id POSITIVE_ID
codeyard ssh serve --socket ABS --deploy-key-id POSITIVE_ID
```

Only `SSH_ORIGINAL_COMMAND` and optional `GIT_PROTOCOL` are passed to sudo. The
sudoers grant allows only `ssh serve --socket ABS --(key|deploy-key)-id [1-9][0-9]*` as
an anchored argument regex, **not** a whitespace-consuming
wildcard or broad SETENV/shell/app-subcommand permission. The app CLI must also
reject extra/invalid arguments. Existing unrelated sudo privileges are not audited
or removed; keep the Git OS account otherwise unprivileged.

The Match block requires public-key authentication only, `AuthorizedKeysFile none`
(no local-key bypass), the keys helper with `%u %t %k`, the service user,
`DisableForwarding yes`, `PermitTTY no`, and `PermitUserRC no`. Effective
`PermitUserEnvironment no` is verified without changing global policy (that
option is not portable inside Match). Git's Match accepts `GIT_PROTOCOL` for optional
Git protocol negotiation; helpers discard every other inherited variable. It ends
with `Match all`. An exact marked Include is appended **only at EOF** of the main
sshd config, never at its top: a top Include
containing Match can make following global directives invalid. Earlier includes
that could already activate the managed snippet are refused.

Apply validates the scoped sudoers with `visudo` before installation and validates
full sudoers afterwards. It runs full `sshd -t`, compares `sshd -T -C` against the
required Git policy, and compares the **entire** effective config for root and
all specified admin identities before/after. Tests use a loopback context; because
prior Match blocks are refused, the only introduced context selector is Git's
username. It then reloads the selected active service if managed files changed.
No acceptance manifest is published before validation/reload succeeds:

```json
{"schema":1,"git_user":"git","service_user":"gitserv","socket":"/run/codeyard/ssh.sock","serve_command":"/etc/codeyard/ssh-serve","binary":"/usr/local/bin/codeyard"}
```

Managed names are no-clobber: existing files must have **exact expected content,
mode and root ownership**. Exact repeat installation is idempotent (validation still
runs; no config rewrite or reload). An existing manifest with missing managed
files/main inclusion is refused as an incomplete installation. Changed settings,
helpers/manifests, symlinks, unrelated files, edited markers or an unrelated
backup cause refusal; there is no
automatic upgrade, migration or uninstall. Operator changes require review.

On validation/reload/publication failure, rollback touches only this invocation's
exact file versions and still-identical empty directories; it never recursively
cleans a path. The original backup remains. If a reload was attempted, the restored
config is validated and reloaded. Concurrent administrator edits are left alone;
rollback/recovery errors are reported and may require console repair. There is no
crash/power-loss transaction guarantee: keep the backup and admin session.

## Declarative hosts and containers

NixOS/store-symlink or non-systemd layouts are **manual**, not silently rewritten.
Dry-run prints the helpers, scoped sudoers, Match block, end Include and manifest
for declarative deployment. Integrate them in the host configuration manager,
ensure the Match block is at the end of the **generated** config, validate full
sshd and Git/admin effective policy, and publish the manifest only after a
successful authorized reload. Never edit a Nix store symlink or blindly paste a
Match block into a top-level snippet directory. Distribution-generated config,
sudo versions, shells and systemd units still need operator review.

A container cannot claim auto mode sees host services or processes, so select
`SSH_MODE=openssh` explicitly when it uses host sshd. Do not run the host installer
inside a container as a substitute for host setup. For independent container SSH,
explicitly choose `builtin` and publish its 2222 port as appropriate; never infer
permission to take over host port 22.

### Podman and Compose

The stock `compose.yaml` selects `SSH_MODE=builtin` and mounts neither the broker
socket nor the manifest, so a container that uses host OpenSSH needs an override:

- Host: run the installer, then restart the app. The helpers and sudoers rule stay on the host, not in the image.
- Container: `SSH_MODE=openssh`. `SSH_SOCKET_PATH`, `SSH_GIT_USER` and
  `SSH_INSTALL_MANIFEST` must match the installer; the `compose.yaml` defaults do.
- Socket: bind-mount the broker directory `/run/codeyard` at the same path. It must
  already exist, owned by the service UID with mode 0700.
- Manifest: bind-mount `/etc/codeyard/openssh.json` read-only at the same path. Without
  it the app starts with no SSH listener and logs that setup is required.
- Serve helper: bind-mount `/etc/codeyard/ssh-serve` read-only at the same absolute
  path (or the installer's custom path). Key authorization validates this executable
  inside the container. Preserve root ownership, mode 0755 and non-writable,
  root-owned parent directories.
- UID: run the container as the installer's `--service-user`, with that numeric UID and
  GID, so the host helpers and the daemon share one UID.

Minimal override, saved as `compose.override.yaml` beside `compose.yaml`. Compose merges
it automatically. The repository does not test this file:

```yaml
services:
  git-server:
    # Numeric UID:GID of the installer's --service-user (for example from `id gitserv`).
    user: "1001:1001"
    environment:
      SSH_MODE: openssh
    volumes:
      - /run/codeyard:/run/codeyard
      - /etc/codeyard/openssh.json:/etc/codeyard/openssh.json:ro
      - /etc/codeyard/ssh-serve:/etc/codeyard/ssh-serve:ro
```

Changing `user:` also changes access: the `/data` volume and the `CI_PODMAN_SOCKET`
mount must be usable by that UID. Use rootful Podman or Docker. The manifest check needs
the file to look root-owned inside the container. Rootless Podman run by an ordinary user
usually does not give that, because host root is unmapped there. Check the result with
`ls -ln` on the socket and manifest, and with `podman exec <container> id`.

Host OpenSSH serves only repositories stored on the node it runs on. For a cluster or
remote repositories, use built-in SSH (`SSH_MODE=builtin`) or smart HTTP, which reach
any serving node.

The tests include installer fixtures in `tests/scripts/test-install-openssh.py`,
real read-only sshd/visudo parser checks in `tests/scripts/test-openssh-config.py`, and
private broker/Git tests under `backend/app/src/repositories/git_sessions/broker_tests.rs`. The real
loopback sshd test uses generated host/client keys, the actual key helper's
restricted authorized line, a private PostgreSQL schema and a random loopback
port. It changes no host SSH configuration or accounts. Because the test account may
have `nologin`, a tiny test-only preload substitutes only that UID's shell inside
the disposable sshd; UID/GID and public-key authentication remain real. That C
fixture is compiled only for these tests and never linked or installed into the app.

Setting `GITSERV_TEST_ACT_BINARY` to the pinned, verified act fixture enables the
opt-in real SSH push/CI test, which is marked `#[ignore]` and runs only with `--ignored`. It executes only a conductor-owned workflow
as an unprivileged user, with isolated act HOME/cache and no container-engine
socket. A gate held until the SSH helper exits prevents premature workflow success;
the workflow then verifies the pushed SHA and binary, and its persisted result/logs
are checked. This is trusted host execution, not an untrusted-workflow sandbox.

These tests do not apply the privileged installer, exercise its sudo transition,
or certify production account/PAM policy, runner isolation, durability or deployment.
Source tests alone do not establish production readiness.
