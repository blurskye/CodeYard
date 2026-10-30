//! Job and step execution following actions/runner's `Runner.Worker/JobRunner.cs` and `StepsRunner.cs`.
mod actions;
pub(crate) use actions::ActionReference;
mod commands;
mod containers;
mod environment_values;
mod matchers;
mod output;
mod process;
pub(crate) use process::{read_lines, output as capture_output};
mod reusable;

use crate::expressions::{self, Contexts, Status, Value};
use crate::progress::{
    merge_outputs, Conclusion, JobResult, Matrix, Replay, State as JobState, Update,
};
use crate::workflow::{self, Container, PlannedJob, Step, StepAction};
use anyhow::{bail, Context, Result};
use clap::Parser;
use commands::{Commands, Log, Progress};
pub use commands::Masker;
use containers::Podman;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::process::Command;
use tokio::sync::watch;

#[derive(Clone, Debug, Parser)]
#[command(
    name = "codeyard-actions",
    version,
    about = "Run GitHub Actions workflows",
    // Like act, a repeated single-value flag keeps its last value.
    args_override_self = true
)]
pub struct Options {
    pub event: String,
    #[arg(short = 'W')]
    pub workflow: PathBuf,
    #[arg(long, default_value = ".")]
    pub directory: PathBuf,
    #[arg(long = "env")]
    pub env: Vec<String>,
    #[arg(long)]
    pub env_file: Option<PathBuf>,
    #[arg(long, default_value = "codeyard")]
    pub actor: String,
    #[arg(long)]
    pub eventpath: Option<PathBuf>,
    #[arg(short = 'P')]
    pub platforms: Vec<String>,
    #[arg(long, default_value = "true", action = clap::ArgAction::Set)]
    pub pull: bool,
    #[arg(long, default_value = "1", value_parser = clap::value_parser!(u32).range(1..))]
    pub concurrent_jobs: u32,
    #[arg(long, default_value = "", allow_hyphen_values = true)]
    pub container_options: String,
    #[arg(long, default_value = "-", allow_hyphen_values = true)]
    pub container_daemon_socket: String,
    #[arg(long)]
    pub secret_file: Option<PathBuf>,
    #[arg(long)]
    pub var_file: Option<PathBuf>,
    /// Private job bindings and environment values, separate from workflow-wide contexts.
    #[arg(long)]
    pub environment_values_file: Option<PathBuf>,
    /// A private JSON file containing job-id credentials, read once before execution.
    #[arg(long)]
    pub job_token_file: Option<PathBuf>,
    /// Private replay state, written atomically after completed jobs. Never mount it into a job.
    #[arg(long)]
    pub job_state_file: Option<PathBuf>,
    #[arg(long, hide = true)]
    pub replay_file: Option<PathBuf>,
    /// A private directory where the worker answers job concurrency requests, one file per
    /// job run. Never mount it into a job.
    #[arg(long)]
    pub concurrency_dir: Option<PathBuf>,
    #[arg(long, hide = true)]
    pub debug: bool,
    #[arg(long)]
    pub no_cache_server: bool,
    #[arg(long)]
    pub action_offline_mode: bool,
    #[arg(long, hide = true)]
    pub deny_docker: bool,
    #[arg(skip)]
    containment_lost: Arc<std::sync::atomic::AtomicBool>,
}

fn validate_replay_matrix(expected: &BTreeMap<u32, serde_json::Value>, plans: &[PlannedJob]) -> Result<()> {
    anyhow::ensure!(expected.len() == plans.len() && plans.iter().enumerate().all(|(index, plan)| {
        expected.get(&(index as u32)).is_some_and(|matrix| Value::from_json(matrix) == plan.matrix)
    }), "Matrix context has changed; re-run the whole workflow");
    Ok(())
}

fn object(values: &BTreeMap<String, String>) -> Value {
    Value::Object(
        values
            .iter()
            .map(|(key, value)| (key.clone(), Value::String(value.clone())))
            .collect(),
    )
}

fn text(template: &str, contexts: &Contexts, status: Status) -> Result<String> {
    Ok(expressions::interpolate(template, contexts, status)?.to_text())
}

fn boolean(template: &str, contexts: &Contexts, status: Status) -> Result<bool> {
    let value = if template.contains("${{") {
        expressions::interpolate(template, contexts, status)?
    } else {
        expressions::evaluate(template, contexts, status)?
    };
    match value {
        Value::Bool(value) => Ok(value),
        _ => bail!("Expected a boolean value"),
    }
}

fn minutes(value: f64) -> Result<Duration> {
    workflow::validate_timeout_minutes(value)?;
    Ok(Duration::from_secs_f64(value * 60.0))
}

// Job success() reads direct dependencies; failure() also reads their ancestors. Skipped
// dependencies make neither failure() nor cancelled() true. Preserve these distinctions here.
fn condition_with_success(condition: &str, success: bool) -> (String, bool) {
    let bytes = condition.as_bytes();
    let mut result = String::new();
    let mut explicit_status = false;
    let mut index = 0;
    let mut copied = 0;
    while index < bytes.len() {
        if bytes[index] == b'\'' {
            index += 1;
            while index < bytes.len() {
                if bytes[index] == b'\'' {
                    index += 1;
                    if bytes.get(index) == Some(&b'\'') {
                        index += 1;
                    } else {
                        break;
                    }
                } else {
                    index += 1;
                }
            }
        } else if bytes[index].is_ascii_alphabetic() || bytes[index] == b'_' {
            let start = index;
            while index < bytes.len()
                && (bytes[index].is_ascii_alphanumeric() || bytes[index] == b'_')
            {
                index += 1;
            }
            let name = condition[start..index].to_ascii_lowercase();
            let mut end = index;
            while bytes.get(end).is_some_and(u8::is_ascii_whitespace) {
                end += 1;
            }
            if bytes.get(end) == Some(&b'(')
                && ["success", "failure", "cancelled", "always"].contains(&name.as_str())
            {
                explicit_status = true;
                if name == "success" {
                    end += 1;
                    while bytes.get(end).is_some_and(u8::is_ascii_whitespace) {
                        end += 1;
                    }
                    if bytes.get(end) == Some(&b')') {
                        result.push_str(&condition[copied..start]);
                        result.push_str(&format!("(always() && {success})"));
                        index = end + 1;
                        copied = index;
                    }
                }
            }
        } else {
            index += 1;
        }
    }
    result.push_str(&condition[copied..]);
    (result, explicit_status)
}

fn expand_with_dependency_status(
    raw: &serde_yaml::Value,
    id: &str,
    contexts: &Contexts,
    status: Status,
    success: bool,
) -> Result<Option<Vec<PlannedJob>>> {
    let condition = raw
        .get("jobs")
        .and_then(|jobs| jobs.get(id))
        .and_then(|job| job.get("if"));
    let condition = condition.map(|value| workflow::scalar_text(value, "job.if")).transpose()?.unwrap_or_else(|| "success()".into());
    let condition = condition.trim();
    let condition = condition
        .strip_prefix("${{")
        .and_then(|value| value.strip_suffix("}}"))
        .unwrap_or(condition)
        .trim();
    let (condition, explicit_status) = condition_with_success(condition, success);
    if !explicit_status && !success {
        return Ok(None);
    }
    let condition = if explicit_status {
        condition
    } else {
        format!("always() && ({condition})")
    };
    if !expressions::evaluate_condition(&condition, contexts, status)? {
        return Ok(None);
    }
    // The condition was checked above; do not let the planner re-evaluate success() without
    // the skipped dependency information that only the executor has.
    let mut selected = raw.clone();
    let job = selected
        .get_mut("jobs")
        .and_then(|jobs| jobs.get_mut(id))
        .and_then(serde_yaml::Value::as_mapping_mut)
        .context("Job does not exist")?;
    job.insert("if".into(), "always()".into());
    workflow::expand(
        &workflow::parse(&serde_yaml::to_string(&selected)?)?,
        id,
        contexts,
        status,
    )
}

/// GitHub.com serves its API from its own hosts. Other servers use /api/v3 and /api/graphql.
/// Explicit GITHUB_API_URL and GITHUB_GRAPHQL_URL values win.
fn api_endpoints(server: &str, env: &BTreeMap<String, String>) -> (String, String) {
    let (api, graphql) = if server == "https://github.com" {
        (
            "https://api.github.com".to_owned(),
            "https://api.github.com/graphql".to_owned(),
        )
    } else {
        (format!("{server}/api/v3"), format!("{server}/api/graphql"))
    };
    (
        env.get("GITHUB_API_URL").cloned().unwrap_or(api),
        env.get("GITHUB_GRAPHQL_URL").cloned().unwrap_or(graphql),
    )
}

/// The `ref_name` context drops only the `refs/<namespace>/` prefix, so `refs/pull/42/merge`
/// becomes `42/merge`, as on GitHub.
fn ref_name(reference: &str) -> &str {
    reference
        .strip_prefix("refs/")
        .and_then(|rest| rest.split_once('/'))
        .map_or(reference, |(_, name)| name)
}

/// Dispatch inputs arrive as text. The `inputs` context uses the declared types, as on GitHub,
/// so `if: inputs.publish` sees a boolean. `github.event.inputs` keeps the text.
struct DispatchInputs { typed: serde_json::Value, event: serde_json::Value }
fn typed_dispatch_inputs(raw: &serde_yaml::Value, inputs: serde_json::Value) -> Result<DispatchInputs> {
    use crate::workflow::dispatch_inputs::{self, InputKind};
    let declared = dispatch_inputs::parse(raw.get("on").and_then(|on| on.get("workflow_dispatch")).unwrap_or(&serde_yaml::Value::Null))?;
    let submitted = inputs.as_object().context("Dispatch inputs must be an object")?.iter().map(|(key, value)| {
        let text = match value {
            serde_json::Value::String(value) => value.clone(),
            serde_json::Value::Bool(_) | serde_json::Value::Number(_) => value.to_string(),
            _ => bail!("Dispatch input must be a scalar"),
        };
        Ok((key.clone(), text))
    }).collect::<Result<BTreeMap<_, _>>>()?;
    let values = dispatch_inputs::validate_inputs(&declared, &submitted).map_err(anyhow::Error::msg)?;
    let mut inputs = serde_json::Map::new();
    for input in declared {
        let value = &values[&input.name];
        let value = match input.kind {
            InputKind::Boolean => serde_json::Value::Bool(value == "true"),
            InputKind::Number => serde_json::json!(value.parse::<f64>()?),
            _ => serde_json::Value::String(value.clone()),
        };
        inputs.insert(input.name, value);
    }
    Ok(DispatchInputs { typed: serde_json::Value::Object(inputs), event: serde_json::to_value(values)? })
}

