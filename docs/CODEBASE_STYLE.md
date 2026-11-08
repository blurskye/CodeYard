# Codebase style

The layout and rendering model are in [Architecture](ARCHITECTURE.md), and the
[codebase map](CODEBASE_MAP.md) has one line per file. This file covers how the code is
written.

## Layout

- One application and one binary. Backend crates live under `backend/` (see
  [Crates](ARCHITECTURE.md#crates)), with the remaining native server code in
  `backend/app/src/`. `webui/` holds the Dioxus crate shared by server rendering and the
  browser.
- Database operations belong in the domain's `backend/db/<domain>/` crate, with matching
  `postgres/` and `sqlite/` owners; `backend/database/` keeps connections, migrations and
  transactions that span domains. Call qualified operations such as
  `codeyard_db_repos::repositories::records::get_repository_by_id`, not root wildcard
  exports. Schema and migration SQL files live separately in `schema/postgres/` and
  `schema/sqlite/`. Handlers and domain code call the database functions and do not build
  SQL strings. Tests may query the database directly.
- HTTP adapters live in the `backend/api/` crates, grouped by resource. Each resource
  composes its JSON, HTML and native-form routes; shared context, rendering, session,
  origin and middleware live in `backend/api/http/`. Do not call one handler from another.
- Reused composed operations live in feature modules and take HTTP-free inputs.
  Adapters may call named database transactions directly; do not wrap them in an
  empty service layer or move their live authorization into a preflight check.
- `webui/src` holds views and safe presentation models only. It does not reach the
  database, server state or secrets. Resource page adapters load and authorize data
  before SSR; hydration uses the same models and native forms, not a second UI.

## Names

- Use plain, specific names: `create_repo`, `RepoStore`, `list_open_pull_requests`.
- Avoid catch-all modules and types: `utils`, `common`, `helpers`, `manager`,
  `service`, `actions`, and `models` outside the database record types.
- Name modules after the feature they implement, not the layer they sit in.

## One implementation per concept

- A rule, query, validation or message exists in one place. Call it; do not copy it.
- When a path is replaced, delete the old path together with its callers. Do not keep
  a forwarding wrapper, a compatibility alias or an unused branch "for now".
- Do not add an abstraction with a single implementation. A trait needs a second
  implementation or a test seam. A struct needs state it owns.

## Functions and errors

- Take explicit inputs and return a typed result. Authorization is an explicit check
  made before data is loaded or changed.
- Return typed outcomes for missing, conflicting, denied and unavailable states. Do not
  turn an internal error into empty data or into success.
- Public error text is stable and short. Logs carry operation names and error codes,
  never secrets, credentials, raw auth headers or URLs with embedded passwords.

## Transactions and external effects

- A change that must commit as one unit runs in one database transaction. Git, S3 and
  network calls stay out of database transactions.
- Say which step runs first, and what happens when a later step fails. Use typed
  partial outcomes when Git, an object or a database row already changed; do not
  report rollback or atomicity across Git, SQL and storage.
- Runtime owners retain task handles and stop/join them on shutdown. `CiRuns` owns
  run admission/control; `CiWorkers` owns worker tasks. Constructing a notification
  broadcaster does not start its listener: runtime calls `start` and `shutdown`.

## Comments

- Comment invariants, protocol constraints and the reason for a choice. Do not narrate
  what the code does.

## Tests

- Test each rule where it is owned, and test behavior at the HTTP, SSH or CLI boundary.
- A test that needs a database or service that is missing must fail. It must not pass.

## Attribution

- No AI attribution in code, comments, commit messages or pull requests.
