//! Tests examples from actions/runner's `Sdk/WorkflowParser` and the workflow syntax documentation.
use super::*;
use crate::expressions::Value;

fn workflow(job: &str) -> Workflow {
    parse(&format!(
        "on: push\njobs:\n  build:\n{}",
        job.lines()
            .map(|s| format!("    {s}\n"))
            .collect::<String>()
    ))
    .unwrap()
}

fn plan(job: &str) -> Vec<PlannedJob> {
    expand(
        &workflow(job),
        "build",
        &Contexts::default(),
        Status::default(),
    )
    .unwrap()
    .unwrap()
}

fn object(values: &[(&str, Value)]) -> Value {
    Value::Object(
        values
            .iter()
            .map(|(k, v)| ((*k).into(), v.clone()))
            .collect(),
    )
}

#[test]
fn event_forms() {
    for on in [
        "push",
        "[push, pull_request]",
        "{push: {branches: [main], paths: ['src/**']}, workflow_dispatch: null}",
    ] {
        let w = parse(&format!(
            "on: {on}\njobs: {{build: {{runs-on: ubuntu-latest, steps: [{{run: echo hi}}]}}}}"
        ))
        .unwrap();
        assert!(listens_to(&w, "push"));
        assert!(!listens_to(&w, "issues"));
    }
}

#[test]
fn workflow_settings_and_events() {
    let w = parse(
        r#"
name: CI
run-name: Deploy ${{ inputs.target }} by @${{ github.actor }}
on:
  push:
    branches: [main, 'releases/**']
    tags-ignore: ['v1.*']
    paths: ['**.rs', '!docs/**']
  pull_request:
    types: [opened, synchronize]
  workflow_dispatch:
    inputs:
      target:
        description: Deployment target
        type: choice
        options: [staging, production]
        required: true
        default: staging
      debug:
        type: boolean
        default: false
      count:
        type: number
        default: 3
      environment:
        type: environment
  schedule:
    - cron: '30 5 * * 1,3'
env:
  SERVER: production
  TOKEN: ${{ secrets.TOKEN }}
defaults:
  run:
    shell: bash
    working-directory: scripts
permissions:
  contents: read
  id-token: write
concurrency:
  group: ${{ github.workflow }}-${{ github.ref }}
  cancel-in-progress: true
jobs:
  build:
    runs-on: ubuntu-latest
    steps:
      - run: echo hello
"#,
    )
    .unwrap();
    assert!(listens_to(&w, "schedule"));
    assert!(listens_to(&w, "workflow_dispatch"));
    assert_eq!(w.fields.get("name").unwrap().as_str(), Some("CI"));
}

#[test]
fn reusable_workflow_inputs_outputs_and_secrets() {
    let w = parse(
        r#"
on:
  workflow_call:
    inputs:
      username:
        type: string
        default: ${{ github.actor }}
      dry-run:
        type: boolean
        default: false
    secrets:
      token:
        required: true
    outputs:
      digest:
        description: Image digest
        value: ${{ jobs.build.outputs.digest }}
jobs:
  build:
    uses: octo/repo/.github/workflows/build.yml@main
    with:
      username: ${{ inputs.username }}
    secrets: inherit
"#,
    )
    .unwrap();
    assert!(listens_to(&w, "workflow_call"));
    let plans = expand(&w, "build", &Contexts::default(), Status::default())
        .unwrap()
        .unwrap();
    assert_eq!(plans.len(), 1);
    let call = plans[0].call.as_ref().unwrap();
    assert_eq!(
        call.get("uses").unwrap().as_str(),
        Some("octo/repo/.github/workflows/build.yml@main")
    );
    assert_eq!(call.get("secrets").unwrap().as_str(), Some("inherit"));
    assert!(plans[0].steps.is_empty());
}