/// Bounds an action's preparation (fetching, runtime probing, image builds) by the step deadline.
async fn until_deadline<T>(
    deadline: Instant,
    operation: impl std::future::Future<Output = Result<T>>,
) -> Result<T> {
    tokio::time::timeout_at(deadline.into(), operation)
        .await
        .context("The operation has timed out.")?
}

fn cache_directory() -> PathBuf {
    std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(std::env::var_os("HOME").unwrap_or_else(|| ".".into())).join(".cache")
        })
        .join("codeyard-actions")
}

async fn git_value(directory: &Path, args: &[&str]) -> String {
    match process::output(
        Command::new("git").arg("-C").arg(directory).args(args),
        Duration::from_secs(30),
    )
    .await
    {
        Ok(output) if output.status.success() => {
            String::from_utf8_lossy(&output.stdout).trim_end_matches(['\r', '\n']).into()
        }
        _ => String::new(),
    }
}

fn environment_file(path: Option<&Path>) -> Result<BTreeMap<String, String>> {
    let Some(path) = path else {
        return Ok(BTreeMap::new());
    };
    let file = std::fs::File::open(path).context("Unable to read environment file")?;
    dotenvy::from_read_iter(file)
        .map(|pair| {
            pair.map_err(|_| {
                anyhow::anyhow!("Invalid environment file: expected dotenv KEY=value lines")
            })
        })
        .collect()
}

fn matrix_metadata(matrix: &Value, masker: &Masker) -> Option<Matrix> {
    fn safe(value: &serde_json::Value, masker: &Masker) -> bool {
        match value {
            serde_json::Value::Object(values) => values.iter().all(|(key, value)| {
                !key.contains('\0') && !masker.contains(key) && safe(value, masker)
            }),
            serde_json::Value::Array(values) => values.iter().all(|value| safe(value, masker)),
            serde_json::Value::String(value) => !value.contains('\0') && !masker.contains(value),
            value => !masker.contains(&value.to_string()),
        }
    }
    let values = matrix.to_json();
    (safe(&values, masker) && serde_json::to_vec(&values).is_ok_and(|bytes| bytes.len() <= 8192))
        .then_some(Matrix { values })
}

#[derive(Clone, Default, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct JobTokens {
    tokens: BTreeMap<String, String>,
    completion_secret: String,
    #[serde(skip)]
    output: Option<output::Output>,
    #[serde(skip)]
    nested: bool,
    #[serde(skip)]
    completions: Arc<std::sync::Mutex<BTreeMap<String, usize>>>,
    #[serde(default)]
    oidc_url: String,
    #[serde(default)]
    oidc_jobs: std::collections::BTreeSet<String>,
}

impl JobTokens {
    fn progress(&self, id: &str, variant: usize) -> Option<Progress> {
        (!self.nested && !self.completion_secret.is_empty()).then(|| Progress {
            nonce: Arc::from(self.completion_secret.as_str()),
            output: self.output.clone(),
            job: id.to_owned(),
            variant: variant.try_into().expect("matrix index is bounded"),
            step: Arc::new(std::sync::atomic::AtomicU32::new(0)),
            active: Default::default(),
        })
    }
    fn reserve_calls(&self, prefix: &str, count: usize) {
        let mut completions = self.completions.lock().unwrap_or_else(|error| error.into_inner());
        for id in self.tokens.keys().filter(|id| id.starts_with(prefix)) {
            *completions.entry(id.clone()).or_insert(1) += count.saturating_sub(1);
        }
    }
    fn completed(&self, id: &str) {
        if !self.completion_secret.is_empty() {
            let mut completions = self.completions.lock().unwrap_or_else(|error| error.into_inner());
            let remaining = completions.entry(id.to_owned()).or_insert(1);
            if *remaining == 0 { return; }
            *remaining -= 1;
            if *remaining == 0 {
                if let Some(output) = &self.output {
                    output.line(format!("::codeyard-job-completed::{}:{id}", self.completion_secret), true);
                }
            }
        }
    }
}

pub const CANCELLED_EXIT_CODE: i32 = 130;
pub const CLEANUP_GRACE: Duration = Duration::from_secs(60);
/// Includes the shared cleanup grace, container removal, and final process reaping.
pub const SHUTDOWN_GRACE: Duration = Duration::from_secs(95);

#[derive(Debug, thiserror::Error)]
#[error("The workflow was cancelled")]
pub struct Cancelled;

/// Run the selected workflow, returning whether all jobs succeeded; cancellation is typed.
pub async fn run(mut options: Options) -> Result<bool> {
    let secrets = environment_file(options.secret_file.as_deref())?;
    let masker = Masker::default();
    for secret in secrets.values() {
        masker.add(secret);
    }
    let environment_values = environment_values::EnvironmentValues::load(
        options.environment_values_file.as_deref(),
        &masker,
    )?;
    let tokens: JobTokens = match &options.job_token_file {
        Some(path) => {
            serde_json::from_slice(&std::fs::read(path).context("Unable to read job token file")?)?
        }
        None => JobTokens::default(),
    };
    for token in tokens.tokens.values() {
        masker.add(token);
    }
    masker.add(&tokens.completion_secret);
    let result = run_workflow(
        &mut options,
        secrets,
        tokens,
        masker.clone(),
        environment_values,
    )
    .await;
    result.map_err(|error| if error.is::<Cancelled>() { error }
        else { anyhow::anyhow!(masker.mask(&format!("{error:#}"))) })
}

async fn run_workflow(
    options: &mut Options,
    secrets: BTreeMap<String, String>,
    tokens: JobTokens,
    masker: Masker,
    environment_values: environment_values::EnvironmentValues,
) -> Result<bool> {
    if options.container_daemon_socket != "-" {
        bail!(
            "Mounting a container engine socket is not supported; use --container-daemon-socket -"
        );
    }
    let yaml = std::fs::read_to_string(&options.workflow).context("Unable to read workflow")?;
    let raw: serde_yaml::Value = serde_yaml::from_str(&yaml)?;
    let workflow = workflow::parse(&yaml)?;
    let order = workflow::job_order(&workflow)?;
    let replay = if let Some(path) = &options.replay_file {
        use std::io::Read;
        let mut bytes = Vec::new();
        std::fs::File::open(path)?
            .take(16 * 1024 * 1024 + 1)
            .read_to_end(&mut bytes)?;
        anyhow::ensure!(
            bytes.len() <= 16 * 1024 * 1024,
            "replay state exceeds the native limit"
        );
        let replay: Replay =
            serde_json::from_slice(&bytes).context("Invalid private replay state")?;
        anyhow::ensure!(
            replay.version == 1
                && !replay.selected.is_empty()
                && replay
                    .selected
                    .keys()
                    .chain(replay.state.jobs.keys())
                    .chain(replay.state.variants.keys())
                    .all(|job| order.contains(job)),
            "replay does not match the workflow"
        );
        anyhow::ensure!(
            replay
                .state
                .jobs
                .values()
                .chain(
                    replay
                        .state
                        .variants
                        .values()
                        .flat_map(|variants| variants.values())
                )
                .all(|job| matches!(
                    job.result.as_str(),
                    "success" | "failure" | "cancelled" | "skipped"
                )),
            "unknown source job outcome"
        );
        Some(replay)
    } else {
        None
    };
    if !workflow::listens_to(&workflow, &options.event) {
        return Ok(true);
    }
    options.directory = options
        .directory
        .canonicalize()
        .context("Unable to open checkout directory")?;
    let mut event: serde_json::Value = if let Some(path) = &options.eventpath {
        serde_json::from_slice(&std::fs::read(path)?)?
    } else {
        serde_json::json!({})
    };
    let submitted_inputs = event.get("inputs").cloned().unwrap_or_else(|| serde_json::json!({}));
    let inputs = if options.event == "workflow_dispatch" {
        let inputs = typed_dispatch_inputs(&raw, submitted_inputs)?;
        event.as_object_mut().context("Dispatch event must be an object")?.insert("inputs".into(), inputs.event);
        inputs.typed
    } else { submitted_inputs };
    let mut env = environment_file(options.env_file.as_deref())?;
    for pair in &options.env {
        let (key, value) = pair.split_once('=').context("--env requires KEY=VALUE")?;
        env.insert(key.to_owned(), value.to_owned());
    }
    let workflow_name = raw.get("name").and_then(serde_yaml::Value::as_str)
        .filter(|name| !name.trim().is_empty()).map(str::to_owned).unwrap_or_else(|| {
            env.get("GITHUB_WORKFLOW_REF").and_then(|reference| reference.split_once('@'))
                .and_then(|(path, _)| path.splitn(3, '/').nth(2)).map(str::to_owned)
                .unwrap_or_else(|| options.workflow.strip_prefix(&options.directory).unwrap_or(&options.workflow).to_string_lossy().replace('\\', "/"))
        });
    if options.debug {
        for (key, value) in [
            ("ACTIONS_RUNNER_DEBUG", "true"),
            ("ACTIONS_STEP_DEBUG", "true"),
            ("RUNNER_DEBUG", "1"),
        ] {
            env.insert(key.into(), value.into());
        }
    }
    for name in ["ACTIONS_RUNTIME_TOKEN", "GITHUB_TOKEN"] {
        if let Some(value) = env.get(name) {
            masker.add(value);
        }
    }
    let sha = match env.get("GITHUB_SHA") {
        Some(sha) => sha.clone(),
        None => git_value(&options.directory, &["rev-parse", "HEAD"]).await,
    };
    let reference = match env.get("GITHUB_REF") {
        Some(reference) => reference.clone(),
        None => git_value(&options.directory, &["symbolic-ref", "-q", "HEAD"]).await,
    };
    let repository = env
        .get("GITHUB_REPOSITORY")
        .cloned()
        .or_else(|| {
            event
                .pointer("/repository/full_name")
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned)
        })
        .unwrap_or_default();
    let server = env
        .get("GITHUB_SERVER_URL")
        .cloned()
        .unwrap_or_else(|| "https://github.com".into());
    let (api_url, graphql_url) = api_endpoints(&server, &env);
    let run_id = env
        .get("GITHUB_RUN_ID")
        .cloned()
        .or_else(|| std::env::var("GITSERV_RUN_ID").ok())
        .unwrap_or_else(|| "1".into());
    let number = env
        .get("GITHUB_RUN_NUMBER")
        .cloned()
        .unwrap_or_else(|| "1".into());
    let github = serde_json::json!({
        "event":event, "event_name":options.event, "actor":options.actor, "triggering_actor":env.get("GITHUB_TRIGGERING_ACTOR").unwrap_or(&options.actor), "actor_id":env.get("GITHUB_ACTOR_ID"),
        "repository":repository, "repository_owner":repository.split('/').next().unwrap_or(""),
        "ref":reference,"ref_name":ref_name(&reference),
        "ref_type":if reference.starts_with("refs/tags/") {"tag"} else {"branch"},
        "sha":sha,"head_ref":event.pointer("/pull_request/head/ref").and_then(serde_json::Value::as_str).unwrap_or(""),"base_ref":event.pointer("/pull_request/base/ref").and_then(serde_json::Value::as_str).unwrap_or(""),"workspace":options.directory,"run_id":run_id,"run_number":number,"run_attempt":env.get("GITHUB_RUN_ATTEMPT").cloned().unwrap_or_else(||"1".into()),
        "server_url":server,"api_url":api_url,"graphql_url":graphql_url,
        "workflow":workflow_name,"workflow_ref":env.get("GITHUB_WORKFLOW_REF"),"workflow_sha":env.get("GITHUB_WORKFLOW_SHA"),"job":"", "action":"", "token":secrets.get("GITHUB_TOKEN").cloned().unwrap_or_default()
    });
    let mut contexts = Contexts::default();
    contexts
        .values
        .insert("github".into(), Value::from_json(&github));
    contexts.values.insert("secrets".into(), object(&secrets));
    let variables = environment_file(options.var_file.as_deref())?;
    contexts.values.insert("vars".into(), object(&variables));
    contexts
        .values
        .insert("inputs".into(), Value::from_json(&inputs));
    for name in ["steps", "needs", "matrix", "strategy", "job", "runner"] {
        contexts
            .values
            .insert(name.into(), Value::Object(BTreeMap::new()));
    }
    contexts.values.insert("env".into(), object(&env));
    let options = Arc::new(options.clone());
    let (cancel_tx, cancel) = watch::channel(false);
    let (output, output_task) = output::Output::start(cancel_tx.clone())?;
    let mut tokens = tokens;
    tokens.output = Some(output.clone());
    let signal = tokio::spawn(async move {
        #[cfg(unix)]
        {
            if let Ok(mut term) =
                tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            {
                tokio::select! { _ = tokio::signal::ctrl_c() => {}, _ = term.recv() => {} }
            } else {
                let _ = tokio::signal::ctrl_c().await;
            }
        }
        #[cfg(not(unix))]
        let _ = tokio::signal::ctrl_c().await;
        let _ = cancel_tx.send(true);
    });
    let files = reusable::files(&raw, &options.directory)?;
    let workspace = if options.concurrent_jobs > 1 {
        let source = options.directory.clone();
        Some(Arc::new(tokio::task::spawn_blocking(move || crate::fs::snapshot_tree(&source, crate::fs::SnapshotLinks::Preserve)).await??))
    } else { None };
    let permits = Arc::new(tokio::sync::Semaphore::new(options.concurrent_jobs.max(1) as usize));
    let execution = WorkflowRun {
        options,
        permits,
        workspace,
        tokens: Arc::new(tokens),
        files: Arc::new(files),
        masker,
        cancel,
        environment_values: Arc::new(environment_values),
        replay: replay.map(Arc::new),
    };
    let result = execution
        .execute(raw, contexts, env, String::new(), String::new())
        .await;
    signal.abort();
    let flushed = output.flush().await;
    output_task.abort();
    flushed?;
    let result = result?;
    if result.result == "cancelled" { return Err(Cancelled.into()); }
    Ok(result.result == "success")
}

