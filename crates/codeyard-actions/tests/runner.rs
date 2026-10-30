//! Real CLI tests following actions/runner's `Runner.Worker` job and handler behaviour.
use std::path::Path;
use std::process::{Command, Output};

/// The binary under test. The variable lets a test binary built elsewhere run here.
fn binary() -> std::ffi::OsString {
    std::env::var_os("CODEYARD_ACTIONS_BIN")
        .unwrap_or(env!("CARGO_BIN_EXE_codeyard-actions").into())
}

fn command(workspace: &Path) -> Command {
    let mut command = Command::new(binary());
    command
        .arg("push")
        .arg("-W")
        .arg(workspace.join("workflow.yml"))
        .arg("--directory")
        .arg(workspace)
        .args([
            "-P",
            "self-hosted=-self-hosted",
            "--container-daemon-socket",
            "-",
            "--no-cache-server",
            "--pull=false",
        ]);
    command.env("XDG_CACHE_HOME", workspace.join("cache"));
    command
}

fn run(yaml: &str, files: &[(&str, &str)], args: &[&str]) -> Output {
    let workspace = tempfile::tempdir().unwrap();
    std::fs::write(workspace.path().join("workflow.yml"), yaml).unwrap();
    for (path, contents) in files {
        let path = workspace.path().join(path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, contents).unwrap();
    }
    command(workspace.path()).args(args).output().unwrap()
}

fn log(output: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

fn success(output: &Output) {
    assert!(output.status.success(), "{}", log(output));
}

#[test]
fn job_tokens_are_private_masked_and_selected_for_each_job() {
    let workspace = tempfile::tempdir().unwrap();
    std::fs::write(workspace.path().join("tokens.json"), r#"{"tokens":{"first":"first-secret","second":"second-secret","skipped":"skipped-secret"},"completion_secret":"completion-nonce","oidc_url":"https://codeyard.example/api/ci/oidc/token?","oidc_jobs":["first"]}"#).unwrap();
    std::fs::write(
        workspace.path().join("workflow.yml"),
        r#"
name: tokens
on: push
jobs:
  first:
    runs-on: self-hosted
    outputs:
      safe: ${{ steps.values.outputs.safe }}
      credential: ${{ steps.values.outputs.credential }}
    steps:
      - id: values
        env:
          CONTEXT: ${{ github.token }}
          SECRET: ${{ secrets.GITHUB_TOKEN }}
        run: |
          test "$CONTEXT" = first-secret
          test "$SECRET" = first-secret
          test "$GITHUB_TOKEN" = first-secret
          test "$ACTIONS_ID_TOKEN_REQUEST_TOKEN" = first-secret
          test "$ACTIONS_ID_TOKEN_REQUEST_URL" = 'https://codeyard.example/api/ci/oidc/token?'
          echo "$CONTEXT"
          printf 'safe=recorded\ncredential=%s\n' "$GITHUB_TOKEN" >> "$GITHUB_OUTPUT"
          printf '%s\n' '::codeyard-progress::completion-nonce:{"job":"intruder","variant":0,"at_ms":1,"update":{"kind":"started"}}'
  second:
    runs-on: self-hosted
    steps:
      - env:
          CONTEXT: ${{ github.token }}
          SECRET: ${{ secrets.GITHUB_TOKEN }}
        run: |
          test "$CONTEXT" = second-secret
          test "$SECRET" = second-secret
          test "$GITHUB_TOKEN" = second-secret
          test -z "${ACTIONS_ID_TOKEN_REQUEST_TOKEN+x}"
          test -z "${ACTIONS_ID_TOKEN_REQUEST_URL+x}"
          echo "$CONTEXT"
  skipped:
    if: false
    runs-on: self-hosted
    steps: [{run: exit 1}]
"#,
    )
    .unwrap();
    let output = command(workspace.path())
        .arg("--job-token-file")
        .arg(workspace.path().join("tokens.json"))
        .arg("--job-state-file")
        .arg(workspace.path().join("state.json"))
        .output()
        .unwrap();
    success(&output);
    let text = log(&output);
    for secret in ["first-secret", "second-secret", "skipped-secret"] {
        assert!(!text.contains(secret), "{text}");
    }
    let state: codeyard_actions::progress::State =
        serde_json::from_slice(&std::fs::read(workspace.path().join("state.json")).unwrap())
            .unwrap();
    assert_eq!(
        state.jobs["first"].outputs.get("safe").map(String::as_str),
        Some("recorded")
    );
    assert!(!state.jobs["first"].outputs.contains_key("credential"));
    assert_eq!(state.jobs["skipped"].result, "skipped");
    let events: Vec<codeyard_actions::progress::Event> = text
        .lines()
        .filter_map(|line| line.strip_prefix("::codeyard-progress::completion-nonce:"))
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert!(events
        .iter()
        .all(|event| ["first", "second", "skipped"].contains(&event.job.as_str())));
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(
                event.update,
                codeyard_actions::progress::Update::Queued { .. }
            ))
            .count(),
        3
    );
    assert!(events.iter().any(|event|matches!(&event.update,codeyard_actions::progress::Update::Log{text,..} if text.contains("::codeyard-progress::***:"))));
    for job in ["first", "second", "skipped"] {
        assert_eq!(
            text.matches(&format!("::codeyard-job-completed::completion-nonce:{job}"))
                .count(),
            1,
            "{text}"
        );
    }
}

