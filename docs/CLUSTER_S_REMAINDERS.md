# Cluster S: account locks and remaining claim-scan work

## S1 locking contract

PostgreSQL account restrictions take transaction advisory locks keyed by schema and
account ID. Authority readers take shared locks; restrictions take exclusive locks.
Each complete known account set is sorted and deduplicated before acquisition, ahead
of namespace and resource rows. Administrative changes retain actor and target
locks together. Organization and repository policy rows retain resource authority
through the effect, with fresh authorization inside the write transaction.

The former shared/exclusive membership lock `37002` is removed. Name/setup exclusion
`37003` remains: user and organization names share a namespace, and initial setup
must have one winner. Cross-table email ownership exclusion `37004` also remains;
account scoping cannot enforce email uniqueness across users and secondary emails.
Configuration and unrelated storage/metadata locks are not removed by S1.

An issuer discovered only after resource rows cannot introduce a reverse-order
wait. Such paths retain issuer rows with `FOR SHARE NOWAIT` (and fail closed on
contention); CI bot discovery uses a nonblocking shared account lock. Trusted App
bot writes retain the App owner's availability even without a token issuer.

Added paired SQLite/PostgreSQL contracts cover unrelated account transitions,
same-account compare/exchange, reciprocal administrator bans, revoked administrator
authority, and App-owner revocation of bot resource writes. PostgreSQL barriers
also check independent progress, same-account queuing, and late issuer contention.

## S3: returned candidates are bounded; examined rows are not

`src/backend/db/ci/claims.rs` limits returned compatible candidates to 128 by
default, configurable from 1 to 1024. Runner compatibility and workflow concurrency
are filtered before that limit; each candidate still receives live authorization
and eligibility checks during its claim transaction.

Both engines can examine an arbitrarily large queued backlog to find that page.
Claims start at cursor zero, so an incompatible prefix can be revisited on every
poll. `SKIP LOCKED` does not provide an examined-row budget. Correlated label and
concurrency checks add work beyond the outer queue scan.

Analytical example, **not a measurement**: with 1,000,000 queued rows and only the
last compatible, a claim can examine 1,000,000 outer candidates while returning
one, despite a configured limit of 128 (7,812.5 times that limit). No runtime or
EXPLAIN measurements were collected: builds and tests were explicitly prohibited.

A hard scan bound needs more than moving LIMIT ahead of eligibility filters: that
would let an incompatible prefix starve compatible work indefinitely. Follow-up
needs indexed scheduling buckets or a fair, persistent bounded-scan cursor, with
paired-engine starvation/rotation tests and examined-row measurements. This remains
unfinished rather than claiming the returned-row limit solves S3.