#[derive(Clone)]
struct WorkflowRun {
    options: Arc<Options>,
    permits: Arc<tokio::sync::Semaphore>,
    workspace: Option<Arc<tempfile::TempDir>>,
    tokens: Arc<JobTokens>,
    files: Arc<BTreeMap<String, serde_yaml::Value>>,
    masker: Masker,
    cancel: watch::Receiver<bool>,
    environment_values: Arc<environment_values::EnvironmentValues>,
    replay: Option<Arc<Replay>>,
}

impl WorkflowRun {
    async fn run_leaf(
        &self, job: PlannedJob, index: usize, options: Arc<Options>, mut contexts: Contexts,
        env: BTreeMap<String, String>, log: Log, mut cancel: watch::Receiver<bool>,
    ) -> JobResult {
        let result = async {
            let _permit = tokio::select! {
                biased;
                _ = cancel.wait_for(|cancelled| *cancelled) => return Err(anyhow::Error::from(Cancelled)),
                permit = self.permits.clone().acquire_owned() => permit.context("Job concurrency permits closed")?,
            };
            let workspace = if let Some(source) = &self.workspace {
                let source = source.clone();
                Some(tokio::task::spawn_blocking(move || crate::fs::snapshot_tree(source.path(), crate::fs::SnapshotLinks::Preserve)).await??)
            } else { None };
            let options = if let Some(workspace) = &workspace {
                let mut options = (*options).clone();
                options.directory = workspace.path().to_owned();
                if let Some(Value::Object(github)) = contexts.values.get_mut("github") {
                    github.insert("workspace".into(), Value::String(options.directory.display().to_string()));
                }
                Arc::new(options)
            } else { options };
            Ok::<_, anyhow::Error>(run_job(job, index, options, contexts, env, log.clone(), cancel.clone()).await)
        }.await;
        match result {
            Ok(result) => result,
            Err(error) => {
                let cancelled = error.is::<Cancelled>();
                log.emit(Update::Started);
                log.line(&format!("Job preparation failed: {error:#}"));
                log.emit(Update::Finished { conclusion: if cancelled { Conclusion::Cancelled } else { Conclusion::Failure }, duration_ms: 0 });
                JobResult { result: if cancelled { "cancelled" } else { "failure" }.into(), outputs: BTreeMap::new() }
            }
        }
    }