#[test]
fn replay_uses_real_dependency_outputs_without_reexecuting_successful_jobs() {
    let root = tempfile::tempdir().unwrap();
    let dir = root.path();
    std::fs::write(
        dir.join("workflow.yml"),
        r#"
on: push
jobs:
  producer:
    runs-on: self-hosted
    outputs:
      value: ${{ steps.values.outputs.value }}
    steps:
      - id: values
        run: |
          test ! -f producer-once
          touch producer-once
          echo value=observed >> "$GITHUB_OUTPUT"
  retry:
    runs-on: self-hosted
    needs: producer
    steps:
      - run: |
          test '${{ needs.producer.outputs.value }}' = observed
          if [ ! -f retry-once ]; then touch retry-once; exit 1; fi
  dependent:
    runs-on: self-hosted
    needs: retry
    steps:
      - run: touch dependent-ran
"#,
    )
    .unwrap();
    let state = dir.join("state.json");
    let first = command(dir)
        .arg("--job-state-file")
        .arg(&state)
        .output()
        .unwrap();
    assert!(!first.status.success(), "{}", log(&first));
    let captured: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&state).unwrap()).unwrap();
    assert_eq!(captured["jobs"]["producer"]["outputs"]["value"], "observed");
    assert!(!dir.join("dependent-ran").exists());
    let replay = dir.join("replay.json");
    std::fs::write(&replay,serde_json::to_vec(&serde_json::json!({"version":1,"state":captured,"selected":{"retry":null,"dependent":null}})).unwrap()).unwrap();
    let credentials = dir.join("tokens.json");
    std::fs::write(&credentials,r#"{"tokens":{"retry":"fresh-retry-token","dependent":"fresh-dependent-token"},"completion_secret":"replay-nonce"}"#).unwrap();
    let next = dir.join("next-state.json");
    let output = command(dir)
        .arg("--replay-file")
        .arg(&replay)
        .arg("--job-token-file")
        .arg(&credentials)
        .arg("--job-state-file")
        .arg(&next)
        .output()
        .unwrap();
    success(&output);
    assert!(dir.join("dependent-ran").exists());
    let fresh: serde_json::Value = serde_json::from_slice(&std::fs::read(next).unwrap()).unwrap();
    assert!(fresh["jobs"].get("producer").is_none());
    assert_eq!(fresh["jobs"]["retry"]["result"], "success");
    for line in String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| line.strip_prefix("::codeyard-progress::replay-nonce:"))
    {
        let event: serde_json::Value = serde_json::from_str(line).unwrap();
        assert_ne!(event["job"], "producer");
    }
}

#[test]
fn replay_keeps_original_matrix_indices_and_inherits_only_real_variants() {
    let root = tempfile::tempdir().unwrap();
    let dir = root.path();
    std::fs::write(
        dir.join("workflow.yml"),
        r#"
on: push
jobs:
  build:
    runs-on: self-hosted
    strategy:
      max-parallel: 1
      matrix:
        index: [0, 1]
    outputs:
      value: ${{ steps.values.outputs.value }}
    steps:
      - id: values
        run: |
          if [ '${{ matrix.index }}' = 0 ]; then
            test ! -f zero-once
            touch zero-once
            echo value=zero >> "$GITHUB_OUTPUT"
          else
            if [ ! -f retry-once ]; then touch retry-once; exit 1; fi
            test '${{ strategy.job-index }}' = 1
            echo value=one >> "$GITHUB_OUTPUT"
          fi
  dependent:
    runs-on: self-hosted
    needs: build
    steps:
      - run: |
          test '${{ needs.build.outputs.value }}' = one
          touch dependent-ran
"#,
    )
    .unwrap();
    let state = dir.join("state.json");
    let first = command(dir)
        .arg("--job-state-file")
        .arg(&state)
        .output()
        .unwrap();
    assert!(!first.status.success(), "{}", log(&first));
    let captured: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&state).unwrap()).unwrap();
    assert_eq!(captured["variants"]["build"]["0"]["result"], "success");
    assert_eq!(captured["variants"]["build"]["1"]["result"], "failure");
    let replay = dir.join("replay.json");
    std::fs::write(&replay,serde_json::to_vec(&serde_json::json!({"version":1,"state":captured,"selected":{"build":[1],"dependent":null},"matrices":{"build":{"1":{"index":1}}}})).unwrap()).unwrap();
    let next = dir.join("next-state.json");
    let output = command(dir)
        .arg("--replay-file")
        .arg(&replay)
        .arg("--job-state-file")
        .arg(&next)
        .output()
        .unwrap();
    success(&output);
    assert!(dir.join("dependent-ran").exists());
    let fresh: serde_json::Value = serde_json::from_slice(&std::fs::read(next).unwrap()).unwrap();
    assert!(fresh["variants"]["build"].get("0").is_none());
    assert_eq!(fresh["variants"]["build"]["1"]["result"], "success");
    let mut changed: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&replay).unwrap()).unwrap();
    changed["matrices"]["build"]["1"]["index"] = serde_json::json!(0);
    std::fs::write(&replay, serde_json::to_vec(&changed).unwrap()).unwrap();
    let refused = command(dir)
        .arg("--replay-file")
        .arg(&replay)
        .output()
        .unwrap();
    assert!(!refused.status.success());
    assert!(
        log(&refused).contains("Selected matrix context has changed"),
        "{}",
        log(&refused)
    );
    assert!(!log(&refused).contains("Job started"), "{}", log(&refused));
}

