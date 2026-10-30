# Actions compatibility fixtures

`../compat.rs` starts the real `codeyard-actions` binary for every workflow. Each
fixture has `workflow.yml`, `expected.yml`, and optionally `workspace/` files copied
into a fresh Git checkout. The checkout, home and action cache are private to that
run. Assertions in the workflow check effects, not just whether `uses:` returned.

`expected.yml` records the process exit code, job names and conclusions, required
log fragments, and optional forbidden fragments, secrets, CLI environment values
and a wall-clock deadline. JSON syntax in these YAML sidecars keeps strings such
as `true` unambiguous. Skipped jobs are checked by an `always()` observer reading
`needs.<job>.result`, since the CLI does not log a completion for them. Other job
conclusions are checked against the CLI's final `Job completed:` lines. Names are
explicit for matrix runs.

## Feature workflows

The small examples exercise the behavior described in GitHub's documentation:

- [Expressions and functions](https://docs.github.com/en/actions/learn-github-actions/expressions):
  `expressions`, `functions`, `hash-files`, `status-functions`.
- [Contexts](https://docs.github.com/en/actions/learn-github-actions/contexts):
  `contexts`, `env`, `outputs`, `needs`, `continue-on-error`.
- [Workflow commands](https://docs.github.com/en/actions/using-workflows/workflow-commands-for-github-actions):
  `file-commands`, `workflow-commands`, `command-parsing`, `problem-matchers`, `masking`,
  `local-node`.
- [OIDC](https://docs.github.com/en/actions/security-for-github-actions/security-hardening-your-deployments/about-security-hardening-with-openid-connect):
  `oidc-permissions` checks per-job request credentials; `tests/ci/oidc.rs` checks the issuer and signatures on both databases.
- [Matrices](https://docs.github.com/en/actions/using-jobs/using-a-matrix-for-your-jobs):
  `matrix`, `dynamic-matrix`.
- [Workflow syntax](https://docs.github.com/en/actions/writing-workflows/workflow-syntax-for-github-actions):
  `defaults`, `shells`, `step-timeout`, `job-timeout`, `cancellation` (cleanup steps
  after a fail-fast cancel).
- [Reusable workflows](https://docs.github.com/en/actions/using-workflows/reusing-workflows):
  `reusable-workflows`, `reusable-validation` check typed inputs, secrets, outputs,
  matrices, four-level nesting and refused contracts.
- [Composite actions](https://docs.github.com/en/actions/creating-actions/creating-a-composite-action):
  `composite`, `nested-composite`.
- [JavaScript actions](https://docs.github.com/en/actions/creating-actions/creating-a-javascript-action):
  `local-node` checks pre/main/post, inputs, outputs and state.
- [Docker actions](https://docs.github.com/en/actions/creating-actions/creating-a-docker-container-action)
  and [service containers](https://docs.github.com/en/actions/using-containerized-services/about-service-containers):
  `local-docker` (also pre/post entrypoints and the `args` input), `job-container`,
  `services`.

Timeout workflows use a real one-minute timeout, rather than a fractional timeout
that GitHub's integer fields do not support. They run concurrently in the test
harness, so the host suite takes about one minute.

## Public actions

`actions/catalog.json` lists 100 public action fixtures. Every remote `uses:` is
pinned to a full commit SHA, with its original upstream ref in a comment. Pins were
resolved using `git ls-remote`; annotated tags use the peeled commit. Each workflow
links to that upstream revision for its documentation. `dtolnay/rust-toolchain`
publishes toolchain branches rather than version tags; its comment preserves the
original `1.85.0` branch name. Cache restore/save and other separately documented
subactions have their own fixtures.

A host-side manifest test parses **all** workflows and sidecars, checks that they
are registered, and checks the pins against the catalog. It does not claim that
ignored workflows ran.

## Running

The normal crate test command runs the host features and manifest validation. No
feature is ignored merely because it is unimplemented. External prerequisites
are grouped explicitly:

- `needs node`: `local_node` (Node 20 or a compatible later release).
- `needs podman`: `job_container`, `services`, `local_docker`, plus the Terragrunt,
  Docker login, build-push, bake, buildx, QEMU, and find-replace action fixtures.
  These also need network access to obtain images and action repositories.
- `needs network`: the other public action fixtures. Current setup-node/go/python
  pins require Node 24; the pinned test shell supplies `node24` so installing an
  older `node` for a workflow does not replace the action runtime. Setup-python
  seeds the tool cache with the test shell's real Python 3.12 on platforms its
  installer does not cover; elsewhere it uses `actions/python-versions`. Its pinned
  matcher is also checked independently in `problem_matchers`. Other tool-specific
  prerequisites below still apply.

On the test machine, run one named ignored test, for example:

```sh
cargo test --locked -p codeyard-actions --test compat action_setup_node -- --ignored --exact
cargo test --locked -p codeyard-actions --test compat services -- --ignored --exact
```

Use a **disposable Linux runner** for the public actions. They execute upstream
code and some installers need sudo, compilers, Python, curl, tar, unzip, OpenSSL,
or other tools supplied by GitHub-hosted Ubuntu runners. Nix, PHP, R, TinyTeX and
similar installers can change system configuration. An isolated checkout/home is
not a security sandbox. Run system-changing installers serially (`--test-threads=1`).
Docker ecosystem actions invoke `docker` themselves and require a compatible
Docker API/CLI on top of the Podman test environment; engine socket mounting is
not enabled by the harness. QEMU additionally needs privileged binfmt support.

Optional credentials are read only from `COMPAT_SECRET_<NAME>` environment
variables and passed through a private dotenv secret file:

- `GITHUB_TOKEN`: public checkout and API download rate limits.
- `DOCKERHUB_USERNAME`, `DOCKERHUB_TOKEN`: Docker Hub login.
- `DIGITALOCEAN_ACCESS_TOKEN`: doctl setup.
- `COMPAT_SSH_PRIVATE_KEY`: disposable private key for SSH setup/agent tests.

Cache and artifact tests also need working Actions cache/artifact services.
`ACTIONS_RUNTIME_TOKEN`, `ACTIONS_RUNTIME_URL` and `ACTIONS_RESULTS_URL`, if set,
are forwarded through the CLI environment. The current runner intentionally does
not implement those services; these tests measure that gap rather than pretending
that merely creating the upload source file proves an upload worked. Upload checks
`artifact-id`; download first uploads its own artifact and verifies its contents;
cache save checks its success log. Missing credentials, tools or services cause an
explicit failure when an ignored test is requested, never a successful early exit.