    fn execute(
        &self,
        raw: serde_yaml::Value,
        mut contexts: Contexts,
        mut env: BTreeMap<String, String>,
        name_prefix: String,
        token_prefix: String,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<JobResult>> + Send + '_>> {
        Box::pin(async move {
            let Self {
                options,
                tokens,
                masker,
                cancel,
                ..
            } = self;
            let workflow = workflow::parse(&serde_yaml::to_string(&raw)?)?;
            let workflow_name = contexts
                .values
                .get("github")
                .and_then(|value| match value {
                    Value::Object(values) => values.get("workflow"),
                    _ => None,
                })
                .map(Value::to_text)
                .unwrap_or_else(|| "workflow".into());
            let replay = if token_prefix.is_empty() {
                self.replay.as_deref()
            } else {
                None
            };
            let environment_values = &self.environment_values;
            let mut saved = JobState::default();
            if !tokens.completion_secret.is_empty() {
                anyhow::ensure!(
                    workflow_name.len() <= 1024,
                    "workflow name exceeds the progress metadata limit"
                );
            }
            let mut results = BTreeMap::<String, JobResult>::new();
            let mut ancestors = BTreeMap::<String, Status>::new();
            let mut successful = true;
            for id in workflow::job_order(&workflow)? {
                anyhow::ensure!(!options.containment_lost.load(std::sync::atomic::Ordering::Acquire),
                    "Container cleanup failed; refusing further execution");
                if *cancel.borrow() {
                    successful = false;
                    break;
                }
                let token_id = format!("{token_prefix}{id}");
                if options.job_token_file.is_some()
                    && replay.is_none_or(|replay| replay.selected.contains_key(&id))
                {
                    let token = tokens
                        .tokens
                        .get(&token_id)
                        .context("Missing job credential")?;
                    if let Some(Value::Object(github)) = contexts.values.get_mut("github") {
                        github.insert("token".into(), Value::String(token.clone()));
                    }
                    if let Some(Value::Object(secrets)) = contexts.values.get_mut("secrets") {
                        secrets.insert("GITHUB_TOKEN".into(), Value::String(token.clone()));
                    }
                    env.insert("GITHUB_TOKEN".into(), token.clone());
                    for name in [
                        "ACTIONS_ID_TOKEN_REQUEST_URL",
                        "ACTIONS_ID_TOKEN_REQUEST_TOKEN",
                    ] {
                        env.remove(name);
                    }
                    if tokens.oidc_jobs.contains(&token_id) {
                        env.insert(
                            "ACTIONS_ID_TOKEN_REQUEST_URL".into(),
                            tokens.oidc_url.clone(),
                        );
                        env.insert("ACTIONS_ID_TOKEN_REQUEST_TOKEN".into(), token.clone());
                    }
                }
                // expand checks the job's own dependencies; unrelated failures must not skip it.
                let needs_ids = raw
                    .get("jobs")
                    .and_then(|jobs| jobs.get(&id))
                    .and_then(|job| job.get("needs"));
                let needs_ids: Vec<String> = match needs_ids {
                    Some(serde_yaml::Value::Sequence(names)) => names.iter().map(|value| workflow::scalar_text(value, "needs")).collect::<Result<_>>()?,
                    Some(value) => vec![workflow::scalar_text(value, "needs")?],
                    None => Vec::new(),
                };
                let mut needs = BTreeMap::new();
                let mut status = Status::default();
                let mut dependency_success = true;
                let mut direct_status = Status::default();
                for name in needs_ids {
                    if let Some((canonical, result)) = results
                        .iter()
                        .find(|(key, _)| key.eq_ignore_ascii_case(&name))
                    {
                        status.failed |= result.result == "failure";
                        status.cancelled |= result.result == "cancelled";
                        dependency_success &= result.result == "success";
                        direct_status.failed |= result.result == "failure";
                        direct_status.cancelled |= result.result == "cancelled";
                        if let Some(ancestor) = ancestors.get(canonical) {
                            status.failed |= ancestor.failed;
                            status.cancelled |= ancestor.cancelled;
                        }
                        needs.insert(
                    name,
                    Value::from_json(
                        &serde_json::json!({"result":result.result,"outputs":result.outputs}),
                    ),
                );
                    }
                }
                contexts.values.insert("needs".into(), Value::Object(needs));
                ancestors.insert(id.clone(), status);
                if let Some(replay) = &replay {
                    if !replay.selected.contains_key(&id) {
                        let result = replay
                            .state
                            .jobs
                            .get(&id)
                            .context("Source dependency state is unavailable")?
                            .clone();
                        if let Some(expected) = replay.matrices.get(&id) {
                            let plans = workflow::expand(&workflow, &id, &contexts, status)?
                                .context("Retained matrix context has changed; re-run the whole workflow")?;
                            validate_replay_matrix(expected, &plans)?;
                        }
                        successful &= matches!(result.result.as_str(), "success" | "skipped");
                        results.insert(id, result);
                        continue;
                    }
                }
                let expanded = if !dependency_success
                    && !direct_status.failed
                    && !direct_status.cancelled
                    || status != direct_status
                {
                    expand_with_dependency_status(&raw, &id, &contexts, status, dependency_success)
                } else {
                    workflow::expand(&workflow, &id, &contexts, status)
                };
                let expanded = match expanded {
                    Ok(expanded) => expanded,
                    Err(error) => {
                        Log { prefix: format!("{name_prefix}{id}"), masker: masker.clone(), progress: None, output: tokens.output.clone() }
                            .line(&format!("Job expansion failed: {error:#}"));
                        let result = JobResult { result: "failure".into(), outputs: BTreeMap::new() };
                        saved.jobs.insert(id.clone(), result.clone());
                        save_job_state(options, &mut saved, masker)?;
                        results.insert(id, result);
                        successful = false;
                        tokens.completed(&token_id);
                        continue;
                    }
                };
                // A matrix whose excludes remove every combination runs nothing, like a skipped job.
                let Some(mut plans) = expanded.filter(|plans| !plans.is_empty()) else {
                    if let Some(progress) = tokens.progress(&id, 0) {
                        let name = raw
                            .get("jobs")
                            .and_then(|jobs| jobs.get(&id))
                            .and_then(|job| job.get("name"))
                            .and_then(serde_yaml::Value::as_str)
                            .filter(|name| !name.contains("${{"))
                            .unwrap_or(&id);
                        anyhow::ensure!(
                            id.len() <= 128 && name.len() <= 1024,
                            "job metadata exceeds the progress limit"
                        );
                        progress.emit(Update::Queued {
                            workflow_name: commands::bounded(&masker.mask(&workflow_name), 1024),
                            name: commands::bounded(&masker.mask(name), 1024),
                            labels: vec![],
                            matrix: None,
                        });
                        progress.emit(Update::Started);
                        progress.emit(Update::Finished {
                            conclusion: Conclusion::Skipped,
                            duration_ms: 0,
                        });
                    }
                    let result = JobResult {
                        result: "skipped".into(),
                        outputs: BTreeMap::new(),
                    };
                    results.insert(id.clone(), result.clone());
                    saved.jobs.insert(id.clone(), result);
                    save_job_state(options, &mut saved, masker)?;
                    if token_prefix.is_empty() {
                        tokens.completed(&token_id);
                    }
                    continue;
                };
                let variants = replay
                    .as_ref()
                    .and_then(|replay| replay.selected.get(&id))
                    .and_then(Option::as_ref);
                anyhow::ensure!(
                    variants.is_none_or(|variants| !variants.is_empty()
                        && variants.iter().all(|index| (*index as usize) < plans.len())),
                    "selected matrix variant is unavailable"
                );
                if variants.is_some() {
                    if let Some(expected) = replay.as_ref().and_then(|source| source.matrices.get(&id)) {
                        validate_replay_matrix(expected, &plans)?;
                    } else {
                        anyhow::ensure!(raw["jobs"][id.as_str()]["strategy"].get("matrix").is_none(),
                            "Source matrix metadata is unavailable");
                    }
                }
                for job in &mut plans {
                    job.name = masker.mask(&job.name);
                    if job.name.chars().count() > 100 {
                        job.name = job.name.chars().take(97).collect::<String>() + "...";
                    }
                }
                for (index, job) in plans.iter().enumerate() {
                    if variants.is_some_and(|variants| !variants.contains(&(index as u32))) {
                        continue;
                    }
                    if let Some(progress) = tokens.progress(&id, index) {
                        anyhow::ensure!(
                            id.len() <= 128 && job.name.len() <= 1024,
                            "job metadata exceeds the progress limit"
                        );
                        progress.emit(Update::Queued {
                            workflow_name: commands::bounded(&masker.mask(&workflow_name), 1024),
                            name: job.name.clone(),
                            labels: job.runs_on.iter().map(|label| commands::bounded(&masker.mask(label), 1024)).take(64).collect(),
                            matrix: matrix_metadata(&job.matrix, masker),
                        });
                    }
                }
                let limit = plans
                    .first()
                    .and_then(|job| job.max_parallel)
                    .unwrap_or(options.concurrent_jobs)
                    .min(options.concurrent_jobs)
                    .max(1) as usize;
                let fail_fast = plans.first().is_none_or(|job| job.fail_fast);
                let called = plans.first().is_some_and(|job| job.call.is_some());
                let total = plans.len();
                if called {
                    tokens.reserve_calls(&format!("{token_id}/"), variants.map_or(total, |variants| variants.len()));
                }
                let mut plans = plans.into_iter().enumerate().filter(|(index, _)| {
                    variants.is_none_or(|variants| variants.contains(&(*index as u32)))
                });
                let mut tasks = tokio::task::JoinSet::new();
                let (matrix_tx, matrix_cancel) = watch::channel(false);
                let relay_tx = matrix_tx.clone();
                let mut global_cancel = cancel.clone();
                let relay = tokio::spawn(async move {
                    while global_cancel.changed().await.is_ok() {
                        if *global_cancel.borrow() {
                            let _ = relay_tx.send(true);
                            break;
                        }
                    }
                });
                let mut aggregate = JobResult {
                    result: "success".into(),
                    outputs: BTreeMap::new(),
                };
                if let Some(selected) = variants {
                    let source = &replay
                        .as_ref()
                        .expect("a variant selection requires a replay")
                        .state;
                    aggregate.outputs = source
                        .retained_outputs(&id, selected, total, called)
                        .map_err(anyhow::Error::msg)?;
                    for index in (0..total as u32).filter(|index| !selected.contains(index)) {
                        let result = source
                            .variants
                            .get(&id)
                            .and_then(|variants| variants.get(&index))
                            .context("Source matrix state is unavailable")?;
                        if result.result == "failure" {
                            aggregate.result = "failure".into();
                        } else if result.result == "cancelled" && aggregate.result == "success" {
                            aggregate.result = "cancelled".into();
                        }
                    }
                }
                loop {
                    while tasks.len() < limit && !*matrix_cancel.borrow() {
                        let Some((index, job)) = plans.next() else {
                            break;
                        };
                        let options = options.clone();
                        let mut contexts = contexts.clone();
                        // Bindings are keyed by run-wide job identity, so a called workflow's job
                        // never takes a root job's bindings.
                        environment_values.apply(&token_id, &mut contexts);
                        contexts.values.insert(
                            "strategy".into(),
                            Value::from_json(&serde_json::json!({"job-index":index,"job-total":total,"fail-fast":job.fail_fast,"max-parallel":job.max_parallel.map_or(total as f64, f64::from)})),
                        );
                        contexts.values.insert("matrix".into(), job.matrix.clone());
                        let env = env.clone();
                        let job_masker = masker.fork();
                        let name = format!("{name_prefix}{}", job.name);
                        let log = Log {
                            prefix: format!("{workflow_name}/{name}"),
                            masker: job_masker.clone(),
                            progress: tokens.progress(&id, index),
                            output: tokens.output.clone(),
                        };
                        let cancel = matrix_cancel.clone();
                        let mut execution = self.clone();
                        execution.masker = job_masker;
                        execution.cancel = cancel.clone();
                        let mut nested_options = (*options).clone();
                        nested_options.job_state_file = None;
                        execution.options = Arc::new(nested_options);
                        let mut nested_tokens = (**tokens).clone();
                        nested_tokens.nested = true;
                        execution.tokens = Arc::new(nested_tokens);
                        let called_prefix = format!("{token_id}/");
                        tasks.spawn(async move {
                            let Some(cancel) = admit(&job, index, &options, &log, cancel).await
                            else {
                                log.emit(Update::Started);
                                log.line("Job cancelled: a newer job took its concurrency group");
                                log.emit(Update::Finished {
                                    conclusion: Conclusion::Cancelled,
                                    duration_ms: 0,
                                });
                                let result = JobResult {
                                    result: "cancelled".into(),
                                    outputs: BTreeMap::new(),
                                };
                                return (index, result);
                            };
                            execution.cancel = cancel.clone();
                            let result = if let Some(call) = &job.call {
                                log.emit(Update::Started);
                                let started = Instant::now();
                                let result = async {
                                    let reference = call
                                        .get("uses")
                                        .and_then(serde_yaml::Value::as_str)
                                        .context("Missing called workflow")?;
                                    let raw = execution
                                        .files
                                        .get(reference)
                                        .context("Called workflow was not resolved")?
                                        .clone();
                                    let contexts = reusable::contexts(
                                        call,
                                        &raw,
                                        &contexts,
                                        &execution.masker,
                                    )?;
                                    execution
                                        .execute(
                                            raw,
                                            contexts,
                                            env,
                                            format!("{name} / "),
                                            called_prefix,
                                        )
                                        .await
                                }
                                .await;
                                let result = match result {
                                    Ok(result) => result,
                                    Err(error) => {
                                        log.line(&format!("Error: {error:#}"));
                                        JobResult {
                                            result: "failure".into(),
                                            outputs: BTreeMap::new(),
                                        }
                                    }
                                };
                                log.emit(Update::Finished {
                                    conclusion: Conclusion::from_result(&result.result)
                                        .expect("known workflow result"),
                                    duration_ms: started
                                        .elapsed()
                                        .as_millis()
                                        .try_into()
                                        .unwrap_or(u64::MAX),
                                });
                                result
                            } else {
                                execution.run_leaf(job, index, options, contexts, env, log, cancel).await
                            };
                            (index, result)
                        });
                    }
                    let Some(result) = tasks.join_next().await else {
                        break;
                    };
                    let (index, result) = result.context("Job task failed")?;
                    saved
                        .variants
                        .entry(id.clone())
                        .or_default()
                        .insert(index.try_into().expect("matrix is bounded"), result.clone());
                    saved
                        .variant_order
                        .entry(id.clone())
                        .or_default()
                        .push(index.try_into().expect("matrix is bounded"));
                    save_job_state(options, &mut saved, masker)?;
                    if result.result == "failure" {
                        aggregate.result = "failure".into();
                        if fail_fast {
                            let _ = matrix_tx.send(true);
                        }
                    } else if result.result == "cancelled" && aggregate.result == "success" {
                        aggregate.result = "cancelled".into();
                    }
                    if !called {
                        aggregate.outputs.extend(result.outputs);
                    } else if result.result == "success" {
                        merge_outputs(&mut aggregate.outputs, result.outputs);
                    }
                }
                relay.abort();
                for (index, _) in plans {
                    if let Some(progress) = tokens.progress(&id, index) {
                        progress.emit(Update::Finished {
                            conclusion: Conclusion::Cancelled,
                            duration_ms: 0,
                        });
                    }
                    let index = index.try_into().expect("matrix is bounded");
                    saved.variants.entry(id.clone()).or_default().insert(
                        index,
                        JobResult {
                            result: "cancelled".into(),
                            outputs: BTreeMap::new(),
                        },
                    );
                    saved
                        .variant_order
                        .entry(id.clone())
                        .or_default()
                        .push(index);
                    if aggregate.result == "success" {
                        aggregate.result = "cancelled".into();
                    }
                }
                successful &= aggregate.result == "success" || aggregate.result == "skipped";
                saved.jobs.insert(id.clone(), aggregate.clone());
                save_job_state(options, &mut saved, masker)?;
                tokens.completed(&token_id);
                results.insert(id, aggregate);
            }
            contexts.values.insert("jobs".into(), Value::Object(results.iter().map(|(id, result)| (
        id.clone(), Value::from_json(&serde_json::json!({"result": result.result, "outputs": result.outputs}))
    )).collect()));
            Ok(JobResult {
                result: if *cancel.borrow() || results.values().any(|result| result.result == "cancelled") {
                    "cancelled"
                } else if successful {
                    "success"
                } else {
                    "failure"
                }
                .into(),
                outputs: if token_prefix.is_empty() { BTreeMap::new() } else { reusable::outputs(&raw, &contexts, masker)? },
            })
        })
    }
}