#[test]
fn unnamed_workflows_use_the_real_filename() {
    let output=run("on: push\njobs:\n  test:\n    runs-on: self-hosted\n    steps:\n      - run: test '${{ github.workflow }}' = workflow.yml\n",&[],&[]);
    success(&output);
}

#[test]
fn outputs_environment_paths_and_needs() {
    let output = run(
        r#"
name: host
on: push
env:
  LAYER: workflow
jobs:
  produce:
    runs-on: self-hosted
    env:
      LAYER: job
    outputs:
      result: ${{ steps.values.outputs.answer }}
    steps:
      - id: values
        env:
          LAYER: step
        run: |
          test "$LAYER" = step
          printf 'answer<<END\none\ntwo\nEND\n' >> "$GITHUB_OUTPUT"
          printf 'PERSISTED<<END\nhello\nworld\nEND\n' >> "$GITHUB_ENV"
          mkdir -p "$GITHUB_WORKSPACE/bin"
          printf '#!/bin/sh\necho from-path\n' > "$GITHUB_WORKSPACE/bin/example"
          chmod +x "$GITHUB_WORKSPACE/bin/example"
          echo "$GITHUB_WORKSPACE/bin" >> "$GITHUB_PATH"
      - run: |
          test "$LAYER" = job
          test "$PERSISTED" = 'hello
          world'
          example
  consume:
    needs: produce
    runs-on: self-hosted
    steps:
      - env:
          ANSWER: ${{ needs.produce.outputs.result }}
          RESULT: ${{ needs.produce.result }}
        run: |
          test "$ANSWER" = 'one
          two'
          test "$RESULT" = success
          echo needs-ok
"#,
        &[],
        &[],
    );
    success(&output);
    let text = log(&output);
    assert!(text.contains("[host/produce] from-path"), "{text}");
    assert!(text.contains("[host/consume] needs-ok"), "{text}");
}

#[test]
fn failure_outcome_continue_on_error_and_conditions() {
    let output = run(
        r#"
on: push
jobs:
  test:
    runs-on: self-hosted
    steps:
      - id: tolerated
        continue-on-error: true
        run: exit 7
      - if: steps.tolerated.outcome == 'failure' && steps.tolerated.conclusion == 'success'
        run: echo tolerated-ok
      - id: failed
        run: exit 1
      - run: echo SHOULD-NOT-RUN
      - if: failure()
        run: echo failure-handler
      - if: always()
        run: echo always-handler
  skipped:
    needs: test
    runs-on: self-hosted
    steps:
      - run: echo SHOULD-NOT-RUN-EITHER
  inspect:
    needs: test
    if: always()
    runs-on: self-hosted
    steps:
      - run: test '${{ needs.test.result }}' = failure
"#,
        &[],
        &[],
    );
    assert!(!output.status.success());
    let text = log(&output);
    assert!(text.contains("tolerated-ok"), "{text}");
    assert!(text.contains("failure-handler"), "{text}");
    assert!(text.contains("always-handler"), "{text}");
    assert!(!text.contains("SHOULD-NOT-RUN"), "{text}");
}

