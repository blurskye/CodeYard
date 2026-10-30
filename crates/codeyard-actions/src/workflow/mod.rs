//! Workflow files, ported from actions/runner's `Sdk/WorkflowParser`: parsing and checking
//! them, the order of their jobs, and expanding a job's matrix into the runs it makes.
//!
//! The planner evaluates the job-level fields GitHub evaluates on the server (`runs-on`,
//! `name`, `strategy`, `timeout-minutes`, `continue-on-error`, the job's `if`). Everything in
//! [`PlannedJob`] and [`Step`] that stays a `String` is a template the runner evaluates when
//! the job or step starts. The public items below are the interface the runner uses; keep
//! their names and signatures, add what else you need.
use crate::expressions::{Contexts, Status};

pub mod dispatch_inputs;
pub mod events;
mod parser;
mod planner;
pub use parser::text as scalar_text;
#[cfg(test)]
mod tests;
use std::collections::BTreeMap;

pub const MAX_JOB_STEPS: usize = crate::progress::MAX_STEP_NUMBER as usize - 2;

pub fn validate_timeout_minutes(value: f64) -> anyhow::Result<()> {
    anyhow::ensure!(value.is_finite() && value > 0.0 && value <= 525_600.0, "timeout-minutes must be a positive number no greater than one year");
    Ok(())
}

pub const MAX_REUSABLE_LEVELS: usize = 4;
pub const MAX_CALLED_WORKFLOWS: usize = 20;
pub fn has_job_concurrency(raw: &serde_yaml::Value) -> bool {
    raw.get("jobs").and_then(serde_yaml::Value::as_mapping)
        .is_some_and(|jobs| jobs.values().any(|job| job.get("concurrency").is_some()))
}
pub fn validate_called_concurrency(raw: &serde_yaml::Value) -> anyhow::Result<()> {
    anyhow::ensure!(raw.get("concurrency").is_none() && !has_job_concurrency(raw), "Called workflow concurrency is unsupported");
    Ok(())
}
pub const MAX_CONCURRENCY_GROUP_BYTES: usize = 256;
pub fn valid_concurrency_group(group: &str) -> bool {
    !group.is_empty() && group.len() <= MAX_CONCURRENCY_GROUP_BYTES && !group.chars().any(char::is_control)
}

pub const PERMISSION_SCOPES: &[&str] = &[
    "actions", "artifact-metadata", "attestations", "checks", "contents", "deployments",
    "discussions", "id-token", "issues", "models", "packages", "pages", "pull-requests",
    "repository-projects", "security-events", "statuses", "vulnerability-alerts", "metadata",
];

pub fn permission_level_allowed(scope: &str, level: &str) -> bool {
    PERMISSION_SCOPES.contains(&scope) && match level {
        "none" => true,
        "read" => scope != "id-token",
        "write" => !matches!(scope, "models" | "metadata" | "vulnerability-alerts"),
        _ => false,
    }
}

pub fn valid_identifier(id: &str) -> bool {
    !id.is_empty() && id.len() < 100 && !id.starts_with("__")
        && id.bytes().enumerate().all(|(i, b)| b.is_ascii_alphabetic() || b == b'_' || (i > 0 && (b.is_ascii_digit() || b == b'-')))
}

/// A parsed and checked workflow file. Its fields belong to this module.
#[derive(Clone, Debug)]
pub struct Workflow {
    /// The original checked fields, including event filters and unenforced settings.
    pub fields: serde_yaml::Mapping,
    jobs: Vec<(String, serde_yaml::Mapping)>,
}

/// One run of a job: the job itself, or one combination of its matrix.
#[derive(Clone, Debug, PartialEq)]
pub struct PlannedJob {
    /// The key under `jobs:`.
    pub job_id: String,
    /// The name shown for this run, with the matrix values as GitHub adds them.
    pub name: String,
    pub needs: Vec<String>,
    pub runs_on: Vec<String>,
    /// A runner group is separate from its required labels.
    pub runs_on_group: Option<String>,
    /// This run's matrix values, or `Value::Null` without a matrix.
    pub matrix: crate::expressions::Value,
    pub fail_fast: bool,
    pub max_parallel: Option<u32>,
    pub timeout_minutes: Option<f64>,
    pub continue_on_error: bool,
    /// Workflow and job environment scopes, retained until their execution contexts exist.
    pub env: Vec<EnvironmentTemplate>,
    /// `defaults.run`, the job's over the workflow's.
    pub default_shell: Option<String>,
    pub default_working_directory: Option<String>,
    pub container: Option<Container>,
    pub services: BTreeMap<String, Container>,
    pub steps: Vec<Step>,
    pub outputs: BTreeMap<String, String>,
    /// A workflow call, evaluated with this run's matrix and dependency contexts.
    pub call: Option<serde_yaml::Mapping>,
    /// The job's `concurrency`, evaluated with this run's matrix and dependency contexts.
    pub concurrency: Option<Concurrency>,
}