const ADMISSION_POLL: Duration = Duration::from_millis(500);

/// The worker's answer to a job's concurrency request, once it has answered.
fn admission(path: &Path) -> Option<bool> {
    match std::fs::read_to_string(path).ok()?.as_str() {
        "granted" => Some(true),
        "cancelled" => Some(false),
        _ => None,
    }
}

/// Holds a job with a `concurrency` group until the worker grants the group. The returned
/// signal also fires when a newer job in the group cancels this one. `None` means the job
/// was replaced or cancelled before it started. Without a worker there is nobody to share
/// a group with, so the job starts at once.
async fn admit(
    job: &PlannedJob,
    index: usize,
    options: &Options,
    log: &Log,
    mut cancel: watch::Receiver<bool>,
) -> Option<watch::Receiver<bool>> {
    let (Some(concurrency), Some(directory), Some(_)) =
        (&job.concurrency, &options.concurrency_dir, &log.progress)
    else {
        return Some(cancel);
    };
    let digest = <sha2::Sha256 as sha2::Digest>::digest(concurrency.group.to_lowercase());
    let group = format!("{digest:x}");
    log.emit(Update::Concurrency {
        group,
        cancel_in_progress: concurrency.cancel_in_progress,
    });
    log.line(&format!("Waiting for concurrency group '{}'", log.masker.mask(&concurrency.group)));
    let path = directory.join(format!("{}.{index}", job.job_id));
    loop {
        match admission(&path) {
            Some(true) => break,
            Some(false) => return None,
            None => {}
        }
        tokio::select! {
            _ = cancel.wait_for(|cancelled| *cancelled) => return None,
            _ = tokio::time::sleep(ADMISSION_POLL) => {}
        }
    }
    let (signal, admitted) = watch::channel(*cancel.borrow());
    tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = signal.closed() => break,
                cancelled = async { cancel.wait_for(|cancelled| *cancelled).await.is_ok() } => {
                    if cancelled {
                        let _ = signal.send(true);
                    }
                    break;
                }
                _ = tokio::time::sleep(ADMISSION_POLL) => {
                    if admission(&path) == Some(false) {
                        let _ = signal.send(true);
                        break;
                    }
                }
            }
        }
    });
    Some(admitted)
}

fn save_job_state(options: &Options, state: &mut JobState, masker: &Masker) -> Result<()> {
    let Some(path) = &options.job_state_file else {
        return Ok(());
    };
    for result in state.jobs.values_mut().chain(
        state
            .variants
            .values_mut()
            .flat_map(|variants| variants.values_mut()),
    ) {
        result.outputs.retain(|_, value| !masker.contains(value));
    }
    let mut file = tempfile::NamedTempFile::new_in(
        path.parent()
            .context("job state needs a parent directory")?,
    )?;
    struct Bounded<'a> { file: &'a mut std::fs::File, remaining: usize }
    impl std::io::Write for Bounded<'_> {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            if bytes.len() > self.remaining { return Err(std::io::ErrorKind::FileTooLarge.into()); }
            let written = std::io::Write::write(self.file, bytes)?;
            self.remaining -= written;
            Ok(written)
        }
        fn flush(&mut self) -> std::io::Result<()> { std::io::Write::flush(self.file) }
    }
    let written = serde_json::to_writer(Bounded { file: file.as_file_mut(), remaining: crate::progress::MAX_STATE_BYTES }, state);
    if written.as_ref().err().is_some_and(|error| error.io_error_kind() == Some(std::io::ErrorKind::FileTooLarge)) {
        match std::fs::remove_file(path) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        eprintln!("::warning::Private replay state exceeds its budget; replay snapshot omitted");
        return Ok(());
    }
    written?;
    file.persist(path)
        .context("Unable to save private job replay state")?;
    Ok(())
}

struct Job {
    plan: PlannedJob,
    options: Arc<Options>,
    contexts: Contexts,
    env: BTreeMap<String, String>,
    saved_env: BTreeMap<String, String>,
    paths: Vec<String>,
    log: Log,
    cancel: watch::Receiver<bool>,
    deadline: Instant,
    status: Status,
    temp: tempfile::TempDir,
    cache: PathBuf,
    podman: Podman,
    explicit_container: bool,
    default_shell: Option<String>,
    runtime_path: String,
    default_working_directory: Option<String>,
    sequence: usize,
    anonymous_action: usize,
    command_stopped: Option<String>,
    public_step: u32,
    next_step: usize,
    active_files: PathBuf,
    posts: Vec<actions::Post>,
    matchers: matchers::Matchers,
}

async fn run_job(
    plan: PlannedJob,
    index: usize,
    options: Arc<Options>,
    contexts: Contexts,
    env: BTreeMap<String, String>,
    log: Log,
    cancel: watch::Receiver<bool>,
) -> JobResult {
    log.emit(Update::Started);
    let job_started = Instant::now();
    log.line("Job started");
    if options.debug {
        log.line(&format!(
            "Runner debug: job {} uses labels {:?}",
            plan.job_id, plan.runs_on
        ));
    }
    let continued = plan.continue_on_error;
    let cancelled = cancel.clone();
    let mut interrupted = None;
    let containment = options.containment_lost.clone();
    let result = async {
        let temp = crate::fs::private_tempdir("codeyard-actions-", None)?;
        let prefix = format!(
            "codeyard-{}-{index}-{}",
            std::process::id(),
            temp.path()
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
        )
        .to_ascii_lowercase();
        let podman = Podman::new(&options, prefix)?;
        let deadline = Instant::now() + minutes(plan.timeout_minutes.unwrap_or(360.0))?;
        let cache = cache_directory();
        std::fs::create_dir_all(&cache)?;
        let mut job = Job {
            plan,
            options,
            contexts,
            env,
            saved_env: BTreeMap::new(),
            paths: Vec::new(),
            log: log.clone(),
            cancel,
            deadline,
            status: Status::default(),
            temp,
            cache,
            podman,
            explicit_container: false,
            default_shell: None,
            runtime_path: std::env::var("PATH").unwrap_or_default(),
            default_working_directory: None,
            sequence: 0,
            anonymous_action: 0,
            command_stopped: None,
            public_step:1,
            next_step: 0,
            active_files: PathBuf::new(),
            posts: Vec::new(),
            matchers: matchers::Matchers::default(),
        };
        let mut cancellation = job.cancel.clone();
        let result = tokio::select! {
            result = job.execute() => result,
            _ = tokio::time::sleep_until(deadline.into()) => {interrupted=Some(Conclusion::TimedOut);Err(anyhow::anyhow!("The job has timed out."))},
            _ = cancellation.wait_for(|cancelled| *cancelled) => {interrupted=Some(Conclusion::Cancelled);Err(anyhow::anyhow!("The job was cancelled."))},
        };
        job.status.cancelled = *job.cancel.borrow();
        job.status.failed |= result.is_err() && !job.status.cancelled;
        job.sync_status();
        if result.is_err(){job.log.interrupt_step(interrupted.unwrap_or(Conclusion::Failure));}
        let mut result = result;
        job.deadline = Instant::now() + CLEANUP_GRACE;
        let (_cleanup_tx, cleanup_cancel) = watch::channel(false);
        job.cancel = cleanup_cancel;
        if interrupted.is_some() && !job.podman.cleanup_active_actions().await {
            containment.store(true, std::sync::atomic::Ordering::Release);
        }
        if interrupted.is_some() && !containment.load(std::sync::atomic::Ordering::Acquire) {
            let remaining = job.plan.steps[job.next_step..].to_vec();
            let mut outputs = BTreeMap::new();
            for step in remaining {
                if Instant::now() >= job.deadline { break; }
                job.public_step += 1;
                let deadline = job.deadline;
                let _ = tokio::time::timeout_at(deadline.into(), job.step(&step, 0, &mut outputs)).await;
            }
        }
        while let Some(post) = job.posts.pop() {
            if Instant::now() >= job.deadline || containment.load(std::sync::atomic::Ordering::Acquire) { break; }
            job.sync_status();
            job.public_step+=1;
            let number=job.public_step;
            let started=Instant::now();
            job.log.start_step(number,"Post action");
            let post=job.post(post).await;
            job.log.finish_step(number,match &post{Ok(true)=>Conclusion::Success,Ok(false)=>Conclusion::Skipped,Err(_)=>Conclusion::Failure},started);
            if let Err(error) = post {
                job.log.line(&format!("Post action failed: {error:#}"));
                job.status.failed = true;
                if let Ok(result) = &mut result { result.result = "failure".into(); }
            }
        }
        job.public_step+=1;
        let cleanup=Instant::now();
        job.log.start_step(job.public_step,"Complete job");
        let cleaned=job.podman.cleanup().await;
        if !cleaned {
            containment.store(true, std::sync::atomic::Ordering::Release);
            job.log.line("Container cleanup failed");
        }
        job.log.finish_step(job.public_step,if cleaned{Conclusion::Success}else{Conclusion::Failure},cleanup);
        anyhow::ensure!(cleaned, "Container cleanup failed");
        result
    }
    .await;
    let result = match result {
        Ok(result) => result,
        Err(error) => {
            log.line(&format!("Error: {error:#}"));
            JobResult {
                result: "failure".into(),
                outputs: BTreeMap::new(),
            }
        }
    };
    let mut result = result;
    if *cancelled.borrow() && result.result != "success" {
        result.result = "cancelled".into();
    }
    let conclusion = interrupted.unwrap_or_else(|| {
        Conclusion::from_result(&result.result).expect("runner produces a known job result")
    });
    log.emit(Update::Finished {
        conclusion,
        duration_ms: job_started
            .elapsed()
            .as_millis()
            .try_into()
            .unwrap_or(u64::MAX),
    });
    if continued && result.result == "failure" && !containment.load(std::sync::atomic::Ordering::Acquire) {
        log.line("Job failure ignored (continue-on-error)");
        result.result = "success".into();
    }
    log.line(&format!("Job completed: {}", result.result));
    result
}

impl Job {
    fn sync_status(&mut self) {
        if let Some(Value::Object(job)) = self.contexts.values.get_mut("job") {
            job.insert("status".into(), Value::String(if self.status.cancelled { "cancelled" } else if self.status.failed { "failure" } else { "success" }.into()));
        }
    }