#[test]
fn skipped_dependencies_do_not_count_as_failures_or_cancellation() {
    let output = run(
        r#"
on: push
jobs:
  skip:
    if: false
    runs-on: self-hosted
    steps:
      - run: echo SHOULD-NOT-RUN
  default:
    needs: skip
    runs-on: self-hosted
    steps:
      - run: echo SHOULD-NOT-RUN
  failure:
    needs: skip
    if: failure()
    runs-on: self-hosted
    steps:
      - run: echo SHOULD-NOT-RUN
  cancelled:
    needs: skip
    if: cancelled()
    runs-on: self-hosted
    steps:
      - run: echo SHOULD-NOT-RUN
  success:
    needs: skip
    if: success()
    runs-on: self-hosted
    steps:
      - run: echo SHOULD-NOT-RUN
  inspect:
    needs: [skip, default, failure, cancelled, success]
    if: ${{ !success() && !failure() && !cancelled() }}
    runs-on: self-hosted
    steps:
      - run: |
          test '${{ needs.skip.result }}' = skipped
          echo skipped-status-ok
"#,
        &[],
        &[],
    );
    success(&output);
    assert!(
        log(&output).contains("skipped-status-ok"),
        "{}",
        log(&output)
    );
    assert!(!log(&output).contains("SHOULD-NOT-RUN"));
}

#[test]
fn dependency_ids_ignore_case_and_failure_follows_ancestors() {
    let output = run(
        r#"
on: push
jobs:
  fail:
    runs-on: self-hosted
    steps:
      - run: exit 1
  skipped:
    needs: FAIL
    runs-on: self-hosted
    steps:
      - run: echo SHOULD-NOT-RUN
  handler:
    needs: SKIPPED
    if: failure()
    runs-on: self-hosted
    steps:
      - run: |
          test '${{ needs.skipped.result }}' = skipped
          echo ancestor-failure-ok
"#,
        &[],
        &[],
    );
    assert!(!output.status.success());
    assert!(
        log(&output).contains("ancestor-failure-ok"),
        "{}",
        log(&output)
    );
    assert!(!log(&output).contains("SHOULD-NOT-RUN"));
}

#[test]
fn job_continue_on_error_and_matrix() {
    let output = run(
        r#"
on: push
jobs:
  matrix:
    runs-on: self-hosted
    continue-on-error: ${{ matrix.experimental }}
    strategy:
      fail-fast: false
      max-parallel: 2
      matrix:
        experimental: [false, true]
    steps:
      - run: |
          echo 'matrix-${{ matrix.experimental }}'
          if [ '${{ matrix.experimental }}' = true ]; then exit 1; fi
  next:
    needs: matrix
    runs-on: self-hosted
    steps:
      - run: test '${{ needs.matrix.result }}' = success
"#,
        &[],
        &["--concurrent-jobs", "2"],
    );
    success(&output);
    let text = log(&output);
    assert!(text.contains("matrix-false"), "{text}");
    assert!(text.contains("matrix-true"), "{text}");
}

#[test]
fn repository_variables_are_plain_context_values_not_environment_or_secrets() {
    let workspace = tempfile::tempdir().unwrap();
    std::fs::write(
        workspace.path().join("variables"),
        "REGION='public-region'\nMULTILINE=\"first\\nsecond\"\n",
    )
    .unwrap();
    std::fs::write(
        workspace.path().join("workflow.yml"),
        r#"
on: push
jobs:
  test:
    runs-on: self-hosted
    if: vars.REGION == 'public-region'
    steps:
      - env:
          REGION_VALUE: ${{ vars.REGION }}
          MULTILINE_VALUE: ${{ vars.MULTILINE }}
          SECRET_VALUE: ${{ secrets.REGION }}
        run: |
          test "$REGION_VALUE" = public-region
          test "$MULTILINE_VALUE" = "$(printf 'first\nsecond')"
          test -z "$SECRET_VALUE"
          test -z "${REGION-}"
          echo "$REGION_VALUE"
"#,
    )
    .unwrap();
    let output = command(workspace.path())
        .arg("--var-file")
        .arg(workspace.path().join("variables"))
        .output()
        .unwrap();
    success(&output);
    assert!(log(&output).contains("public-region"));
}

#[test]
fn secret_masking_commands_and_stopped_commands() {
    let workspace = tempfile::tempdir().unwrap();
    std::fs::write(
        workspace.path().join("secrets"),
        "TOKEN='top-secret-value'\n",
    )
    .unwrap();
    std::fs::write(
        workspace.path().join("workflow.yml"),
        r#"
name: masking
on: push
jobs:
  test:
    runs-on: self-hosted
    outputs:
      hidden: ${{ steps.test.outputs.hidden }}
    steps:
      - id: test
        env:
          TOKEN: ${{ secrets.TOKEN }}
        run: |
          echo "$TOKEN"
          echo "$TOKEN" >&2
          echo '::add-mask::runtime-secret'
          echo runtime-secret
          echo "hidden=$TOKEN" >> "$GITHUB_OUTPUT"
          echo '::stop-commands::resume-token'
          echo '::add-mask::not-a-secret'
          echo '::resume-token::'
          echo not-a-secret
          echo '::group::my-group'
          echo '::notice::some-notice'
          echo '::endgroup::'
"#,
    )
    .unwrap();
    let output = command(workspace.path())
        .arg("--secret-file")
        .arg(workspace.path().join("secrets"))
        .output()
        .unwrap();
    success(&output);
    let text = log(&output);
    assert!(!text.contains("top-secret-value"), "{text}");
    assert!(!text.contains("runtime-secret"), "{text}");
    assert!(text.contains("***"), "{text}");
    assert!(text.contains("not-a-secret"), "{text}");
    assert!(text.contains("some-notice"), "{text}");
    assert!(text.contains("Skip output 'hidden'"), "{text}");
}

