# Database-owned authentication

There is no cache server. The application database (SQLite or PostgreSQL, selected
by `DATABASE_URL`) owns authenticated and anonymous sessions, session-bound CSRF
tokens, OAuth state and shared login admission. There is no second session store.
Bare Git repositories stay on disk. Uploads go to the local `objects/` directory or
to an S3-compatible bucket (see [Storage](STORAGE_ARCHITECTURE.md)).

This describes the current code. It is not a production certification: database
tests, browser checks and release validation are separate steps. Credential and
account operations are in `backend::identity`. The JSON and page adapters translate
their typed outcomes. Password hashing, shared admission and session rotation are
not exported through `backend::api`.

## Configuration and upgrade

- The schema is a baseline (`schema/<engine>/0001.schema.sql`) followed by any later
  migrations in the same directories. `db::run_migrations` applies the pending ones, in
  order, on every start. Session CSRF fields, OAuth browser binding and the
  login-admission table are part of the baseline.
- The migration runner records the applied history. If it reports a history or
  checksum mismatch, the database was created by a different build. This build does
  not convert it, so restore from a backup or start from a fresh database. Do not
  edit the migration ledger by hand.
- An old page's cached CSRF token must be refreshed by loading the page again.
  Unbound OAuth attempts that were in flight must start again. A rejected mutation is
  never retried silently.
- Set `PUBLIC_URL` to the exact external HTTPS origin when OAuth is enabled. Explicit
  loopback HTTP is allowed for isolated development. Host and forwarded headers do not
  select the callback origin. An `https://` `PUBLIC_URL` makes session cookies Secure.
- Private nodes accept existing approved provider links, not automatic email-based
  enrollment. There is no administrator interface for approving a new OAuth subject
  yet, so new subjects cannot be enrolled. Do not treat this as working enrollment.
- Removing a Compose service or volume is never automatic. Do not delete an existing
  volume as part of an upgrade.

## Security behavior

Successful authentication creates a fresh random session ID and transactionally
invalidates the initiating anonymous session and its CSRF token. A live
already-authenticated ID cannot be rebound to another account. Password reset,
password change and suspension revoke authenticated sessions; suspension preserves
repositories and other stored data.

New passwords use Argon2id (19456 KiB memory, 2 iterations, 1 lane). Successful legacy
SHA256 verification upgrades the stored hash conditionally, so it cannot overwrite a
concurrent reset. Four native KDF permits bound in-process work. The selected database
enforces shared per-account and per-client-address limits across independent handles
and PostgreSQL nodes. Current defaults are 10 failed attempts per five minutes per
account and 30 per five minutes per client address; a right password gives its attempt
back. A deployment still needs load calibration and ingress protection. `X-Forwarded-For`
is read only from the proxies listed in `HTTP_TRUSTED_PROXY_IPS`.

Database failures deny authentication and authority rather than treating the request as
anonymous. CSRF tokens are stored on the live session, reused across concurrent page
loads, and expire no later than that session or two hours.

## Operations still requiring proof

Expired sessions, anonymous sessions and OAuth states are deleted, and processed signup
requests lose their password verifier, by a sweep every five minutes on web nodes
(`remove_expired_identity_rows`, batches of 500). Establish reviewed retention, capacity and backup
and restore procedures. Keep the secret-encryption key
(`SECRETS_MASTER_KEY`) in protected backups: a database restore alone cannot replace
it. Existing OAuth bearer tokens are not purged; new sign-ins stop storing them.

Database barriers and controlled provider tests are not full browser acceptance,
real-provider certification, process-restart proof or multi-store disaster recovery.
The wider production-readiness gate remains separate.