    fn restore_event(&self) -> Result<PathBuf> {
        let path = self.temp.path().join("event.json");
        let payload = self.contexts.values.get("github").and_then(|github| match github {
            Value::Object(values) => values.get("event"),
            _ => None,
        }).map(Value::to_json).unwrap_or_else(|| serde_json::json!({}));
        crate::fs::replace_file(&path, &serde_json::to_vec(&payload)?)?;
        Ok(path)
    }

    async fn execute(&mut self) -> Result<JobResult> {
        let setup = Instant::now();
        self.log.start_step(1, "Set up job");
        let event = self.restore_event()?;
        if let Some(Value::Object(github)) = self.contexts.values.get_mut("github") {
            github.insert("job".into(), Value::String(self.plan.job_id.clone()));
            github.insert(
                "event_path".into(),
                Value::String(event.display().to_string()),
            );
        }
        self.contexts
            .values
            .insert("matrix".into(), self.plan.matrix.clone());
        if let Some(Value::Object(strategy)) = self.contexts.values.get_mut("strategy") {
            strategy.insert("fail-fast".into(), Value::Bool(self.plan.fail_fast));
            strategy.entry("max-parallel".into()).or_insert_with(|| Value::Number(self.plan.max_parallel.unwrap_or(self.options.concurrent_jobs) as f64));
        }
        self.contexts.values.insert("runner".into(), Value::from_json(&serde_json::json!({"os":runner_os(),"arch":runner_arch(),"name":"codeyard-actions","temp":self.temp.path(),"tool_cache":self.cache.join("tool_cache"),"environment":"self-hosted","debug":self.env.get("RUNNER_DEBUG").cloned().unwrap_or_default()})));
        std::fs::create_dir_all(self.cache.join("tool_cache"))?;
        for (key, template) in &self.plan.env {
            self.env
                .insert(key.clone(), text(template, &self.contexts, self.status)?);
        }
        self.contexts.values.insert("env".into(), object(&self.env));
        // An empty job container image means no job container, as on GitHub.
        let mut container = self
            .plan
            .container
            .as_ref()
            .map(|container| self.container(container))
            .transpose()?
            .filter(|container| !container.image.is_empty());
        self.explicit_container = container.is_some();
        if container.is_none() {
            // Admission lowercases labels, so the platform key is matched without case.
            let mapped = self.plan.runs_on.iter().find_map(|label| {
                self.options.platforms.iter().find_map(|mapping| {
                    mapping
                        .split_once('=')
                        .filter(|(key, _)| key.eq_ignore_ascii_case(label))
                        .map(|(_, value)| value)
                })
            });
            match mapped {
                Some(image) if image.starts_with("vm:") => bail!("VM platform mappings are not integrated with workflow execution; use the JobVm API explicitly"),
                Some(image) if image != "-self-hosted" => {
                    container = Some(Container {
                        image: image.into(),
                        ..Container::default()
                    });
                }
                Some(_) => {}
                // With platform mappings, host execution needs an explicit -self-hosted mapping.
                None if !self.options.platforms.is_empty() => bail!(
                    "No platform mapping matches runs-on: {}",
                    self.plan.runs_on.join(", ")
                ),
                None => {}
            }
        }
        let services = self
            .plan
            .services
            .iter()
            .map(|(name, container)| Ok((name.clone(), self.container(container)?)))
            .collect::<Result<BTreeMap<_, _>>>()?;
        // An empty image disables a service, as on GitHub. An expression can produce one.
        let services: BTreeMap<_, _> = services
            .into_iter()
            .filter(|(_, container)| !container.image.is_empty())
            .collect();
        let services = self
            .podman
            .setup(
                container.as_ref(),
                &services,
                &self.options.directory,
                self.temp.path(),
                &self.cache,
            )
            .await?;
        if self.podman.job.is_some() {
            self.runtime_path = self.podman.path.clone().unwrap_or_else(|| "/usr/bin:/bin".into());
        }
        self.contexts.values.insert("job".into(), Value::from_json(&serde_json::json!({"status":"success","services":services,"container":{"id":self.podman.job,"network":self.podman.network}})));
        self.default_shell = self.plan.default_shell.as_ref().map(|value| text(value, &self.contexts, self.status)).transpose()?;
        self.default_working_directory = self.plan.default_working_directory.as_ref().map(|value| text(value, &self.contexts, self.status)).transpose()?;
        self.log.finish_step(1, Conclusion::Success, setup);
        let steps = self.plan.steps.clone();
        let mut outputs = BTreeMap::new();
        for (index, step) in steps.iter().enumerate() {
            self.next_step = index + 1;
            anyhow::ensure!(!self.options.containment_lost.load(std::sync::atomic::Ordering::Acquire),
                "Container cleanup failed; refusing further execution");
            self.public_step += 1;
            let started = Instant::now();
            if let Err(error) = self.step(step, 0, &mut outputs).await {
                self.status.failed = true;
                self.log.line(&format!("Error evaluating step: {error:#}"));
                self.log.finish_step(self.public_step, Conclusion::Failure, started);
                if let Some(Value::Object(job)) = self.contexts.values.get_mut("job") {
                    job.insert("status".into(), Value::String("failure".into()));
                }
            }
        }
        let mut env = self.env.clone();
        env.extend(self.saved_env.clone());
        self.contexts.values.insert("env".into(), object(&env));
        let mut outputs = BTreeMap::new();
        for (key, template) in &self.plan.outputs {
            let value = text(template, &self.contexts, self.status)?;
            if self.log.masker.contains(&value) {
                self.log
                    .line(&format!("Skip output '{key}' since it may contain secret."));
            } else {
                outputs.insert(key.clone(), value);
            }
        }
        let result = if *self.cancel.borrow() {
            "cancelled"
        } else if self.status.failed {
            "failure"
        } else {
            "success"
        };
        Ok(JobResult {
            result: result.into(),
            outputs,
        })
    }

    fn container(&self, container: &Container) -> Result<Container> {
        let expand = |value: &str| text(value, &self.contexts, self.status);
        // A whole expression keeps its type: `ports: ${{ fromJSON('[8080]') }}` is a list, and
        // `container: ${{ fromJSON(...) }}` is the whole definition.
        let items = |value: Value| -> Vec<String> {
            match value {
                Value::Array(items) => items.iter().map(Value::to_text).collect(),
                other => vec![other.to_text()],
            }
        };
        let list = |values: &[String]| -> Result<Vec<String>> {
            let mut out = Vec::new();
            for value in values {
                out.extend(items(expressions::interpolate(
                    value,
                    &self.contexts,
                    self.status,
                )?));
            }
            Ok(out)
        };
        if let Value::Object(fields) =
            expressions::interpolate(&container.image, &self.contexts, self.status)?
        {
            let field = |name: &str| fields.get(name).cloned().unwrap_or(Value::Null);
            let image = field("image").to_text();
            return Ok(Container {
                image: image.strip_prefix("docker://").unwrap_or(&image).to_owned(),
                credentials: match field("credentials") {
                    Value::Object(credentials) => Some((
                        credentials
                            .get("username")
                            .map(Value::to_text)
                            .unwrap_or_default(),
                        credentials
                            .get("password")
                            .map(Value::to_text)
                            .unwrap_or_default(),
                    )),
                    _ => None,
                },
                env: match field("env") {
                    Value::Object(env) => env
                        .iter()
                        .map(|(key, value)| (key.clone(), value.to_text()))
                        .collect(),
                    _ => BTreeMap::new(),
                },
                ports: match field("ports") {
                    Value::Null => Vec::new(),
                    value => items(value),
                },
                volumes: match field("volumes") {
                    Value::Null => Vec::new(),
                    value => items(value),
                },
                options: match field("options") {
                    Value::Null => None,
                    value => Some(value.to_text()),
                },
            });
        }
        let image = expand(&container.image)?;
        Ok(Container {
            image: image.strip_prefix("docker://").unwrap_or(&image).to_owned(),
            credentials: container
                .credentials
                .as_ref()
                .map(|(user, password)| Ok::<_, anyhow::Error>((expand(user)?, expand(password)?)))
                .transpose()?,
            env: container
                .env
                .iter()
                .map(|(key, value)| Ok((key.clone(), expand(value)?)))
                .collect::<Result<_>>()?,
            ports: list(&container.ports)?,
            volumes: list(&container.volumes)?,
            options: container.options.as_deref().map(expand).transpose()?,
        })
    }

    fn step<'a>(
        &'a mut self,
        step: &'a Step,
        depth: usize,
        parent_outputs: &'a mut BTreeMap<String, String>,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<()>> + Send + 'a>> {
        Box::pin(async move {
            if depth > 20 {
                bail!("Composite actions nested more than 20 levels");
            }
            self.status.cancelled |= *self.cancel.borrow();
            self.matchers.reset();
            self.prepare_files()?;
            let action = match &step.id {
                Some(id) => id.clone(),
                None => {
                    self.anonymous_action += 1;
                    if self.anonymous_action == 1 { "__run".into() } else { format!("__run_{}", self.anonymous_action) }
                }
            };
            if let Some(Value::Object(github)) = self.contexts.values.get_mut("github") {
                github.insert("action".into(), Value::String(action));
            }
            let name = step
                .name
                .clone()
                .or_else(|| step.id.clone())
                .unwrap_or_else(|| match &step.action {
                    StepAction::Run { .. } => "Run script".into(),
                    StepAction::Uses { reference, .. } => reference.clone(),
                });
            let mut env = self.env.clone();
            env.extend(self.saved_env.clone());
            self.contexts.values.insert("env".into(), object(&env));
            for (key, template) in &step.env {
                env.insert(key.clone(), text(template, &self.contexts, self.status)?);
            }
            self.contexts.values.insert("env".into(), object(&env));
            let name = text(&name, &self.contexts, self.status)?;
            let name = commands::bounded(&self.log.masker.mask(&name), 1024);
            let step_started = Instant::now();
            if depth == 0 { self.log.start_step(self.public_step, &name); }
            let condition = expressions::evaluate_condition(
                step.condition.as_deref().unwrap_or("success()"),
                &self.contexts,
                self.status,
            )?;
            let mut outputs = BTreeMap::new();
            let outcome;
            let conclusion;
            if !condition {
                outcome = "skipped";
                conclusion = "skipped";
                self.log.line(&format!("Step skipped: {name}"));
            } else {
                self.log.line(&format!("Step started: {name}"));
                let deadline = if let Some(template) = &step.timeout_minutes {
                    let value = text(template, &self.contexts, self.status)?
                        .parse::<f64>()
                        .context("timeout-minutes must be a number")?;
                    self.deadline.min(Instant::now() + minutes(value)?)
                } else {
                    self.deadline
                };
                let continued = if let Some(template) = &step.continue_on_error {
                    boolean(template, &self.contexts, self.status)?
                } else { false };
                let result = self
                    .action(&step.action, env, deadline, depth, &mut outputs)
                    .await;
                let ok = match result {
                    Ok(ok) => ok,
                    Err(error) => {
                        self.log.line(&format!("Error: {error:#}"));
                        false
                    }
                };
                outcome = if *self.cancel.borrow() {
                    "cancelled"
                } else if ok {
                    "success"
                } else {
                    "failure"
                };
                conclusion = if outcome == "failure" && continued {
                    "success"
                } else {
                    outcome
                };
                if conclusion == "failure" {
                    self.status.failed = true;
                }
                self.log.line(&format!(
                    "Step completed: {name} ({conclusion}; outcome: {outcome})"
                ));
            }
            if depth == 0 {
                self.log.finish_step(
                    self.public_step,
                    Conclusion::from_result(conclusion).expect("known step conclusion"),
                    step_started,
                );
            }
            if let Some(id) = &step.id {
                if let Some(Value::Object(steps)) = self.contexts.values.get_mut("steps") {
                    steps.insert(id.clone(), Value::from_json(&serde_json::json!({"outputs":outputs,"outcome":outcome,"conclusion":conclusion})));
                }
            }
            parent_outputs.extend(outputs);
            self.sync_status();
            Ok(())
        })
    }