#[test]
fn composites_have_private_steps_and_mapped_outputs() {
    let action = r#"
inputs:
  greeting:
    default: default-greeting
outputs:
  message:
    value: ${{ steps.inner.outputs.message }}
runs:
  using: composite
  steps:
    - id: inner
      shell: bash
      env:
        GREETING: ${{ inputs.greeting }}
      run: |
        test -f "$GITHUB_ACTION_PATH/action.yml"
        echo "message=$GREETING" >> "$GITHUB_OUTPUT"
        echo 'COMPOSITE_ENV=survives' >> "$GITHUB_ENV"
"#;
    let output = run(
        r#"
on: push
jobs:
  test:
    runs-on: self-hosted
    steps:
      - id: before
        run: echo 'value=outer' >> "$GITHUB_OUTPUT"
      - id: local
        uses: ./action
        with:
          greeting: hello-action
      - env:
          MESSAGE: ${{ steps.local.outputs.message }}
        run: |
          test "$MESSAGE" = hello-action
          test "$COMPOSITE_ENV" = survives
          test '${{ steps.before.outputs.value }}' = outer
          test '${{ steps.inner.outputs.message }}' = ''
          echo composite-ok
"#,
        &[("action/action.yml", action)],
        &[],
    );
    success(&output);
    assert!(log(&output).contains("composite-ok"));
}

#[test]
fn defaults_custom_shell_working_directory_and_env_precedence() {
    let output = run(
        r#"
on: push
defaults:
  run:
    working-directory: sub
jobs:
  test:
    runs-on: self-hosted
    steps:
      - run: echo 'VALUE=file' >> "$GITHUB_ENV"
      - env:
          VALUE: step
        shell: bash --noprofile --norc -e {0}
        run: |
          test "$VALUE" = step
          test "$PWD" = "$GITHUB_WORKSPACE/sub"
          test '${{ env.VALUE }}' = step
      - run: test "$VALUE" = file
"#,
        &[("sub/placeholder", "")],
        &[],
    );
    success(&output);
}

#[test]
fn disabled_commands_fail_but_set_output_is_deprecated() {
    let output = run(
        r#"
on: push
jobs:
  test:
    runs-on: self-hosted
    steps:
      - id: old
        run: echo '::set-output name=value::legacy'
      - run: test '${{ steps.old.outputs.value }}' = legacy
      - run: echo '::set-env name=EVIL::value'
"#,
        &[],
        &[],
    );
    assert!(!output.status.success());
    assert!(log(&output).contains("command is disabled"));
}

#[test]
fn unmatched_event_is_a_successful_noop() {
    let output = run("on: pull_request\njobs:\n  test:\n    runs-on: self-hosted\n    steps:\n      - run: exit 1\n", &[], &[]);
    success(&output);
    assert!(!log(&output).contains("Job started"));
}

#[test]
fn step_timeout_stops_descendants() {
    let started = std::time::Instant::now();
    let output = run(
        r#"
on: push
jobs:
  test:
    runs-on: self-hosted
    steps:
      - timeout-minutes: 0.01
        run: sleep 30 & wait
      - if: always()
        run: echo cleanup-after-timeout
"#,
        &[],
        &[],
    );
    assert!(!output.status.success());
    assert!(started.elapsed() < std::time::Duration::from_secs(10));
    assert!(log(&output).contains("cleanup-after-timeout"));
}