/// A job's concurrency group. At most one job in a group runs and one waits; a newer job
/// replaces the waiting one, and with `cancel_in_progress` also cancels the running one.
#[derive(Clone, Debug, PartialEq)]
pub struct Concurrency {
    pub group: String,
    pub cancel_in_progress: bool,
}

/// A job container or a service container.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Container {
    pub image: String,
    pub credentials: Option<(String, String)>,
    pub env: BTreeMap<String, String>,
    pub ports: Vec<String>,
    pub volumes: Vec<String>,
    pub options: Option<String>,
}

/// Environment entries or one expression producing an environment object.
#[derive(Clone, Debug, PartialEq)]
pub enum EnvironmentTemplate {
    Entries(BTreeMap<String, String>),
    Expression(String),
}
impl Default for EnvironmentTemplate {
    fn default() -> Self { Self::Entries(BTreeMap::new()) }
}
impl EnvironmentTemplate {
    pub fn resolve(&self, contexts: &Contexts, status: Status) -> anyhow::Result<BTreeMap<String, String>> {
        use crate::expressions::{self, Value};
        fn insert(result: &mut BTreeMap<String, String>, bytes: &mut usize, key: String, value: Value) -> anyhow::Result<()> {
            anyhow::ensure!(!key.is_empty() && !key.contains(['=', '\0']), "Invalid environment name");
            anyhow::ensure!(!matches!(value, Value::Array(_) | Value::Object(_)), "Environment values must be scalar");
            let value = value.to_text();
            anyhow::ensure!(!value.contains('\0'), "Invalid environment value");
            *bytes = bytes.saturating_add(key.len()).saturating_add(value.len());
            anyhow::ensure!(*bytes <= crate::values::MAX_ENVIRONMENT_FILE_BYTES, "Environment scope exceeds its delivery budget");
            result.insert(key, value);
            Ok(())
        }
        let mut result = BTreeMap::new();
        let mut bytes = 0;
        match self {
            Self::Entries(entries) => for (key, template) in entries {
                insert(&mut result, &mut bytes, key.clone(), expressions::interpolate(template, contexts, status)?)?;
            },
            Self::Expression(template) => {
                let Value::Object(entries) = expressions::interpolate(template, contexts, status)? else {
                    anyhow::bail!("Environment expression must produce an object");
                };
                for (key, value) in entries { insert(&mut result, &mut bytes, key, value)?; }
            }
        }
        Ok(result)
    }
}

/// A step of a job or of a composite action.
#[derive(Clone, Debug, PartialEq)]
pub struct Step {
    pub id: Option<String>,
    pub name: Option<String>,
    pub condition: Option<String>,
    pub action: StepAction,
    pub env: EnvironmentTemplate,
    pub continue_on_error: Option<String>,
    pub timeout_minutes: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum StepAction {
    Run {
        script: String,
        shell: Option<String>,
        working_directory: Option<String>,
    },
    /// `uses:` an action: `owner/repo@ref`, `owner/repo/path@ref`, `./local/path` or
    /// `docker://image`.
    Uses {
        reference: String,
        with: BTreeMap<String, String>,
    },
}

/// Parses and checks a workflow file, with errors that name the line, as GitHub reports them.
pub fn parse(yaml: &str) -> anyhow::Result<Workflow> {
    parser::parse(yaml)
}

/// Parses one step; the runner uses it for the steps of composite actions.
pub fn parse_step(value: &serde_yaml::Value) -> anyhow::Result<Step> {
    parser::parse_step(value)
}

/// Whether the workflow's `on:` lists this event. Branch, tag and path filters are Codeyard's
/// business before it starts a run, so they are not checked here, as `act` does not either.
pub fn listens_to(workflow: &Workflow, event: &str) -> bool {
    events::declares_event(workflow.fields.get("on"), event)
}

/// The job ids in an order where every job comes after the jobs it needs. Cycles and unknown
/// needs are errors.
pub fn job_order(workflow: &Workflow) -> anyhow::Result<Vec<String>> {
    planner::job_order(workflow)
}

/// The runs a job makes once the jobs it needs have finished: `contexts` holds `github`,
/// `needs`, `vars`, `inputs` and the others the job level may read, and `status` says whether
/// a needed job failed. `None` means the job's `if` skipped it.
pub fn expand(
    workflow: &Workflow,
    job_id: &str,
    contexts: &Contexts,
    status: Status,
) -> anyhow::Result<Option<Vec<PlannedJob>>> {
    planner::expand(workflow, job_id, contexts, status)
}