    fn action<'a>(
        &'a mut self,
        action: &'a StepAction,
        env: BTreeMap<String, String>,
        deadline: Instant,
        depth: usize,
        outputs: &'a mut BTreeMap<String, String>,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<bool>> + Send + 'a>> {
        Box::pin(async move {
            match action {
                StepAction::Run {
                    script,
                    shell,
                    working_directory,
                } => {
                    let script = text(script, &self.contexts, self.status)?;
                    let shell = match shell {
                        Some(value) => Some(text(value, &self.contexts, self.status)?),
                        None => self.default_shell.clone(),
                    };
                    let directory = match working_directory {
                        Some(value) => text(value, &self.contexts, self.status)?,
                        None => self.default_working_directory.clone().unwrap_or_default(),
                    };
                    let directory = self.options.directory.join(directory);
                    self.script(
                        &script,
                        shell.as_deref(),
                        &directory,
                        env,
                        deadline,
                        outputs,
                    )
                    .await
                }
                StepAction::Uses { reference, with } => {
                    self.uses(reference, with, env, deadline, depth, outputs)
                        .await
                }
            }
        })
    }

    async fn script(
        &mut self,
        script: &str,
        shell: Option<&str>,
        directory: &Path,
        env: BTreeMap<String, String>,
        deadline: Instant,
        outputs: &mut BTreeMap<String, String>,
    ) -> Result<bool> {
        // GitHub gives a job container sh and a host job bash, whatever runs the container engine.
        let explicit_shell = shell.is_some();
        let shell = shell.unwrap_or(if self.explicit_container {
            "sh"
        } else {
            "bash"
        });
        let mut parts = commands::words(shell)?;
        if parts.is_empty() {
            bail!("Invalid shell: empty shell");
        }
        let mut program = parts.remove(0);
        if program == "bash"
            && !explicit_shell
            && self.podman.job.is_none()
            && !executable("bash", &env)
            && shell == "bash"
        {
            program = "sh".into();
        }
        let extension = match program.as_str() {
            "python" | "python3" => "py",
            "pwsh" | "powershell" => "ps1",
            "cmd" => "cmd",
            _ => "sh",
        };
        self.sequence += 1;
        let path = self
            .temp
            .path()
            .join(format!("script-{}.{}", self.sequence, extension));
        let script = match program.as_str() {
            "pwsh" | "powershell" => format!("$ErrorActionPreference = 'stop'\n{script}\nif ((Test-Path -LiteralPath variable:\\LASTEXITCODE)) {{ exit $LASTEXITCODE }}"),
            "cmd" => format!("@echo off\n{script}"),
            _ => script.into(),
        };
        crate::fs::replace_file(&path, script.as_bytes())?;
        if parts.is_empty() {
            parts = match program.as_str() {
                // Only an explicitly chosen bash gets pipefail. GitHub's default is bash -e.
                "bash" if explicit_shell => {
                    vec!["--noprofile", "--norc", "-e", "-o", "pipefail", "{0}"]
                }
                "bash" | "sh" => vec!["-e", "{0}"],
                "pwsh" | "powershell" => vec!["-command", ". '{0}'"],
                "cmd" => vec!["/D", "/E:ON", "/V:OFF", "/S", "/C", "CALL \"{0}\""],
                "python" | "python3" => vec!["{0}"],
                _ => bail!("Invalid shell '{shell}'. A custom shell must contain '{{0}}'"),
            }
            .into_iter()
            .map(str::to_owned)
            .collect();
        }
        if !parts.iter().any(|part| part.contains("{0}")) {
            bail!("Invalid shell '{shell}'. A custom shell must contain '{{0}}'");
        }
        for part in &mut parts {
            *part = part.replace("{0}", &path.display().to_string());
        }
        self.invoke(&program, &parts, directory, env, deadline, outputs, None)
            .await
    }

    fn environment(&self, mut env: BTreeMap<String, String>) -> BTreeMap<String, String> {
        if let Some(Value::Object(github)) = self.contexts.values.get("github") {
            for (key, value) in github {
                if key != "token"
                    && !matches!(value, Value::Object(_) | Value::Array(_) | Value::Null)
                {
                    env.insert(
                        format!("GITHUB_{}", key.to_ascii_uppercase()),
                        value.to_text(),
                    );
                }
            }
        }
        env.entry("CI".into()).or_insert_with(|| "true".into());
        for (key, value) in [
            ("GITHUB_ACTIONS", "true"),
            ("RUNNER_OS", runner_os()),
            ("RUNNER_ARCH", runner_arch()),
        ] {
            env.insert(key.into(), value.into());
        }
        env.insert("RUNNER_TEMP".into(), self.temp.path().display().to_string());
        env.insert(
            "RUNNER_TOOL_CACHE".into(),
            self.cache.join("tool_cache").display().to_string(),
        );
        let mut paths = self.paths.iter().rev().cloned().collect::<Vec<_>>();
        let base = env.get("PATH").cloned().or_else(|| {
            if self.podman.job.is_some() { self.podman.path.clone() }
            else { Some(self.runtime_path.clone()) }
        });
        if let Some(base) = base { paths.push(base); }
        if !paths.is_empty() { env.insert("PATH".into(), paths.join(":")); }
        env
    }