#[test]
fn github_runner_and_event_contexts_match_the_environment() {
    let workspace = tempfile::tempdir().unwrap();
    std::fs::write(
        workspace.path().join("event.json"),
        r#"{"inputs":{"message":"dispatch-value"},"repository":{"full_name":"owner/project"}}"#,
    )
    .unwrap();
    std::fs::write(
        workspace.path().join("workflow.yml"),
        r#"
name: context-test
on: push
jobs:
  test:
    runs-on: self-hosted
    steps:
      - env:
          INPUT_VALUE: ${{ inputs.message }}
        run: |
          test "$GITHUB_ACTOR" = actor-name
          test "$GITHUB_EVENT_NAME" = push
          test "$GITHUB_REPOSITORY" = owner/project
          test "$GITHUB_REF" = refs/heads/topic
          test "$GITHUB_REF_NAME" = topic
          test "$GITHUB_SHA" = given-sha
          test "$GITHUB_RUN_ID" = '123'
          test "$GITHUB_JOB" = test
          test "$GITHUB_WORKFLOW" = context-test
          test "$INPUT_VALUE" = dispatch-value
          test "$RUNNER_OS" = '${{ runner.os }}'
          test "$RUNNER_TEMP" = '${{ runner.temp }}'
          test -f "$GITHUB_EVENT_PATH"
          test -d "$RUNNER_TEMP"
          test -d "$RUNNER_TOOL_CACHE"
          test "$GITHUB_ACTIONS" = true
          echo context-ok
"#,
    )
    .unwrap();
    let output = command(workspace.path())
        .args([
            "--actor",
            "actor-name",
            "--env",
            "GITHUB_REF=refs/heads/topic",
            "--env",
            "GITHUB_SHA=given-sha",
            "--env",
            "GITHUB_RUN_ID=123",
        ])
        .arg("--eventpath")
        .arg(workspace.path().join("event.json"))
        .output()
        .unwrap();
    success(&output);
    assert!(log(&output).contains("context-ok"));
}

#[test]
fn github_context_has_ids_the_workflow_ref_and_runner_names() {
    let workspace = tempfile::tempdir().unwrap();
    std::fs::write(
        workspace.path().join("event.json"),
        r#"{"repository":{"full_name":"owner/project","id":42,"owner":{"id":7}},"sender":{"id":9}}"#,
    )
    .unwrap();
    std::fs::write(
        workspace.path().join("workflow.yml"),
        r#"
on: push
jobs:
  test:
    runs-on: self-hosted
    steps:
      - run: |
          test "$GITHUB_REPOSITORY_ID" = 42
          test "$GITHUB_REPOSITORY_OWNER_ID" = 7
          test "$GITHUB_ACTOR_ID" = 9
          test "$GITHUB_WORKFLOW_REF" = owner/project/.github/workflows/workflow.yml@refs/heads/topic
          test "$GITHUB_WORKFLOW_SHA" = given-sha
          test "$GITHUB_REF_PROTECTED" = false
          test "$GITHUB_RETENTION_DAYS" = 90
          test '${{ github.secret_source }}' = Actions
          test '${{ github.repositoryUrl }}' = git://github.com/owner/project.git
          test -z "${GITHUB_SECRET_SOURCE-}"
          test "$RUNNER_NAME" = '${{ runner.name }}'
          test "$RUNNER_ENVIRONMENT" = self-hosted
          test -n "$RUNNER_TRACKING_ID"
          echo context-ids-ok
"#,
    )
    .unwrap();
    let output = command(workspace.path())
        .args([
            "--env",
            "GITHUB_REF=refs/heads/topic",
            "--env",
            "GITHUB_SHA=given-sha",
        ])
        .arg("--eventpath")
        .arg(workspace.path().join("event.json"))
        .output()
        .unwrap();
    success(&output);
    assert!(log(&output).contains("context-ids-ok"), "{}", log(&output));
}

#[test]
fn offline_remote_action_from_cache() {
    let workspace = tempfile::tempdir().unwrap();
    let cached = workspace
        .path()
        // The cache names a ref by its SHA-256 digest: this one is "v1".
        .join("cache/codeyard-actions/actions/example/action/3bfc269594ef649228e9a74bab00f042efc91d5acc6fbee31a382e80d42388fe");
    std::fs::create_dir_all(cached.join("nested")).unwrap();
    std::fs::write(cached.join("nested/action.yml"), "runs:\n  using: composite\n  steps:\n    - shell: bash\n      run: echo offline-action-ok\n").unwrap();
    for args in [
        vec!["init", "--quiet"],
        vec!["add", "."],
        vec!["commit", "--quiet", "-m", "Add fixture action."],
        vec!["fetch", ".", "HEAD"],
    ] {
        let output = Command::new("git")
            .arg("-C")
            .arg(&cached)
            .args(args)
            .env("GIT_AUTHOR_NAME", "blurskye")
            .env("GIT_AUTHOR_EMAIL", "blurskye@proton.me")
            .env("GIT_COMMITTER_NAME", "blurskye")
            .env("GIT_COMMITTER_EMAIL", "blurskye@proton.me")
            .output()
            .unwrap();
        assert!(output.status.success(), "{}", log(&output));
    }
    std::fs::write(workspace.path().join("workflow.yml"), "on: push\njobs:\n  test:\n    runs-on: self-hosted\n    steps:\n      - uses: example/action/nested@v1\n").unwrap();
    let output = command(workspace.path())
        .arg("--action-offline-mode")
        .output()
        .unwrap();
    success(&output);
    assert!(log(&output).contains("offline-action-ok"));
    std::fs::remove_dir_all(cached).unwrap();
    let output = command(workspace.path())
        .arg("--action-offline-mode")
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(log(&output).contains("offline cache"));
}