#[test]
fn parse_composite_steps() {
    let step = parse_step(
        &serde_yaml::from_str(
            r#"
id: greet
name: Greet ${{ inputs.who }}
if: ${{ !cancelled() }}
run: echo "Hello ${{ inputs.who }}"
shell: bash
working-directory: ${{ github.workspace }}
continue-on-error: false
timeout-minutes: 2
env:
  WHO: ${{ inputs.who }}
"#,
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(step.id.as_deref(), Some("greet"));
    assert_eq!(step.continue_on_error.as_deref(), Some("false"));
    assert!(matches!(step.action, StepAction::Run { .. }));
    let uses = parse_step(
        &serde_yaml::from_str("uses: actions/checkout@v4\nwith: {fetch-depth: 0}").unwrap(),
    )
    .unwrap();
    assert!(
        matches!(uses.action, StepAction::Uses { reference, with } if reference == "actions/checkout@v4" && with["fetch-depth"] == "0")
    );
}

#[test]
fn invalid_step_keys_and_ids() {
    for yaml in [
        "run: echo hi\nuses: actions/checkout@v4",
        "uses: actions/checkout@v4\nshell: bash",
        "run: hi\nwith: {x: y}",
        "name: hi",
        "run: hi\nid: 1bad",
        "run: hi\nid: __reserved",
        "uses: '${{ inputs.action }}'",
        "run: hi\nif: secrets.TOKEN",
        "run: hi\ntimeout-minutes: soon",
        "run: hi\ncontinue-on-error: yes",
        "run: hi\nunknown: true",
    ] {
        let value = serde_yaml::from_str(yaml).unwrap();
        assert!(parse_step(&value).is_err(), "accepted {yaml}");
    }
}

#[test]
fn rejects_invalid_workflows() {
    for (yaml, expected) in [
        ("on: push\nwat: x\njobs: {}", "Unexpected value 'wat'"),
        ("on: push\njobs: {}", "at least one job"),
        ("jobs: {}", "on"),
        ("on: nonsense\njobs: {}", "Unexpected event"),
        (
            "on: {push: {branches: [main], branches-ignore: [dev]}}\njobs: {}",
            "cannot use both",
        ),
        (
            "on: {schedule: [{cron: '@daily'}]}\njobs: {}",
            "Invalid cron",
        ),
        (
            "on: push\npermissions: {id-token: read}\njobs: {}",
            "Expected 'write'",
        ),
        (
            "on: push\npermissions: {models: write}\njobs: {}",
            "Expected 'read'",
        ),
        (
            "on: push\ndefaults: {run: {shell: '${{ inputs.shell }}'}}\njobs: {}",
            "not allowed",
        ),
        (
            "on: push\nrun-name: '${{ secrets.TOKEN }}'\njobs: {}",
            "Unrecognized named-value",
        ),
        (
            "on: push\nconcurrency: {cancel-in-progress: true}\njobs: {}",
            "group",
        ),
    ] {
        let error = format!("{:#}", parse(yaml).unwrap_err());
        assert!(
            error.contains(expected),
            "{error:?} did not contain {expected:?}"
        );
    }
}

#[test]
fn yaml_errors_have_locations() {
    let error = format!("{:#}", parse("on: push\njobs: [\n").unwrap_err());
    assert!(error.contains("line"), "{error}");
    assert!(error.contains("column"), "{error}");
    assert!(parse("on: push\non: issues\njobs: {}").is_err());
}

#[test]
fn rejects_invalid_job_fields_and_contexts() {
    for job in [
        "runs-on: ubuntu-latest\nsteps: [{run: hi}, {run: hi, id: same}, {run: hi, id: SAME}]",
        "runs-on: ubuntu-latest\nsteps: [{run: hi}]\nif: matrix.os == 'linux'",
        "runs-on: '${{ secrets.RUNNER }}'\nsteps: [{run: hi}]",
        "runs-on: ubuntu-latest\nsteps: [{run: hi}]\nstrategy: {matrix: {os: [linux]}, fail-fast: '${{ matrix.os }}'}",
        "runs-on: ubuntu-latest\nsteps: [{run: hi}]\nenv: {BAD: '${{ env.OTHER }}'}",
        "runs-on: ubuntu-latest\nsteps: [{run: hi}]\ntimeout-minutes: never",
        "runs-on: ubuntu-latest\nsteps: [{run: hi}]\ncontainer: {image: alpine, wat: x}",
        "runs-on: ubuntu-latest\nsteps: [{run: hi}]\nenvironment: {url: x}",
        "runs-on: ubuntu-latest\nsteps: [{run: hi}]\nwat: x",
        "runs-on: ubuntu-latest\nsteps: []",
        "steps: [{run: hi}]",
    ] {
        let yaml = format!("on: push\njobs:\n  build:\n{}", job.lines().map(|s| format!("    {s}\n")).collect::<String>());
        assert!(parse(&yaml).is_err(), "accepted {job}");
    }
    for id in ["1bad", "bad.id", "__reserved", "bad id"] {
        assert!(parse(&format!(
            "on: push\njobs:\n  {id}: {{runs-on: linux, steps: [{{run: hi}}]}}"
        ))
        .is_err());
    }
}

#[test]
fn expression_context_scanner_handles_literals_functions_and_properties() {
    workflow("runs-on: ubuntu-latest\nif: contains(github.ref, 'matrix.nope') && fromJSON('true')\nsteps:\n  - run: ${{ format('hello }} {0}', github.actor) }}\n    if: hashFiles('**/Cargo.lock') != ''\n  - run: ${{ env.lowercase }}\n    shell: ${{ matrix.shell }}");
}

#[test]
fn dependency_order() {
    let w = parse(
        r#"
on: push
jobs:
  deploy:
    needs: [test, lint]
    runs-on: linux
    steps: [{run: deploy}]
  test:
    needs: build
    runs-on: linux
    steps: [{run: test}]
  lint:
    runs-on: linux
    steps: [{run: lint}]
  build:
    runs-on: linux
    steps: [{run: build}]
"#,
    )
    .unwrap();
    assert_eq!(job_order(&w).unwrap(), ["build", "test", "lint", "deploy"]);
}

#[test]
fn unknown_needs_and_cycles() {
    assert!(
        parse("on: push\njobs: {a: {needs: missing, runs-on: linux, steps: [{run: hi}]}}")
            .unwrap_err()
            .to_string()
            .contains("unknown job 'missing'")
    );
    assert!(parse("on: push\njobs: {a: {needs: b, runs-on: linux, steps: [{run: hi}]}, b: {needs: a, runs-on: linux, steps: [{run: hi}]}}").unwrap_err().to_string().contains("cycle"));
}

#[test]
fn plain_job_defaults() {
    let jobs = plan("runs-on: ubuntu-latest\nsteps: [{run: echo hello}]");
    let job = &jobs[0];
    assert_eq!(job.name, "build");
    assert_eq!(job.matrix, Value::Null);
    assert_eq!(job.timeout_minutes, Some(360.0));
    assert!(job.fail_fast);
    assert!(!job.continue_on_error);
    assert_eq!(job.max_parallel, None);
    assert_eq!(job.concurrency, None);
}

#[test]
fn job_concurrency_forms() {
    let jobs = plan("runs-on: linux\nconcurrency: deploy-${{ matrix.env }}\nstrategy: {matrix: {env: [prod]}}\nsteps: [{run: hi}]");
    assert_eq!(
        jobs[0].concurrency,
        Some(Concurrency {
            group: "deploy-prod".into(),
            cancel_in_progress: false
        })
    );
    let jobs = plan("runs-on: linux\nconcurrency: {group: deploy, cancel-in-progress: true}\nsteps: [{run: hi}]");
    assert_eq!(
        jobs[0].concurrency,
        Some(Concurrency {
            group: "deploy".into(),
            cancel_in_progress: true
        })
    );
}

#[test]
fn matrix_cross_product_preserves_axis_order() {
    let jobs = plan("runs-on: ${{ matrix.os }}\nstrategy:\n  matrix:\n    os: [ubuntu-latest, windows-latest]\n    node: [18, 20]\nsteps: [{run: echo hi}]");
    assert_eq!(
        jobs.iter().map(|j| j.name.as_str()).collect::<Vec<_>>(),
        [
            "build (ubuntu-latest, 18)",
            "build (ubuntu-latest, 20)",
            "build (windows-latest, 18)",
            "build (windows-latest, 20)"
        ]
    );
    assert_eq!(jobs[2].runs_on, ["windows-latest"]);
}

#[test]
fn documented_matrix_include() {
    let jobs = plan(
        r#"
runs-on: ubuntu-latest
strategy:
  matrix:
    fruit: [apple, pear]
    animal: [cat, dog]
    include:
      - color: green
      - color: pink
        animal: cat
      - fruit: apple
        shape: circle
      - fruit: banana
      - fruit: banana
        animal: cat
steps: [{run: hi}]
"#,
    );
    let s = |v: &str| Value::String(v.into());
    assert_eq!(jobs.len(), 6);
    assert_eq!(
        jobs[0].matrix,
        object(&[
            ("fruit", s("apple")),
            ("animal", s("cat")),
            ("color", s("pink")),
            ("shape", s("circle"))
        ])
    );
    assert_eq!(
        jobs[1].matrix,
        object(&[
            ("fruit", s("apple")),
            ("animal", s("dog")),
            ("color", s("green")),
            ("shape", s("circle"))
        ])
    );
    assert_eq!(
        jobs[2].matrix,
        object(&[
            ("fruit", s("pear")),
            ("animal", s("cat")),
            ("color", s("pink"))
        ])
    );
    assert_eq!(jobs[4].matrix, object(&[("fruit", s("banana"))]));
    assert_eq!(
        jobs[5].matrix,
        object(&[("fruit", s("banana")), ("animal", s("cat"))])
    );
    assert_eq!(jobs[0].name, "build (apple, cat)");
}

#[test]
fn exclude_before_include_can_restore_combinations() {
    let jobs = plan(
        r#"
runs-on: linux
strategy:
  matrix:
    os: [macos-latest, windows-latest]
    version: [12, 14, 16]
    environment: [staging, production]
    exclude:
      - os: macos-latest
        version: 12
        environment: production
      - os: windows-latest
        version: 16
    include:
      - os: windows-latest
        version: 16
        environment: production
steps: [{run: hi}]
"#,
    );
    assert_eq!(jobs.len(), 10);
    assert_eq!(
        jobs.last().unwrap().name,
        "build (windows-latest, 16, production)"
    );
}

#[test]
fn include_only_and_object_vectors() {
    let jobs = plan(
        r#"
runs-on: ${{ matrix.os }}
name: Node ${{ matrix.node.version }} on ${{ matrix.os }}
strategy:
  matrix:
    include:
      - os: ubuntu-latest
        node: {version: 20, env: production}
      - os: windows-latest
        node: {version: 18}
steps: [{run: hi}]
"#,
    );
    assert_eq!(jobs.len(), 2);
    assert_eq!(jobs[0].name, "Node 20 on ubuntu-latest");
    assert_eq!(jobs[1].runs_on, ["windows-latest"]);
}

#[test]
fn filters_match_nested_leaves_and_case_insensitively() {
    let jobs = plan(
        r#"
runs-on: linux
strategy:
  matrix:
    node: [{version: 18, npm: 8}, {version: 20, npm: 10}]
    os: [LINUX, windows]
    exclude:
      - node: {version: 18}
        os: linux
steps: [{run: hi}]
"#,
    );
    assert_eq!(jobs.len(), 3);
    assert_eq!(jobs[0].name, "build (18, 8, windows)");
}

#[test]
fn job_if_runs_before_matrix_and_uses_status() {
    let w = workflow("if: false\nruns-on: linux\nstrategy: {matrix: '${{ fromJSON(inputs.invalid) }}'}\nsteps: [{run: hi}]");
    assert_eq!(
        expand(&w, "build", &Contexts::default(), Status::default()).unwrap(),
        None
    );
    let w = workflow("runs-on: linux\nsteps: [{run: hi}]");
    assert_eq!(
        expand(
            &w,
            "build",
            &Contexts::default(),
            Status {
                failed: true,
                cancelled: false
            }
        )
        .unwrap(),
        None
    );
    let w = workflow("if: always()\nruns-on: linux\nsteps: [{run: hi}]");
    assert!(expand(
        &w,
        "build",
        &Contexts::default(),
        Status {
            failed: true,
            cancelled: false
        }
    )
    .unwrap()
    .is_some());
}

#[test]
fn dynamic_matrix_and_vector_from_needs() {
    let mut contexts = Contexts::default();
    contexts.values.insert(
        "needs".into(),
        object(&[(
            "define",
            object(&[(
                "outputs",
                object(&[
                    (
                        "matrix",
                        Value::String(r#"{"include":[{"os":"linux"},{"os":"windows"}]}"#.into()),
                    ),
                    ("versions", Value::String("[18,20]".into())),
                ]),
            )]),
        )]),
    );
    for matrix in [
        "${{ fromJSON(needs.define.outputs.matrix) }}",
        "{version: '${{ fromJSON(needs.define.outputs.versions) }}'}",
    ] {
        let w = workflow(&format!(
            "runs-on: linux\nstrategy:\n  matrix: {matrix}\nsteps: [{{run: hi}}]"
        ));
        assert_eq!(
            expand(&w, "build", &contexts, Status::default())
                .unwrap()
                .unwrap()
                .len(),
            2
        );
    }
}

#[test]
fn dynamic_matrix_preserves_json_name_order() {
    let mut contexts = Contexts::default();
    contexts.values.insert("needs".into(), object(&[("define", object(&[("outputs", object(&[("matrix", Value::String(r#"{"include":[{"project":"foo","config":"Debug"},{"project":"bar","config":"Release"}]}"#.into()))]))]))]));
    let w = workflow("runs-on: linux\nstrategy: {matrix: '${{ fromJSON(needs.define.outputs.matrix) }}'}\nsteps: [{run: hi}]");
    let jobs = expand(&w, "build", &contexts, Status::default())
        .unwrap()
        .unwrap();
    assert_eq!(jobs[0].name, "build (foo, Debug)");
    assert_eq!(jobs[1].name, "build (bar, Release)");
    let jobs = plan(
        r#"runs-on: linux
strategy:
  matrix: ${{ fromJSON('{"z":[20],"a":[18]}') }}
steps: [{run: hi}]"#,
    );
    assert_eq!(jobs[0].name, "build (20, 18)");
    let jobs = plan(
        r#"runs-on: linux
strategy:
  matrix: ${{ fromJSON('null') || fromJSON('{"x":[1]}') }}
steps: [{run: hi}]"#,
    );
    assert_eq!(jobs[0].name, "build (1)");
}

#[test]
fn dynamic_matrix_reads_json_unicode_escapes() {
    let jobs = plan(
        r#"runs-on: linux
strategy:
  matrix: ${{ fromJSON('{"project":["\ud83d\ude00"]}') }}
steps: [{run: hi}]"#,
    );
    assert_eq!(jobs[0].name, "build (\u{1F600})");
}

#[test]
fn excluded_matrix_leaves_still_count_toward_the_work_limit() {
    let axes: String = (0..40)
        .map(|axis| format!("    a{axis}: [0, 1]\n"))
        .collect();
    let w = workflow(&format!(
        "runs-on: linux\nstrategy:\n  matrix:\n{axes}    exclude: [{{a39: 0}}, {{a39: 1}}]\nsteps: [{{run: hi}}]"
    ));
    let error = expand(&w, "build", &Contexts::default(), Status::default()).unwrap_err();
    assert!(format!("{error:#}").contains("too large"), "{error:#}");
}

#[test]
fn matrix_axis_values_keep_types() {
    let jobs = plan("runs-on: linux\nstrategy:\n  matrix:\n    os: [linux]\n    experimental: [true, false]\n    timeout: [15]\nsteps: [{run: hi}]");
    assert_eq!(
        jobs[0].matrix,
        object(&[
            ("os", Value::String("linux".into())),
            ("experimental", Value::Bool(true)),
            ("timeout", Value::Number(15.0))
        ])
    );
    let contexts = Contexts {
        values: BTreeMap::from([("matrix".into(), jobs[0].matrix.clone())]),
    };
    assert_eq!(
        crate::expressions::interpolate("${{ matrix.timeout }}", &contexts, Status::default())
            .unwrap(),
        Value::Number(15.0)
    );
    let jobs = plan("runs-on: linux\ntimeout-minutes: ${{ matrix.timeout }}\nstrategy: {matrix: {timeout: [15]}}\nsteps: [{run: hi}]");
    assert_eq!(jobs[0].timeout_minutes, Some(15.0));
}

#[test]
fn evaluates_job_fields_and_leaves_runner_templates() {
    let jobs = plan(
        r#"
runs-on:
  group: larger-runners
  labels: [self-hosted, '${{ matrix.os }}']
strategy:
  fail-fast: false
  max-parallel: 2
  matrix:
    os: [linux]
    experimental: [true, false]
    timeout: [15]
continue-on-error: ${{ matrix.experimental }}
timeout-minutes: ${{ matrix.timeout }}
environment:
  name: ${{ matrix.os }}
  url: ${{ steps.deploy.outputs.url }}
permissions: read-all
concurrency:
  group: deploy-${{ matrix.os }}
  cancel-in-progress: ${{ github.ref != 'refs/heads/main' }}
env:
  OS: ${{ matrix.os }}
defaults:
  run:
    shell: ${{ matrix.os == 'linux' && 'bash' || 'pwsh' }}
container: node:20
services:
  redis:
    image: redis:7
    credentials: {username: '${{ vars.USER }}', password: '${{ secrets.PASSWORD }}'}
    ports: ['6379:6379']
    volumes: ['data:/data']
    options: --health-cmd redis-cli
steps:
  - run: echo '${{ matrix.os }}'
outputs:
  result: ${{ steps.test.outputs.result }}
"#,
    );
    assert!(!jobs[0].fail_fast);
    assert_eq!(jobs[0].max_parallel, Some(2));
    assert_eq!(jobs[0].timeout_minutes, Some(15.0));
    assert!(jobs[0].continue_on_error);
    assert!(!jobs[1].continue_on_error);
    assert_eq!(jobs[0].runs_on, ["self-hosted", "linux"]);
    assert_eq!(jobs[0].runs_on_group.as_deref(), Some("larger-runners"));
    assert_eq!(jobs[0].env["OS"], "${{ matrix.os }}");
    assert_eq!(
        jobs[0].default_shell.as_deref(),
        Some("${{ matrix.os == 'linux' && 'bash' || 'pwsh' }}")
    );
    assert_eq!(jobs[0].container.as_ref().unwrap().image, "node:20");
    assert_eq!(
        jobs[0].services["redis"].credentials.as_ref().unwrap().1,
        "${{ secrets.PASSWORD }}"
    );
    assert_eq!(
        jobs[0].outputs["result"],
        "${{ steps.test.outputs.result }}"
    );
}

#[test]
fn merge_env_and_defaults() {
    let w = parse(
        r#"
on: push
env: {A: workflow, B: workflow}
defaults: {run: {shell: bash, working-directory: scripts}}
jobs:
  build:
    runs-on: linux
    env: {B: job, C: job}
    defaults: {run: {shell: sh}}
    steps: [{run: hi}]
"#,
    )
    .unwrap();
    let job = expand(&w, "build", &Contexts::default(), Status::default())
        .unwrap()
        .unwrap()
        .remove(0);
    assert_eq!(
        job.env,
        BTreeMap::from([
            ("A".into(), "workflow".into()),
            ("B".into(), "job".into()),
            ("C".into(), "job".into())
        ])
    );
    assert_eq!(job.default_shell.as_deref(), Some("sh"));
    assert_eq!(job.default_working_directory.as_deref(), Some("scripts"));
}

#[test]
fn strategy_context_has_index_and_total() {
    let jobs = plan("runs-on: linux\nname: '${{ strategy.job-index }} of ${{ strategy.job-total }}'\nstrategy: {matrix: {x: [a, b]}}\nsteps: [{run: hi}]");
    assert_eq!(jobs[0].name, "0 of 2");
    assert_eq!(jobs[1].name, "1 of 2");
}

#[test]
fn matrix_limit_and_invalid_filters() {
    let vector = (0..257)
        .map(|n| n.to_string())
        .collect::<Vec<_>>()
        .join(",");
    let w = workflow(&format!(
        "runs-on: linux\nstrategy: {{matrix: {{x: [{vector}]}}}}\nsteps: [{{run: hi}}]"
    ));
    assert!(format!(
        "{:#}",
        expand(&w, "build", &Contexts::default(), Status::default()).unwrap_err()
    )
    .contains("256"));
    for matrix in [
        "{x: []}",
        "{x: [1], exclude: [{unknown: 1}]}",
        "{x: [1], include: [{}]}",
        "{x: [1], exclude: [{}]}",
        "{exclude: [{x: 1}]}",
    ] {
        let yaml = format!("on: push\njobs:\n  build:\n    runs-on: linux\n    strategy: {{matrix: {matrix}}}\n    steps: [{{run: hi}}]");
        assert!(
            parse(&yaml).is_err(),
            "accepted invalid static matrix {matrix}"
        );
    }
    let jobs = plan(&format!(
        "runs-on: linux\nstrategy: {{matrix: {{x: [{}]}}}}\nsteps: [{{run: hi}}]",
        (0..256)
            .map(|n| n.to_string())
            .collect::<Vec<_>>()
            .join(",")
    ));
    assert_eq!(jobs.len(), 256);
}

#[test]
fn invalid_expression_syntax_is_rejected_without_evaluation() {
    for source in [
        "run: '${{ github.actor == }}'",
        "run: '${{ contains(github.actor) }}'",
        "run: '${{ }}'",
        "run: '${{ github.actor'",
        "run: hi\nif: github.actor &&",
        "run: hi\nif: success(1)",
    ] {
        let value = serde_yaml::from_str(source).unwrap();
        assert!(parse_step(&value).is_err(), "accepted {source}");
    }
    // These would fail if parsing evaluated them rather than just checking grammar.
    let value = serde_yaml::from_str("run: '${{ fromJSON(inputs.not_yet_available) }}'").unwrap();
    assert!(parse_step(&value).is_ok());
    let value = serde_yaml::from_str("run: '${{ hashFiles(inputs.not_yet_available) }}'").unwrap();
    assert!(parse_step(&value).is_ok());
}

#[test]
fn rejects_bad_action_references() {
    for reference in [
        "checkout",
        "owner/repo",
        "owner/repo@",
        "owner/repo@ref@ref",
        "repo@ref",
    ] {
        let value = serde_yaml::from_str(&format!("uses: {reference}")).unwrap();
        assert!(parse_step(&value).is_err(), "accepted {reference}");
    }
    for reference in [
        "owner/repo/path@main",
        "./actions/local",
        "docker://alpine:3",
    ] {
        let value = serde_yaml::from_str(&format!("uses: {reference}")).unwrap();
        assert!(parse_step(&value).is_ok(), "rejected {reference}");
    }
}

#[test]
fn excludes_apply_before_matrix_limit() {
    let vector = (0..257)
        .map(|n| n.to_string())
        .collect::<Vec<_>>()
        .join(",");
    let jobs = plan(&format!("runs-on: linux\nstrategy: {{matrix: {{x: [{vector}], exclude: [{{x: 256}}]}}}}\nsteps: [{{run: hi}}]"));
    assert_eq!(jobs.len(), 256);
}

#[test]
fn dependencies_ignore_case_but_keep_canonical_ids() {
    let w = parse("on: push\njobs: {Build: {runs-on: linux, steps: [{run: hi}]}, deploy: {needs: BUILD, runs-on: linux, steps: [{run: hi}]}}").unwrap();
    assert_eq!(job_order(&w).unwrap(), ["Build", "deploy"]);
    assert_eq!(
        expand(&w, "deploy", &Contexts::default(), Status::default())
            .unwrap()
            .unwrap()[0]
            .needs,
        ["Build"]
    );
}

#[test]
fn container_shorthand_prefix_and_empty_job_container() {
    let jobs = plan("runs-on: linux\ncontainer: docker://alpine:3\nservices: {redis: {image: 'docker://redis:7'}}\nsteps: [{run: hi}]");
    assert_eq!(jobs[0].container.as_ref().unwrap().image, "alpine:3");
    assert_eq!(jobs[0].services["redis"].image, "redis:7");
    assert!(plan("runs-on: linux\ncontainer: ''\nsteps: [{run: hi}]")[0]
        .container
        .is_none());
    // An empty service image is accepted; the runner starts no container for that service.
    assert!(
        plan("runs-on: linux\nservices: {redis: ''}\nsteps: [{run: hi}]")[0].services["redis"]
            .image
            .is_empty()
    );
    for container in ["container: docker://", "services: {redis: {env: {A: b}}}"] {
        let yaml = format!("on: push\njobs:\n  build:\n    runs-on: linux\n    {container}\n    steps: [{{run: hi}}]");
        assert!(parse(&yaml).is_err(), "accepted {container}");
    }
}

#[test]
fn runtime_types_are_checked() {
    for field in [
        "timeout-minutes: ${{ 'bad' }}",
        "continue-on-error: ${{ 'false' }}",
        "strategy: {max-parallel: 0}",
        "strategy: {max-parallel: 1.5}",
        "strategy: {fail-fast: '${{ 1 }}'}",
        r#"strategy: ${{ fromJSON('{"unknown":1}') }}"#,
    ] {
        let w = workflow(&format!("runs-on: linux\nsteps: [{{run: hi}}]\n{field}"));
        assert!(
            expand(&w, "build", &Contexts::default(), Status::default()).is_err(),
            "accepted {field}"
        );
    }
}