    fn prepare_files(&mut self) -> Result<()> {
        self.sequence += 1;
        let files = crate::fs::private_tempdir("commands-", Some(self.temp.path()))?.keep();
        for name in ["ENV", "OUTPUT", "PATH", "STEP_SUMMARY", "STATE"] {
            let path = files.join(name);
            crate::fs::replace_file(&path, b"")?;
            if let Some(Value::Object(github)) = self.contexts.values.get_mut("github") {
                github.insert(
                    name.to_ascii_lowercase(),
                    Value::String(path.display().to_string()),
                );
            }
        }
        self.active_files = files;
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    async fn invoke(
        &mut self,
        program: &str,
        args: &[String],
        directory: &Path,
        env: BTreeMap<String, String>,
        deadline: Instant,
        outputs: &mut BTreeMap<String, String>,
        mut state: Option<&mut BTreeMap<String, String>>,
    ) -> Result<bool> {
        let mut env = self.environment(env);
        let files = self.active_files.clone();
        let event = self.restore_event()?;
        env.insert("GITHUB_EVENT_PATH".into(), event.display().to_string());
        let allow_unsafe = env
            .get("ACTIONS_ALLOW_UNSECURE_COMMANDS")
            .is_some_and(|value| value.eq_ignore_ascii_case("true"));
        let mut command = if self.podman.job.is_some() {
            self.podman.exec(&env, directory, program, args)
        } else {
            let mut command = Command::new(program);
            command.args(args).envs(&env).current_dir(directory);
            command
        };
        let mut commands = Commands::new(
            self.options.directory.clone(),
            std::mem::take(&mut self.matchers),
        );
        commands.stopped = self.command_stopped.take();
        commands.debug = self.options.debug || env.get("ACTIONS_STEP_DEBUG").is_some_and(|value| value.eq_ignore_ascii_case("true"));
        let result = process::execute(
            &mut command,
            &self.log,
            &mut commands,
            &mut self.cancel,
            deadline,
            allow_unsafe,
        )
        .await;
        self.command_stopped = commands.stopped.take();
        self.matchers = commands.matchers;
        self.saved_env.extend(commands.env);
        self.paths.extend(commands.paths);
        commands::update_outputs(outputs, commands.outputs);
        // The deprecated ::save-state command feeds the same STATE_ values as GITHUB_STATE.
        if let Some(state) = state.as_mut() {
            state.extend(commands.state);
        }
        self.read_files(&files, outputs, state)?;
        result
    }

    fn read_files(
        &mut self,
        files: &Path,
        outputs: &mut BTreeMap<String, String>,
        state: Option<&mut BTreeMap<String, String>>,
    ) -> Result<()> {
        for (key, value) in commands::pairs(&read_command_file(&files.join("ENV"))?)? {
            if key.eq_ignore_ascii_case("NODE_OPTIONS") {
                self.log
                    .line("Error: Can't store NODE_OPTIONS using '$GITHUB_ENV' command.");
            } else if !key.starts_with("GITHUB_") && !key.starts_with("RUNNER_") {
                self.saved_env.insert(key, value);
            }
        }
        commands::update_outputs(outputs, commands::entries(&read_command_file(&files.join("OUTPUT"))?)?);
        if let Some(state) = state {
            state.extend(commands::pairs(&read_command_file(&files.join("STATE"))?)?);
        }
        let summary = files.join("STEP_SUMMARY");
        let summary_file = match open_command_file(&summary, false) {
            Ok(file) => Some(file),
            Err(error) if error.downcast_ref::<std::io::Error>().is_some_and(|error| error.kind() == std::io::ErrorKind::NotFound) => None,
            Err(error) => return Err(error),
        };
        if let Some(file) = summary_file {
            if file.metadata()?.len() > COMMAND_FILE_LIMIT {
                self.log.line("Error: Step summary exceeds the maximum allowed size of 1024k.");
                write_command_file(&summary, "")?;
            } else {
                let contents = read_command_file(&summary)?;
                write_command_file(&summary, &self.log.masker.mask(&contents))?;
            }
        }
        for path in read_command_file(&files.join("PATH"))?
            .lines()
            .filter(|line| !line.is_empty())
        {
            self.paths.retain(|previous| previous != path);
            self.paths.push(path.into());
        }
        Ok(())
    }
}

/// The largest command file the runner reads. It matches GitHub's step summary limit.
const COMMAND_FILE_LIMIT: u64 = 1024 * 1024;

/// Opens a command file that the container could have replaced with a link or a FIFO. Links are
/// not followed and the open never blocks, and only regular files are accepted.
fn open_command_file(path: &Path, write: bool) -> Result<std::fs::File> {
    let mut options = std::fs::OpenOptions::new();
    options.read(true).write(write);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    let file = options.open(path).context("Unable to open command file")?;
    anyhow::ensure!(
        file.metadata()?.is_file(),
        "A command file is not a regular file"
    );
    Ok(file)
}

fn read_command_file(path: &Path) -> Result<String> {
    use std::io::Read;
    let mut contents = String::new();
    open_command_file(path, false)?
        .take(COMMAND_FILE_LIMIT + 1)
        .read_to_string(&mut contents)?;
    anyhow::ensure!(
        contents.len() as u64 <= COMMAND_FILE_LIMIT,
        "A command file exceeds the maximum allowed size"
    );
    if contents.starts_with('\u{feff}') { contents.drain(..3); }
    Ok(contents)
}

fn write_command_file(path: &Path, contents: &str) -> Result<()> {
    let _file = open_command_file(path, false)?;
    crate::fs::replace_file(path, contents.as_bytes())?;
    Ok(())
}

fn executable(name: &str, env: &BTreeMap<String, String>) -> bool {
    env.get("PATH")
        .cloned()
        .or_else(|| std::env::var("PATH").ok())
        .is_some_and(|path| {
            std::env::split_paths(&path).any(|directory| directory.join(name).is_file())
        })
}
fn runner_os() -> &'static str {
    if cfg!(target_os = "windows") {
        "Windows"
    } else if cfg!(target_os = "macos") {
        "macOS"
    } else {
        "Linux"
    }
}
fn runner_arch() -> &'static str {
    if cfg!(target_arch = "x86_64") {
        "X64"
    } else if cfg!(target_arch = "aarch64") {
        "ARM64"
    } else {
        "ARM"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replay_retains_only_unselected_outputs_in_actual_completion_order() {
        let mut state = JobState::default();
        state.variants.insert(
            "build".into(),
            [(0, "zero"), (1, "selected"), (2, "two")]
                .into_iter()
                .map(|(index, value)| {
                    (
                        index,
                        JobResult {
                            result: "success".into(),
                            outputs: BTreeMap::from([("value".into(), value.into())]),
                        },
                    )
                })
                .collect(),
        );
        let selected = std::collections::BTreeSet::from([1]);
        assert!(state
            .retained_outputs("build", &selected, 3, false)
            .is_err());
        state.variant_order.insert("build".into(), vec![2, 0, 1]);
        assert!(state.has_valid_variant_order());
        assert_eq!(
            state
                .retained_outputs("build", &selected, 3, false)
                .unwrap()["value"],
            "zero"
        );
        // A called workflow keeps the last successful nonempty value.
        state
            .variants
            .get_mut("build")
            .unwrap()
            .get_mut(&0)
            .unwrap()
            .result = "failure".into();
        assert_eq!(
            state.retained_outputs("build", &selected, 3, true).unwrap()["value"],
            "two"
        );
        state.variant_order.insert("build".into(), vec![2, 2, 1]);
        assert!(!state.has_valid_variant_order());
    }

    #[test]
    fn matrix_observations_preserve_null_and_never_expose_secrets() {
        let masker = Masker::default();
        masker.add("quoted\"secret\n");
        assert!(matrix_metadata(&Value::String("quoted\"secret\n".into()), &masker).is_none());
        let matrix = matrix_metadata(&Value::Null, &masker).unwrap();
        assert!(matrix.values.is_null());
        let update = Update::Queued {
            workflow_name: "Workflow".into(),
            name: "Job".into(),
            labels: vec![],
            matrix: Some(matrix),
        };
        let decoded: Update =
            serde_json::from_slice(&serde_json::to_vec(&update).unwrap()).unwrap();
        assert!(
            matches!(decoded,Update::Queued{matrix:Some(matrix),..} if matrix.values.is_null())
        );
        let oversized = Value::String("x".repeat(8193));
        assert!(matrix_metadata(&oversized, &masker).is_none());
    }

    fn step(id: &str, script: &str) -> Step {
        Step {
            id: Some(id.into()),
            name: None,
            condition: None,
            action: StepAction::Run {
                script: script.into(),
                shell: Some("bash".into()),
                working_directory: None,
            },
            env: BTreeMap::new(),
            continue_on_error: None,
            timeout_minutes: None,
        }
    }

    async fn planned(steps: Vec<Step>) -> JobResult {
        let workspace = tempfile::tempdir().unwrap();
        let mut options =
            Options::try_parse_from(["codeyard-actions", "push", "-W", "unused"]).unwrap();
        options.directory = workspace.path().to_owned();
        let plan = PlannedJob {
            job_id: "test".into(),
            name: "test".into(),
            needs: Vec::new(),
            runs_on: vec!["self-hosted".into()],
            runs_on_group: None,
            matrix: Value::Null,
            fail_fast: true,
            max_parallel: None,
            timeout_minutes: Some(1.0),
            continue_on_error: false,
            env: BTreeMap::new(),
            default_shell: None,
            default_working_directory: None,
            container: None,
            services: BTreeMap::new(),
            steps,
            outputs: BTreeMap::from([("value".into(), "${{ steps.two.outputs.value }}".into())]),
            call: None,
            concurrency: None,
        };
        let mut contexts = Contexts::default();
        for name in [
            "github", "env", "inputs", "vars", "steps", "job", "strategy",
        ] {
            contexts
                .values
                .insert(name.into(), Value::Object(BTreeMap::new()));
        }
        let (_tx, cancel) = watch::channel(false);
        run_job(
            plan,
            0,
            Arc::new(options),
            contexts,
            BTreeMap::new(),
            Log {
                prefix: "unit/test".into(),
                masker: Masker::default(),
                ..Default::default()
            },
            cancel,
        )
        .await
    }

    #[test]
    fn accepts_the_ci_workers_act_options() {
        let options = Options::try_parse_from([
            "codeyard-actions",
            "push",
            "-W",
            "ci.yml",
            "--directory",
            "/checkout",
            "--env",
            "GITHUB_REPOSITORY=owner/repo",
            "--env",
            "GITHUB_REF=refs/heads/main",
            "--actor",
            "actor",
            "--eventpath",
            "event.json",
            "-P",
            "ubuntu-latest=alpine",
            "-P",
            "self-hosted=-self-hosted",
            "--pull=false",
            "--concurrent-jobs",
            "2",
            "--container-options",
            "--label codeyard.run=123 --memory 2g",
            "--container-daemon-socket",
            "-",
            "--secret-file",
            "secrets",
            "--no-cache-server",
            "--action-offline-mode",
        ])
        .unwrap();
        assert!(!options.pull);
        assert_eq!(options.concurrent_jobs, 2);
        assert_eq!(
            options.container_options,
            "--label codeyard.run=123 --memory 2g"
        );
        assert!(options.action_offline_mode);
        assert_eq!(options.env.len(), 2);
    }

    #[tokio::test]
    async fn planned_host_steps_persist_file_commands() {
        let result = planned(vec![
            step("one", "echo 'value=first' >> \"$GITHUB_OUTPUT\"\necho 'SAVED=persisted' >> \"$GITHUB_ENV\""),
            step("two", "test '${{ steps.one.outputs.value }}' = first\ntest \"$SAVED\" = persisted\necho \"value=$SAVED\" >> '${{ github.output }}'"),
        ]).await;
        assert_eq!(result.result, "success");
        assert_eq!(result.outputs["value"], "persisted");
    }

    #[tokio::test]
    async fn planned_steps_keep_outcome_and_conclusion_separate() {
        let mut tolerated = step("one", "exit 1");
        tolerated.continue_on_error = Some("true".into());
        let result = planned(vec![tolerated, step("two", "test '${{ steps.one.outcome }}' = failure\ntest '${{ steps.one.conclusion }}' = success\necho 'value=continued' >> \"$GITHUB_OUTPUT\"")]).await;
        assert_eq!(result.result, "success");
        assert_eq!(result.outputs["value"], "continued");
    }

    #[test]
    fn ref_name_and_api_endpoints_match_github() {
        assert_eq!(ref_name("refs/pull/42/merge"), "42/merge");
        assert_eq!(ref_name("refs/heads/feature/x"), "feature/x");
        assert_eq!(ref_name("refs/tags/v1"), "v1");
        assert_eq!(
            api_endpoints("https://github.com", &BTreeMap::new()),
            (
                "https://api.github.com".to_owned(),
                "https://api.github.com/graphql".to_owned()
            )
        );
        let explicit = BTreeMap::from([(
            "GITHUB_API_URL".to_owned(),
            "https://api.example".to_owned(),
        )]);
        assert_eq!(
            api_endpoints("https://codeyard.example", &explicit).0,
            "https://api.example"
        );
        assert_eq!(
            api_endpoints("https://codeyard.example", &BTreeMap::new()).1,
            "https://codeyard.example/api/graphql"
        );
    }

    #[test]
    fn dispatch_inputs_take_their_declared_types() {
        let raw: serde_yaml::Value = serde_yaml::from_str(
            "on:\n  workflow_dispatch:\n    inputs:\n      publish: {type: boolean}\n      count: {type: number}\n      name: {type: string, default: 'true'}\n",
        )
        .unwrap();
        assert_eq!(
            typed_dispatch_inputs(
                &raw,
                serde_json::json!({"publish": "false", "count": "2.5"})
            ).unwrap().typed,
            serde_json::json!({"publish": false, "count": 2.5, "name": "true"})
        );
    }

    #[cfg(unix)]
    #[test]
    fn command_files_are_not_read_through_links_or_past_the_limit() {
        let temp = tempfile::tempdir().unwrap();
        let secret = temp.path().join("secret");
        std::fs::write(&secret, "credential").unwrap();
        let link = temp.path().join("PATH");
        std::os::unix::fs::symlink(&secret, &link).unwrap();
        assert!(read_command_file(&link).is_err());
        let large = temp.path().join("OUTPUT");
        std::fs::write(&large, vec![b'x'; COMMAND_FILE_LIMIT as usize + 1]).unwrap();
        assert!(read_command_file(&large).is_err());
    }

    #[test]
    fn empty_matrix_outputs_keep_earlier_values() {
        let mut outputs = BTreeMap::from([("value".to_owned(), "release".to_owned())]);
        merge_outputs(
            &mut outputs,
            BTreeMap::from([("value".to_owned(), String::new())]),
        );
        assert_eq!(outputs["value"], "release");
    }

    #[tokio::test]
    async fn implicit_shell_is_bash_errexit_without_pipefail() {
        let mut implicit = step("implicit", "false | true");
        if let StepAction::Run { shell, .. } = &mut implicit.action {
            *shell = None;
        }
        assert_eq!(planned(vec![implicit]).await.result, "success");
        assert_eq!(
            planned(vec![step("explicit", "false | true")]).await.result,
            "failure"
        );
    }
}