#[test]
fn authenticated_progress_reports_the_observed_timeout() {
    let workspace = tempfile::tempdir().unwrap();
    std::fs::write(workspace.path().join("workflow.yml"),"on: push\njobs:\n  test:\n    runs-on: self-hosted\n    timeout-minutes: 0.01\n    steps:\n      - run: sleep 30\n").unwrap();
    std::fs::write(
        workspace.path().join("tokens.json"),
        r#"{"tokens":{"test":"deadline-token"},"completion_secret":"deadline-nonce"}"#,
    )
    .unwrap();
    let output = command(workspace.path())
        .arg("--job-token-file")
        .arg(workspace.path().join("tokens.json"))
        .output()
        .unwrap();
    assert!(!output.status.success());
    let text = log(&output);
    assert!(!text.contains("deadline-token"));
    let events: Vec<codeyard_actions::progress::Event> = text
        .lines()
        .filter_map(|line| line.strip_prefix("::codeyard-progress::deadline-nonce:"))
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert!(
        events.iter().any(|event| matches!(
            event.update,
            codeyard_actions::progress::Update::Finished {
                conclusion: codeyard_actions::progress::Conclusion::TimedOut,
                ..
            }
        )),
        "{text}"
    );
    assert!(
        events.iter().any(|event| matches!(
            event.update,
            codeyard_actions::progress::Update::StepFinished {
                number: 2,
                conclusion: codeyard_actions::progress::Conclusion::TimedOut,
                ..
            }
        )),
        "{text}"
    );
}

#[test]
fn job_timeout_covers_the_whole_job() {
    let started = std::time::Instant::now();
    let output = run("on: push\njobs:\n  test:\n    runs-on: self-hosted\n    timeout-minutes: 0.01\n    steps:\n      - run: sleep 30\n", &[], &[]);
    assert!(!output.status.success());
    assert!(started.elapsed() < std::time::Duration::from_secs(10));
    assert!(log(&output).contains("timed out"));
}

#[test]
fn the_cancel_timeout_bounds_cleanup_steps_after_a_job_timeout() {
    let started = std::time::Instant::now();
    let output = run(
        r#"
on: push
jobs:
  test:
    runs-on: self-hosted
    timeout-minutes: 0.01
    steps:
      - run: sleep 30
      - if: always()
        run: |
          echo cleanup-started
          sleep 30
          echo cleanup-finished
"#,
        &[],
        &["--cancel-timeout-minutes", "0.01"],
    );
    assert!(!output.status.success());
    let text = log(&output);
    assert!(text.contains("cleanup-started"), "{text}");
    assert!(!text.contains("cleanup-finished"), "{text}");
    assert!(text.contains("Job completed: failure"), "{text}");
    assert!(started.elapsed() < std::time::Duration::from_secs(10));
}

#[test]
fn a_fail_fast_cancellation_runs_the_cancelled_legs_cleanup() {
    let started = std::time::Instant::now();
    let output = run(
        r#"
on: push
jobs:
  test:
    runs-on: self-hosted
    strategy:
      matrix:
        leg: [fail, slow]
    steps:
      - run: |
          if [ '${{ matrix.leg }}' = fail ]; then sleep 1; exit 1; fi
          sleep 30
      - if: cancelled()
        run: echo 'cancelled-${{ matrix.leg }}'
"#,
        &[],
        &["--concurrent-jobs", "2"],
    );
    assert!(!output.status.success());
    let text = log(&output);
    assert!(text.contains("cancelled-slow"), "{text}");
    assert!(!text.contains("cancelled-fail"), "{text}");
    assert!(text.contains("Job completed: cancelled"), "{text}");
    assert!(started.elapsed() < std::time::Duration::from_secs(20));
}

#[test]
fn a_step_cannot_redirect_the_event_file_through_a_link() {
    let output = run(
        r#"
on: push
jobs:
  test:
    runs-on: self-hosted
    steps:
      - run: |
          printf keep > outside.txt
          ln -sf "$PWD/outside.txt" "$GITHUB_EVENT_PATH"
      - run: |
          test "$(cat outside.txt)" = keep
          test ! -L "$GITHUB_EVENT_PATH"
          echo event-file-replaced
"#,
        &[],
        &[],
    );
    success(&output);
    assert!(
        log(&output).contains("event-file-replaced"),
        "{}",
        log(&output)
    );
}

