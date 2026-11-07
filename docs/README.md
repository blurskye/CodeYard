# Codeyard documentation

Start with the [README](../README.md) for setup.

## Reference

- [Architecture](ARCHITECTURE.md): the high-level code map, request flows, data layout and how clustering works.
- [HTTP API](API.md): the JSON endpoints, account rules, errors and limits.
- [Gists](GISTS.md): native snippets, Git transport, secret links, limits and recovery.
- [Running a cluster](CLUSTER.md): roles, pairing nodes, storage nodes, CI workers and limits.
- [Kubernetes](KUBERNETES.md): Helm installation, CloudNativePG, role scaling, the cluster failover test and current limits.
- [Cluster readiness plan](CLUSTER_PLAN.md): checks on the cluster that exists, what is still missing, and the decision for each open question.
- [Crate split plan](CRATE_SPLIT_PLAN.md): how the one big crate becomes crates under `backend/`, with the order and the circles to cut.
- [CI API](CI_API.md): workflow runs, who may start them, statuses and known gaps.
- [GitHub API plan](GITHUB_API_PLAN.md): making `/api/v3` and GraphQL match GitHub, what stays unsupported, and the worker split.
- [Pages and repository security](PAGES_AND_SECURITY.md): Pages control-plane limits, pinned dependency/SBOM reports and confidential advisory workflows.
- [CI local runner](CI_LOCAL_RUNNER.md): running jobs with `act` through Podman, and the trust limits.
- [Host OpenSSH](OPENSSH.md): the `SSH_MODE` settings and the installer for host OpenSSH.
- [Storage](STORAGE_ARCHITECTURE.md): where data lives, uploads, backups and moving data.
- [Database-owned authentication](POSTGRES_AUTH.md): sessions, CSRF, OAuth state and login admission.
- [Abuse prevention](ABUSE_PREVENTION.md): the limits in place and the controls that are not implemented.
- [Environment settings](../.env.example): every setting the server reads, with its default.

## Assets

- [Branding](../branding/README.md): the logo, favicon and banner, and their sources.

## Verification

- `cargo fmt --all -- --check`, `cargo clippy --locked --workspace --all-targets -- -D warnings` and
  `cargo test --locked --workspace --all-targets` are the format, lint and test checks. PostgreSQL tests
  read `GITSERV_TEST_DATABASE_URL`.
- `scripts/e2e.sh` starts real servers on SQLite, PostgreSQL and a four-node cluster
  and drives them over HTTP, SSH and Git, including remote storage and database-free CI.
  Its SSR and served JS/WASM checks do not execute hydrated browser navigation.
- The image requires patched `act` 0.2.89-codeyard.1: the pinned v0.2.89 source with
  [job-scoped environment support](../patches/act-environment-values.patch), built as in
  [the CI workflow](../.github/workflows/ci.yml).
  `scripts/e2e.sh` runs the workflow runner named by `E2E_ACT_BINARY`, which must be that same build.
- `scripts/s3-tests.sh` runs the `#[ignore]`d S3 tests of `tests/storage/production_storage.rs`
  against a throwaway RustFS. It needs `GITSERV_TEST_DATABASE_URL` and `RUSTFS_BIN`.
- `cargo audit` checks `Cargo.lock`; the advisories we accept are listed with reasons in
  `.cargo/audit.toml`.
- `scripts/release.sh` builds the release binary and browser bundle on this machine, then the server image from them.
- `scripts/test-actions-ui.py` checks pre-seeded fixture runs without mutating CI;
  `scripts/test-actions-live.py` dispatches a real workflow against an explicitly prepared
  trusted runner and test repository. Neither is part of CI or `scripts/e2e.sh`.
- `scripts/test-gists-browser.py --headed` uses an owned disposable installation to check
  no-JavaScript forms, hydrated gist navigation and 375px/1280px layouts.
- Hydration, navigation, drafts and visual interaction require Playwright in real Chromium;
  final visual sign-off requires a headed-browser run. Served JS/WASM alone is not enough.
- Documentation-only changes need link checks, not builds.
