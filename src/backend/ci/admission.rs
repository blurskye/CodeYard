use super::config::{CiConfig, CiPipelineKind};
use super::dispatch_inputs;
use super::runs::CiRunRequest;
use super::workflows::{infer_pipeline_from_workflow_path, workflow_definition_from_yaml};
use crate::backend::db;
use crate::backend::repositories::store::{repo_store, ErrorKind, RepoError};
use anyhow::Result;
use std::collections::{BTreeMap, BTreeSet};

#[derive(Debug, thiserror::Error)]
pub enum AdmissionError {
    /// The workflow, or Actions for the repository, is turned off: nothing runs, and event
    /// paths skip it quietly.
    #[error("{0}")]
    WorkflowDisabled(&'static str),
    #[error("{0}")]
    Missing(&'static str),
    #[error("{0}")]
    Denied(&'static str),
    #[error("{0}")]
    Conflict(&'static str),
    #[error("{0}")]
    Invalid(&'static str),
    #[error("{0}")]
    InvalidRequest(&'static str),
    #[error("CI is unavailable right now; check run history before retrying")]
    Unavailable,
}

impl From<anyhow::Error> for AdmissionError {
    fn from(error: anyhow::Error) -> Self {
        let error = match error.downcast::<Self>() {
            Ok(error) => return error,
            Err(error) => error,
        };
        match error.downcast::<db::ci::CiAdmissionRefusal>() {
            Ok(db::ci::CiAdmissionRefusal::WorkflowDisabled) => Self::WorkflowDisabled("Workflow is disabled"),
            Ok(db::ci::CiAdmissionRefusal::Missing(message)) => Self::Missing(message),
            Ok(db::ci::CiAdmissionRefusal::Denied(message)) => Self::Denied(message),
            Ok(db::ci::CiAdmissionRefusal::Conflict(message)) => Self::Conflict(message),
            Ok(db::ci::CiAdmissionRefusal::Invalid(message)) => Self::Invalid(message),
            Err(_) => Self::Unavailable,
        }
    }
}

impl From<tokio::time::error::Elapsed> for AdmissionError {
    fn from(_: tokio::time::error::Elapsed) -> Self {
        Self::Unavailable
    }
}

impl From<RepoError> for AdmissionError {
    fn from(error: RepoError) -> Self {
        match error.kind {
            ErrorKind::Invalid | ErrorKind::MissingPath => {
                Self::Invalid("CI revision or workflow is invalid")
            }
            ErrorKind::NotFound | ErrorKind::Conflict => Self::Conflict("CI ref unavailable"),
            ErrorKind::Unavailable => Self::Unavailable,
        }
    }
}

pub fn is_safe_owner(value: &str) -> bool {
    crate::backend::identity::credentials::validate_namespace(value, 3).is_ok()
        // Also accept the names the repository rules allow, such as short or dotted namespaces.
        || crate::backend::repositories::lifecycle::validate_name(value).is_ok()
}

pub(super) struct AdmittedRevision {
    pub request: CiRunRequest,
    pub blobs: serde_json::Value,
    pub definitions: BTreeMap<String, String>,
    pub scheduling: db::runners::Scheduling,
}

/// The events a run can be queued for.
pub fn is_supported_event(event: &str) -> bool {
    matches!(
        event,
        "push" | "pull_request" | "pull_request_target" | "workflow_dispatch" | "schedule"
    ) || super::events::is_repository_event(event)
}

/// An instance admin acts as the owner of personal repositories only, not of organization ones.
pub(super) fn is_personal_admin(actor: &db::models::User, repo: &db::models::Repository) -> bool {
    actor.is_admin
        && repo.owner_kind == "user"
        && repo.owner_user_id.is_some()
        && repo.owner_org_id.is_none()
}

/// Whether `actor`, with `access` to `repo`, may start CI there. Used before workflows are
/// read and again when a run is admitted.
pub(super) fn check_actor(
    config: &CiConfig,
    actor: &db::models::User,
    repo: &db::models::Repository,
    access: db::models::AccessLevel,
) -> Result<(), AdmissionError> {
    if actor.is_banned || actor.must_change_password {
        return Err(AdmissionError::Denied("CI actor is restricted"));
    }
    if !actor.is_admin && !config.trusted_actor_ids.contains(&actor.id) {
        return Err(AdmissionError::Denied(
            "CI host execution is not approved for this actor",
        ));
    }
    if repo.is_archived {
        return Err(AdmissionError::Conflict("CI repository is archived"));
    }
    if !is_personal_admin(actor, repo) && !access.can_write() {
        return Err(AdmissionError::Denied(
            "CI repository execution permission denied",
        ));
    }
    Ok(())
}

pub(crate) async fn fork_approval_required(
    pool: &db::DbPool,
    request: &CiRunRequest,
) -> Result<bool, AdmissionError> {
    if request.event != "pull_request" || request.manual {
        return Ok(false);
    }
    let Some(id) = request.pull_request_id else {
        return Ok(false);
    };
    Ok(db::pull_requests::records::get_pull_request_by_id(pool, id)
        .await?
        .is_some_and(|pull| {
            pull.repo_id == request.repo_id
                && pull.state == "open"
                && !pull.is_draft
                && request.head_sha.as_deref() == Some(pull.head_sha.as_str())
                && pull.head_repo_id != Some(pull.repo_id)
                && pull.workflows_approved_sha.as_deref() != Some(pull.head_sha.as_str())
        }))
}

pub(super) async fn resolve(
    pool: &db::DbPool,
    config: &CiConfig,
    context: Option<&crate::backend::cluster::NodeContext>,
    mut request: CiRunRequest,
    rerun: bool,
    event_payload: Option<&serde_json::Value>,
    source_blobs: Option<&serde_json::Value>,
) -> Result<AdmittedRevision, AdmissionError> {
    if !is_supported_event(&request.event) {
        return Err(AdmissionError::Invalid(
            "CI event is unsupported; use workflow_dispatch for manual execution",
        ));
    }
    let pr_revision = request.event == "pull_request";
    let pr_target = request.event == "pull_request_target";
    let reference = if pr_revision || pr_target || !request.branch.starts_with("refs/") {
        format!("refs/heads/{}", request.branch)
    } else {
        request.branch.clone()
    };
    if !is_safe_ref(if pr_revision || pr_target {
        &reference
    } else {
        &request.branch
    }) {
        return Err(AdmissionError::Invalid("invalid CI ref"));
    }
    let dispatch = request.event == "workflow_dispatch";
    if !dispatch
        && request
            .inputs
            .as_ref()
            .is_some_and(|inputs| !inputs.is_empty())
    {
        return Err(AdmissionError::InvalidRequest(
            "workflow inputs need a workflow_dispatch run",
        ));
    }
    let submitted = request.inputs.take().unwrap_or_default();
    let mut run_inputs = BTreeMap::new();
    let lookup_timeout = std::time::Duration::from_secs(5);
    let actor = tokio::time::timeout(
        lookup_timeout,
        db::users::accounts::get_user_by_id(pool, request.triggered_by_id),
    )
    .await??
    .ok_or(AdmissionError::Unavailable)?;
    if !rerun && actor.username == db::ci_tokens::BOT
        && !matches!(
            request.event.as_str(),
            "workflow_dispatch" | "repository_dispatch"
        )
    {
        return Err(AdmissionError::Denied(
            "GITHUB_TOKEN events do not start workflows",
        ));
    }
    let repo = tokio::time::timeout(
        lookup_timeout,
        db::repositories::records::get_repository_by_id(pool, request.repo_id),
    )
    .await??
    .ok_or(AdmissionError::Unavailable)?;
    let access = tokio::time::timeout(
        lookup_timeout,
        db::repositories::access::get_user_repo_access(pool, actor.id, repo.id),
    )
    .await??;
    let awaiting_approval = !rerun && fork_approval_required(pool, &request).await?;
    if awaiting_approval {
        if actor.is_banned || actor.must_change_password || repo.is_archived || !access.can_read() {
            return Err(AdmissionError::Denied("CI repository access denied"));
        }
    } else {
        check_actor(config, &actor, &repo, access)?;
    }
    // With Actions turned off for the site, the organization or the repository, nothing new
    // is queued.
    let enabled = tokio::time::timeout(
        lookup_timeout,
        db::action_settings::actions_enabled(pool, repo.id),
    )
    .await??;
    if !enabled {
        return Err(AdmissionError::WorkflowDisabled(
            "Actions are turned off for this repository",
        ));
    }
    // Never trust the caller's repository name.
    request.repo_name = repo.name.clone();
    request.repo_owner =
        tokio::time::timeout(lookup_timeout, db::ci::ci_repository_owner(pool, repo.id))
            .await??
            .ok_or(AdmissionError::Unavailable)?;
    if request.workflow_paths.is_empty() {
        request.workflow_paths = config.workflow_paths(request.pipeline);
    }
    let mut seen = BTreeSet::new();
    request
        .workflow_paths
        .retain(|path| seen.insert(path.clone()));
    if request.workflow_paths.is_empty() || request.workflow_paths.len() > 8 {
        return Err(AdmissionError::Invalid(
            "CI requires one to eight workflows",
        ));
    }
    if !request
        .workflow_paths
        .iter()
        .all(|p| is_safe_workflow_path(p))
    {
        return Err(AdmissionError::Invalid("invalid CI workflow path"));
    }
    if let Some(environment) = &request.environment {
        if db::environment_values::environment_key(environment).is_err() {
            return Err(AdmissionError::Invalid("invalid CI environment"));
        }
    }
    if request.pipeline == CiPipelineKind::Ci
        && (request.environment.is_some()
            || request
                .workflow_paths
                .iter()
                .any(|path| infer_pipeline_from_workflow_path(path).includes_cd()))
    {
        request.pipeline = CiPipelineKind::Full;
    }
    if !awaiting_approval
        && request.pipeline.includes_cd()
        && !is_personal_admin(&actor, &repo)
        && !access.can_admin()
    {
        return Err(AdmissionError::Denied("CI deployment permission denied"));
    }
    // Recorded pushes and runs keep their ref kind even after the ref is deleted.
    let accepted_push = if request.event == "push" {
        if let Some(sha) = request.head_sha.as_deref() {
            tokio::time::timeout(
                lookup_timeout,
                db::ci::has_ci_accepted_push(pool, repo.id, actor.id, &reference, sha),
            )
            .await??
        } else {
            false
        }
    } else {
        false
    };
    let accepted_event = event_payload.is_some() && super::events::is_repository_event(&request.event) && request.head_sha.is_some();
    let configured_cd = config.workflow_cd.clone();
    if pr_revision && request.head_sha.is_none() {
        return Err(AdmissionError::Invalid(
            "CI pull request requires an immutable head SHA",
        ));
    }
    // Recorded runs and pushes pin their commits; PR heads are checked against
    // the open PR. Other dispatches must still be reachable from the ref.
    let store = repo_store(&repo, context);
    let commit = store
        .resolve_ci_commit(
            if accepted_push || accepted_event || pr_revision || pr_target || rerun {
                &reference
            } else {
                &request.branch
            },
            request.head_sha.as_deref(),
            !accepted_push && !accepted_event && !pr_revision && !rerun,
        )
        .await?;
    // Inputs belong to one workflow, so a run of several cannot take any.
    if dispatch && request.workflow_paths.len() > 1 && !submitted.is_empty() {
        return Err(AdmissionError::InvalidRequest(
            "workflow inputs need a run of one workflow",
        ));
    }
    let mut blobs = BTreeMap::new();
    // A rerun replays the called commits its source admitted instead of resolving moving references.
    let pinned = source_blobs
        .map(super::reusable::pinned_commits)
        .unwrap_or_default();
    let mut called = super::reusable::Resolver::new(
        pool,
        context,
        &repo,
        actor.id,
        request.environment.clone(),
        pinned,
    );
    let mut definitions = BTreeMap::new();
    let mut scheduling = db::runners::Scheduling::default();
    for path in &request.workflow_paths {
        let file =
            store
                .ci_workflow_file(&commit.sha, path)
                .await
                .map_err(|error| match error.kind {
                    ErrorKind::NotFound | ErrorKind::MissingPath | ErrorKind::Invalid => {
                        AdmissionError::Invalid("CI workflow unavailable at admitted SHA")
                    }
                    _ => AdmissionError::from(error),
                })?;
        if rerun {
            definitions.insert(path.clone(), file.content.clone());
        }
        let ParsedWorkflow {
            definition,
            environment,
            scheduling: requirements,
            concurrency,
            yaml,
        } = parse_admission_workflow(
            path.clone(),
            file.content.clone(),
            request.environment.clone(),
        )
        .await?;
        merge_scheduling(&mut scheduling, requirements)?;
        request.environment = environment;
        if (*path == configured_cd || request.environment.is_some())
            && !request.pipeline.includes_cd()
        {
            request.pipeline = CiPipelineKind::Full;
        }
        if dispatch {
            if request.workflow_paths.len() == 1 {
                run_inputs = dispatch_inputs::validate_inputs(
                    &definition.triggers.dispatch_inputs,
                    &submitted,
                )
                .map_err(AdmissionError::InvalidRequest)?;
            } else if !definition.triggers.dispatch_inputs.is_empty() {
                return Err(AdmissionError::InvalidRequest(
                    "a workflow with inputs must be run on its own",
                ));
            }
        }
        if let Some(concurrency) = concurrency {
            if scheduling.concurrency_expression.is_some() || request.workflow_paths.len() != 1 {
                return Err(AdmissionError::Invalid(
                    "Concurrency requires a run of one workflow",
                ));
            }
            let pull = if let Some(id) = request.pull_request_id {
                db::pull_requests::records::get_pull_request_by_id(pool, id).await?
            } else {
                None
            };
            let variables = super::queue::run_variables(pool, repo.id)
                .await
                .map_err(|_| AdmissionError::Unavailable)?;
            let reference = pull.as_ref().filter(|_| pr_revision).map_or_else(
                || commit.full_ref.clone(),
                |pull| format!("refs/pull/{}/merge", pull.number),
            );
            let reference_name = reference
                .strip_prefix("refs/")
                .and_then(|reference| reference.split_once('/'))
                .map_or(reference.as_str(), |(_, name)| name);
            let typed_inputs: serde_json::Map<String, serde_json::Value> = run_inputs
                .iter()
                .map(|(name, value)| {
                    let kind = definition
                        .triggers
                        .dispatch_inputs
                        .iter()
                        .find(|input| &input.name == name)
                        .map(|input| input.kind);
                    let value = if matches!(
                        kind,
                        Some(
                            dispatch_inputs::InputKind::Boolean
                                | dispatch_inputs::InputKind::Number
                        )
                    ) {
                        if kind == Some(dispatch_inputs::InputKind::Number) {
                            value.parse::<f64>().ok().and_then(serde_json::Number::from_f64)
                                .map(serde_json::Value::Number).unwrap_or(serde_json::Value::Null)
                        } else {
                            serde_json::Value::Bool(value == "true")
                        }
                    } else {
                        serde_json::Value::String(value.clone())
                    };
                    (name.clone(), value)
                })
                .collect();
            let mut github = serde_json::json!({
                "workflow":definition.name,
                "workflow_ref":format!("{}/{}/{}@{}",request.repo_owner,repo.name,path,reference),
                "repository":format!("{}/{}",request.repo_owner,repo.name),
                "repository_id":repo.id.to_string(),
                "event_name":request.event,"ref":reference,"ref_name":reference_name,
                "ref_type":if reference.starts_with("refs/tags/") {"tag"} else {"branch"},
                "sha":commit.sha,"actor":actor.username,"actor_id":actor.id.to_string(),
                "head_ref":pull.as_ref().map(|pull|pull.head_branch.as_str()).unwrap_or(""),
                "base_ref":pull.as_ref().map(|_|request.branch.as_str()).unwrap_or(""),
                "event":{"inputs":run_inputs,"pull_request":{"number":pull.as_ref().map(|pull|pull.number)}}
            });
            if let Some(payload) = event_payload {
                github["event"] = payload.clone();
            }
            let contexts = codeyard_actions::expressions::Contexts {
                values: BTreeMap::from([
                    (
                        "github".into(),
                        codeyard_actions::expressions::Value::from_json(&github),
                    ),
                    (
                        "inputs".into(),
                        codeyard_actions::expressions::Value::from_json(
                            &serde_json::Value::Object(typed_inputs),
                        ),
                    ),
                    (
                        "vars".into(),
                        codeyard_actions::expressions::Value::Object(
                            variables
                                .into_iter()
                                .map(|(key, value)| {
                                    (key, codeyard_actions::expressions::Value::String(value))
                                })
                                .collect(),
                        ),
                    ),
                ]),
            };
            scheduling.concurrency_expression = Some((concurrency, contexts));
        }
        // Pull request filters name the base branch itself, even when it looks like a ref.
        let event_ref = if pr_revision || pr_target {
            request.branch.clone()
        } else {
            commit.full_ref.clone()
        };
        let activity_matches =
            event_payload.is_none_or(|payload| definition.matches_payload(&request.event, payload));
        // workflow_run branch filters describe the source run, not this default-branch checkout.
        let event_ref = if request.event == "workflow_run" {
            event_payload
                .and_then(|payload| payload["workflow_run"]["head_branch"].as_str())
                .unwrap_or(&event_ref)
        } else {
            &event_ref
        };
        if !definition.matches_event(&request.event, event_ref) || !activity_matches {
            return Err(AdmissionError::Invalid(
                "CI workflow does not support requested event/ref",
            ));
        }
        let registration = [(path.to_owned(), definition.name.clone())];
        tokio::time::timeout(
            lookup_timeout,
            db::action_workflows::register_workflows(pool, repo.id, &registration, false),
        )
        .await??;
        let has_calls = yaml
            .get("jobs")
            .and_then(serde_yaml::Value::as_mapping)
            .is_some_and(|jobs| jobs.values().any(|job| job.get("uses").is_some()));
        let snapshot = if has_calls {
            called.environment = request.environment.clone();
            let snapshot = tokio::time::timeout(
                std::time::Duration::from_secs(30),
                called.snapshot(
                    repo.clone(),
                    commit.sha.clone(),
                    path.clone(),
                    file.content,
                    Some(yaml),
                    0,
                    None,
                ),
            )
            .await??;
            request.environment = called.environment.clone();
            if request.environment.is_some() && !request.pipeline.includes_cd() {
                request.pipeline = CiPipelineKind::Full;
            }
            snapshot
        } else {
            serde_json::Value::String(file.blob_id)
        };
        blobs.insert(path.clone(), snapshot);
    }
    merge_scheduling(&mut scheduling, called.scheduling)?;
    if called.files.keys().any(|path| blobs.contains_key(path)) {
        return Err(AdmissionError::Invalid("Called workflow path collides with root workflow"));
    }
    blobs.extend(called.files);
    if serde_json::to_vec(&blobs)
        .map_err(|_| AdmissionError::Unavailable)?
        .len()
        > 2 * 1024 * 1024
    {
        return Err(AdmissionError::Invalid(
            "Called workflow snapshots exceed size limit",
        ));
    }
    if request.pipeline.includes_cd() && request.environment.is_none() {
        request.environment = Some("production".to_owned());
    }
    if !pr_revision && !pr_target {
        // Tags retain their full names; storage accommodates every admitted public ref.
        if commit.full_ref.len() > crate::backend::git::ref_names::MAX_REF_BYTES {
            return Err(AdmissionError::Invalid("CI ref name is too long"));
        }
        request.branch = commit.full_ref;
    }
    request.head_sha = Some(commit.sha);
    request.inputs = dispatch.then_some(run_inputs);
    scheduling.labels.sort();
    scheduling.labels.dedup();
    Ok(AdmittedRevision {
        request,
        blobs: serde_json::to_value(blobs).map_err(|_| AdmissionError::Unavailable)?,
        definitions,
        scheduling,
    })
}

/// Labels accumulate across a run's workflows, and a run has at most one runner group.
pub(super) fn merge_scheduling(
    into: &mut db::runners::Scheduling,
    from: db::runners::Scheduling,
) -> Result<(), AdmissionError> {
    if let Some(group) = from.group {
        if into
            .group
            .as_deref()
            .is_some_and(|g| !g.eq_ignore_ascii_case(&group))
        {
            return Err(AdmissionError::Invalid("a run must use one runner group"));
        }
        into.group = Some(group);
    }
    into.labels.extend(from.labels);
    Ok(())
}

pub(super) struct ParsedWorkflow {
    pub definition: super::workflows::WorkflowDefinition,
    pub environment: Option<String>,
    pub scheduling: db::runners::Scheduling,
    pub(super) concurrency: Option<serde_yaml::Value>,
    pub(super) yaml: serde_yaml::Value,
}

pub(super) async fn parse_admission_workflow(
    path: String,
    content: String,
    mut environment: Option<String>,
) -> Result<ParsedWorkflow, AdmissionError> {
    let permit = super::workflows::PARSING_SLOTS
        .acquire()
        .await
        .map_err(|_| AdmissionError::Unavailable)?;
    tokio::task::spawn_blocking(move || {
        let _permit = permit;
        let yaml = super::workflows::parse_yaml(content.as_bytes())
            .map_err(|_| AdmissionError::Invalid("CI workflow YAML is invalid"))?;
        let jobs = yaml
            .get("jobs")
            .and_then(serde_yaml::Value::as_mapping)
            .ok_or(AdmissionError::Invalid("CI workflow requires actual jobs"))?;
        if jobs.is_empty() || jobs.len() > 128 {
            return Err(AdmissionError::Invalid(
                "CI workflow requires one to 128 jobs",
            ));
        }
        for id in jobs.keys() {
            if !id.as_str().is_some_and(codeyard_actions::workflow::valid_identifier) {
                return Err(AdmissionError::Invalid("CI job identifier is invalid"));
            }
        }
        for job in jobs.values() {
            if job.get("steps").and_then(serde_yaml::Value::as_sequence).is_some_and(|steps| steps.len() > codeyard_actions::workflow::MAX_JOB_STEPS) {
                return Err(AdmissionError::Invalid("CI job has too many steps"));
            }
            if let Some(value) = job.get("environment") {
                let name = value
                    .as_str()
                    .or_else(|| value.get("name").and_then(serde_yaml::Value::as_str))
                    .ok_or(AdmissionError::Invalid(
                        "CI dynamic workflow environments are unsupported",
                    ))?;
                if db::environment_values::environment_key(name).is_err() || name.contains("${{") {
                    return Err(AdmissionError::Invalid(
                        "CI dynamic workflow environments are unsupported",
                    ));
                }
                if let Some(environment) = &environment {
                    if db::environment_values::environment_key(environment).ok()
                        != db::environment_values::environment_key(name).ok()
                    {
                        return Err(AdmissionError::Invalid(
                            "CI workflow environment differs from requested target",
                        ));
                    }
                } else {
                    environment = Some(name.to_owned());
                }
            }
        }
        let definition = workflow_definition_from_yaml(&path, &yaml)
            .map_err(|_| AdmissionError::Invalid("CI workflow YAML is invalid"))?;
        let scheduling = super::runner_labels::scheduling_from_value(&yaml, false)
            .map_err(|_| AdmissionError::Invalid("CI runner selection is invalid"))?;
        Ok(ParsedWorkflow {
            definition,
            environment,
            scheduling,
            concurrency: yaml.get("concurrency").cloned(),
            yaml,
        })
    })
    .await
    .map_err(|_| AdmissionError::Unavailable)?
}

pub fn is_safe_ref(value: &str) -> bool {
    let reference = if value.starts_with("refs/") {
        value.to_owned()
    } else {
        format!("refs/heads/{value}")
    };
    !value.starts_with('-')
        && crate::backend::git::ref_names::is_canonical_ref(&reference)
}

pub fn is_safe_workflow_path(value: &str) -> bool {
    value
        .strip_prefix(".github/workflows/")
        .is_some_and(crate::backend::git::ci::is_ci_workflow_filename)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn admission_errors_preserve_domain_and_database_refusals() {
        assert!(matches!(
            AdmissionError::from(anyhow::Error::new(AdmissionError::InvalidRequest(
                "request"
            ))),
            AdmissionError::InvalidRequest("request")
        ));
        for (refusal, expected) in [
            (db::ci::CiAdmissionRefusal::Missing("missing"), "missing"),
            (db::ci::CiAdmissionRefusal::Denied("denied"), "denied"),
            (db::ci::CiAdmissionRefusal::Conflict("conflict"), "conflict"),
            (db::ci::CiAdmissionRefusal::Invalid("invalid"), "invalid"),
        ] {
            let error = AdmissionError::from(anyhow::Error::new(refusal).context("enqueue CI"));
            assert!(matches!(
                (expected, error),
                ("missing", AdmissionError::Missing("missing"))
                    | ("denied", AdmissionError::Denied("denied"))
                    | ("conflict", AdmissionError::Conflict("conflict"))
                    | ("invalid", AdmissionError::Invalid("invalid"))
            ));
        }
        assert!(matches!(
            AdmissionError::from(anyhow::anyhow!("CI actor is restricted")),
            AdmissionError::Unavailable
        ));
        assert!(matches!(
            AdmissionError::from(anyhow::Error::new(sqlx::Error::PoolClosed)),
            AdmissionError::Unavailable
        ));
    }

    #[test]
    fn names_and_refs_follow_what_the_forge_accepts() {
        assert!(is_safe_owner("功能"));
        assert!(is_safe_ref("refs/heads/équipe/功能"));
        assert!(is_safe_ref("feature/c++"));
        assert!(is_safe_ref("refs/tags/v1.2.3+build.1"));
        assert!(is_safe_ref(&format!("refs/heads/{}", "a".repeat(255))));
        assert!(is_safe_ref(&format!("refs/heads/{}", "a".repeat(256))));
        assert!(!is_safe_ref("refs/heads/a.lock"));
        assert!(!is_safe_owner("a b"));
        assert!(!is_safe_ref("refs/heads/a b"));
        assert!(!is_safe_ref("-option"));
    }

    #[tokio::test]
    async fn admission_parsing_checks_jobs_and_extracts_triggers_and_environment() {
        let path = ".github/workflows/unit-test.yml".to_owned();
        let content = "name: Test\non: workflow_dispatch\njobs:\n  test:\n    runs-on: ubuntu-latest\n    environment: staging\n    steps: [{run: echo ok}]\n".to_owned();
        let ParsedWorkflow {
            definition,
            environment,
            scheduling,
            ..
        } = parse_admission_workflow(path.clone(), content.clone(), None)
            .await
            .unwrap();
        assert!(definition.triggers.workflow_dispatch);
        assert_eq!(environment.as_deref(), Some("staging"));
        assert_eq!(scheduling.labels, ["ubuntu-latest"]);
        assert!(
            parse_admission_workflow(path.clone(), content, Some("production".into()))
                .await
                .is_err()
        );
        assert!(
            parse_admission_workflow(path, "on: push\njobs: {}".into(), None)
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn admission_matches_unicode_environment_aliases_but_rejects_dynamic_targets() {
        let source="on: workflow_dispatch\njobs:\n  first:\n    runs-on: ubuntu-latest\n    environment: İ\n    steps: [{run: 'true'}]\n  second:\n    runs-on: ubuntu-latest\n    environment: i\u{307}\n    steps: [{run: 'true'}]\n";
        let ParsedWorkflow { environment, .. } =
            parse_admission_workflow("ci.yml".into(), source.into(), Some("i\u{307}".into()))
                .await
                .unwrap();
        assert_eq!(environment.as_deref(), Some("i\u{307}"));
        for target in ["${{ inputs.environment }}", " ", "control\nname"] {
            let source=format!("on: workflow_dispatch\njobs:\n  test:\n    environment: {target:?}\n    steps: [{{run: 'true'}}]\n");
            assert!(parse_admission_workflow("ci.yml".into(), source, None)
                .await
                .is_err());
        }
    }

    #[tokio::test]
    async fn admission_waits_for_shared_parsing_capacity() {
        let permits = super::super::workflows::PARSING_SLOTS
            .acquire_many(2)
            .await
            .unwrap();
        let mut parse = Box::pin(parse_admission_workflow(
            ".github/workflows/ci.yml".into(),
            "on: push\njobs: {test: {runs-on: ubuntu-latest, steps: [{run: echo ok}]}}".into(),
            None,
        ));
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(20), &mut parse)
                .await
                .is_err()
        );
        drop(permits);
        assert!(parse.await.unwrap().definition.triggers.push.is_some());
    }

    #[test]
    fn workflow_paths_are_bounded_regular_file_names() {
        assert!(is_safe_workflow_path(".github/workflows/test.yml"));
        assert!(is_safe_workflow_path(".github/workflows/unit-test.yml"));
        assert!(is_safe_workflow_path(".github/workflows/unit test.yml"));
        assert!(is_safe_workflow_path(".github/workflows/テスト.yml"));
        assert!(is_safe_workflow_path(".github/workflows/a..yml"));
        for bad in [
            ".github/workflows/../test.yml",
            ".github/workflows/nested/test.yml",
            ".github/workflows/a.yml,b.yml",
            ".github/workflows/build:linux.yml",
            ".github/workflows/a.yml ",
        ] {
            assert!(!is_safe_workflow_path(bad));
        }
    }
}