#[test]
fn step_debug_from_a_secret_shows_debug_messages_and_echoes_commands() {
    let workspace = tempfile::tempdir().unwrap();
    std::fs::write(
        workspace.path().join("secrets"),
        "ACTIONS_STEP_DEBUG=true\n",
    )
    .unwrap();
    std::fs::write(
        workspace.path().join("workflow.yml"),
        "on: push\njobs:\n  test:\n    runs-on: self-hosted\n    steps:\n      - run: |\n          echo '::debug::shown-debug'\n          echo '::remove-matcher owner=echoed-owner::'\n",
    )
    .unwrap();
    let output = command(workspace.path())
        .arg("--secret-file")
        .arg(workspace.path().join("secrets"))
        .output()
        .unwrap();
    success(&output);
    let text = log(&output);
    assert!(text.contains("debug: shown-debug"), "{text}");
    assert!(
        text.contains("::remove-matcher owner=echoed-owner::"),
        "{text}"
    );
}

#[test]
fn matrix_fail_fast_does_not_start_queued_runs() {
    let output = run(
        r#"
on: push
jobs:
  test:
    runs-on: self-hosted
    strategy:
      max-parallel: 1
      matrix:
        value: [first, queued]
    steps:
      - run: |
          echo 'started-${{ matrix.value }}'
          exit 1
"#,
        &[],
        &[],
    );
    assert!(!output.status.success());
    assert!(log(&output).contains("started-first"));
    assert!(!log(&output).contains("started-queued"));
}

#[cfg(unix)]
#[test]
fn sigterm_stops_the_running_process_group() {
    let workspace = tempfile::tempdir().unwrap();
    std::fs::write(workspace.path().join("workflow.yml"), "on: push\njobs:\n  test:\n    runs-on: self-hosted\n    steps:\n      - run: |\n          echo ready > ready\n          (sleep 2; echo leaked > leaked) &\n          wait\n").unwrap();
    let mut child = command(workspace.path())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while !workspace.path().join("ready").exists() {
        if std::time::Instant::now() > deadline || child.try_wait().unwrap().is_some() {
            let _ = child.kill();
            panic!(
                "Runner did not start its step: {}",
                log(&child.wait_with_output().unwrap())
            );
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    assert_eq!(unsafe { libc::kill(child.id() as i32, libc::SIGTERM) }, 0);
    let output = child.wait_with_output().unwrap();
    assert!(!output.status.success(), "{}", log(&output));
    std::thread::sleep(std::time::Duration::from_secs(3));
    assert!(!workspace.path().join("leaked").exists());
}

#[test]
#[ignore = "needs node"]
fn node_pre_main_post_and_state() {
    let output = run(r#"
on: push
jobs:
  test:
    runs-on: self-hosted
    steps:
      - id: node
        uses: ./action
      - run: test '${{ steps.node.outputs.result }}' = node-ok
"#, &[
        ("action/action.yml", "inputs:\n  value:\n    default: default-input\nruns:\n  using: node20\n  pre: pre.js\n  main: main.js\n  post: post.js\n"),
        ("action/pre.js", "require('fs').appendFileSync(process.env.GITHUB_STATE, 'from_pre=yes\\n'); console.log('pre-ok');"),
        ("action/main.js", "if(process.env.STATE_from_pre !== 'yes' || process.env.INPUT_VALUE !== 'default-input') process.exit(1); require('fs').appendFileSync(process.env.GITHUB_OUTPUT, 'result=node-ok\\n'); require('fs').appendFileSync(process.env.GITHUB_STATE, 'from_main=yes\\n');"),
        ("action/post.js", "if(process.env.STATE_from_main !== 'yes') process.exit(1); console.log('post-ok');"),
    ], &[]);
    success(&output);
    assert!(log(&output).contains("pre-ok"));
    assert!(log(&output).contains("post-ok"));
}

#[test]
#[ignore = "needs podman"]
fn podman_job_services_and_docker_action() {
    let output = run(r#"
on: push
jobs:
  test:
    runs-on: self-hosted
    container: alpine:3.21
    services:
      web:
        image: nginx:alpine
    steps:
      - shell: sh
        run: wget -qO- http://web/ | grep nginx
      - uses: ./docker
      # Without action metadata, the step's entrypoint and args inputs start the container.
      - uses: docker://alpine:3.21
        with:
          Entrypoint: /bin/sh
          ARGS: -c "echo docker-url-$INPUT_EXTRA"
          extra: value
"#, &[
        ("docker/action.yml", "runs:\n  using: docker\n  image: Dockerfile\n"),
        ("docker/Dockerfile", "FROM alpine:3.21\nENTRYPOINT [\"sh\", \"-c\", \"test -d \\\"$GITHUB_WORKSPACE\\\" && echo docker-ok\"]\n"),
    ], &["--container-options", "--label codeyard-actions-test=yes", "--pull=false"]);
    success(&output);
    assert!(log(&output).contains("docker-ok"));
    assert!(log(&output).contains("docker-url-value"));
}
