# Abuse prevention

This lists what the server does today to limit abuse, and what it does not do. Anything
not listed here is not implemented.

## Limits in place

- **Upload quotas.** Each user has a file count and a byte budget per kind of file, over
  rolling one-hour and 24-hour windows. The quota is charged when an upload is reserved,
  so an upload that is never confirmed, or is later deleted, still counts. A refused
  reservation returns 429 `rate_limited`. The limits are rows in `file_reason_limits`,
  read on every reservation. Seed values: avatars 2 MiB, issue and pull request
  attachments 10 MiB, release assets 100 MiB. No object may exceed 100 MiB.
- **Content checks.** The declared content type is checked against an allow-list for some
  kinds of file. For PNG, JPEG, GIF, WebP and PDF the bytes must also match the file
  signature. Only avatars may be public.
- **Concurrency.** At most 32 upload, confirm and delete I/O operations run at once, at
  most 64 local download streams. Uploads, confirmations and downloads are refused as busy
  when none is free, and the client can try again. A delete that finds none free still
  removes the file; its object is removed later by cleanup. At most 4 upload forms are held
  in memory at once, each until its commit lands, and a further upload is refused as busy
  before its body is read.
- **Request bodies.** Every POST, PUT, PATCH and DELETE request is read in full before its
  handler runs, except the streamed file upload `PUT /api/files/{id}/content` and Git over
  HTTP. A body above 4 MiB is refused with 413, and one that does not arrive within 30
  seconds with 408. A body that is not declared to be 64 KiB or smaller takes a slot while
  it is read, from a pool of 8 for signed-in requests or 8 for anonymous ones, and a
  request that finds none free gets 503. Ordinary forms are far below 64 KiB and take no
  slot, so a client that stalls on large bodies cannot block them.
- **Repository downloads.** Raw files and archives are built in memory, so each node serves
  at most 4 at once. A request that finds none free gets 503 `downloads_busy` on the JSON
  API. A transfer whose client stops reading for 30 seconds ends and frees its slot.
- **Anonymous sessions.** The cleanup that runs every 300 seconds on web nodes removes
  expired anonymous browser sessions, then the oldest by expiry beyond 100,000, across every
  node that uses the database. That is not a hard maximum: the count can exceed 100,000
  between runs, and a flood of anonymous requests evicts the oldest sessions, including a
  visitor's unused login or signup form, which the visitor must reload the page to replace.
  Rate limiting anonymous traffic belongs to the reverse proxy. Authenticated sessions are
  not capped, and no new session is refused for this reason.
- **Git sessions.** A process runs at most four clones, fetches and pushes per CPU core (at
  least 16, `GIT_SESSION_LIMIT`) at once, over HTTP, built-in SSH, host OpenSSH and between
  nodes; a request waits up to `GIT_SESSION_WAIT_SECONDS` (default 30) for a slot, then is
  refused as busy. A session with no traffic in either direction for five minutes, or
  running longer than an hour, is killed.
  There is no limit on the size of a push and no per-user disk quota.
- **Login attempts.** 30 failed attempts per five minutes for each client address, and 10 for
  each account. A right password gives its attempt back, so only failures count. Sign-up and
  setup attempts count whether or not they succeed. A refused attempt returns 429
  `rate_limited`. Failed Git-over-HTTP token logins use these same budgets; valid Git
  requests do not consume them. The client address is the connection's. Behind a reverse
  proxy, list the proxy in `HTTP_TRUSTED_PROXY_IPS` so the address it forwards is used;
  otherwise every visitor shares the proxy's budget. IPv6 clients are counted by /64.
- **SSH.** The built-in server accepts public keys only. A client address that fails
  five times within 60 seconds (an unknown key, or a password or keyboard-interactive
  attempt) is refused for 300 seconds, and a refused address is closed before the SSH
  handshake. The record is kept in memory, so a restart clears it.
- **Accounts.** A banned account loses its sessions and cannot reserve uploads. The ban
  action has a guard against removing the last administrator. On private nodes, new
  sign-ups wait for an administrator. When `ADMIN_SECRET` is set, `/setup` requires it.
- **Workflows.** Only site administrators and the accounts listed in
  `CI_TRUSTED_ACTOR_IDS` can start CI runs (see [CI API](CI_API.md)).
- **Expired uploads.** A pending upload that never becomes active expires after 900 seconds.
  A sweep runs every 300 seconds on nodes with the `web` role and removes it.
- **Accounting retention.** Expired rows are purged; see [retention periods](STORAGE_ARCHITECTURE.md#uploads).

## Not implemented

- Per-IP limits on HTTP requests, or on account creation.
- Disk quotas for repositories.
- Recording client IP addresses. Upload and download records have no address, so there
  is no per-IP or cross-account analysis.
- Download rate limits. Downloads are counted, not limited.
- Flagging or banning accounts from upload activity.
- Content scanning, malware checks and CAPTCHAs.

## Tuning

The limits are database rows, read again on each reservation, so a change takes effect
without a restart. Calibrate them under load for the deployment before relying on them.
