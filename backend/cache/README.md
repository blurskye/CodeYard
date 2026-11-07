# Disposable content cache

`codeyard-cache` is native-only. Authorization runs before a cache lookup. The only
integration is the GitHub Markdown rendering endpoints; their key includes the text,
renderer format and fully resolved references. A private repository or reference is
checked again on every request. Credentials, permissions, sessions, limits, locks and
queues never use this cache.

The bounded local LRU is always enabled (2,048 entries and 32 MiB by default). Values
are limited to 1 MiB, batches to 256 keys and TTLs to one hour. Markdown uses five
minutes. Keys have the form `cy:v1:<deployment>:<cell>:<family>:<sha256>`; all input
parts are length-framed and there are no hash tags. Different cells do not share hits.

- `CACHE_MODE=local` is the default. No connection is made.
- `CACHE_MODE=standalone` takes one URL in `CACHE_URLS`.
- `CACHE_MODE=cluster` takes up to 16 comma-separated startup URLs in `CACHE_URLS`.
- `CACHE_DEPLOYMENT` is a distinct installation name (letters, digits, `_` and `-`).
- `CACHE_LOCAL_ENTRIES` and `CACHE_LOCAL_BYTES` set the local bounds.

Use `rediss://user:password@host:port/` with trusted certificates and restricted ACLs
outside a private development network. Database zero is required. Do not expose the
cache publicly; its values are trusted rendered content. URL secrets are never logged.
Invalid configuration warns and falls back to the local tier. Valkey 8.1.3 is the
pinned development server; Compose's `cache` profile is off by default and stores no
persistent data. Run it with `--profile cache` and explicitly set `CACHE_MODE` and
`CACHE_URLS=redis://valkey:6379/` to use it. Configure `allkeys-lru` on remote servers.

Application data commands are only GET, MGET, SET with PX, and DEL. The Redis client
also performs required connection authentication and Cluster topology/redirect
operations (including PING, CLUSTER SLOTS and ASKING); these are not application data
operations. CLIENT SETINFO is disabled. No Lua, transactions, subscriptions or rate
counters are used. MGET and DEL split by slot; successful slots retain input order
when another slot fails. Calls have a 50 ms remote budget and a one-second circuit
breaker after three failures. Cache errors are misses. Striped single-flight locks
and a 16-operation recompute limit per family are bounded and always enabled.

Run unit tests with `cargo test -p codeyard-cache --lib`. The contracts in
`tests/valkey.rs` use `CODEYARD_TEST_VALKEY_URL` and
`CODEYARD_TEST_VALKEY_CLUSTER_URLS` for standalone and Cluster respectively. Each
missing variable prints an explicit SKIP, which is not a pass.
`scripts/with-valkey.sh cargo test -p codeyard-cache --test valkey` starts a disposable
standalone server and a three-shard Cluster from the test shell and sets both. Their
ACL user may only run GET, MGET, SET and DEL on `cy:*` keys plus the client's
connection and topology commands, so any other command fails the contract. TLS is not
covered by these servers. The full application e2e remains the cache-absent run with
`CACHE_MODE=local` and `CACHE_URLS` unset.
