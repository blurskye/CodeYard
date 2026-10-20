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
    crate::backend::identity::credentials::validate_username(value).is_ok()
        // Also accept the names the repository rules allow, such as short or dotted namespaces.
        || crate::backend::repositories::lifecycle::validate_name(value).is_ok()
}

pub fn is_valid_sha1_hex(value: &str) -> bool {
    value.len() == 40 && value.bytes().all(|c| c.is_ascii_hexdigit())
}

pub(super) struct AdmittedRevision {
    pub request: CiRunRequest,
    pub blobs: serde_json::Value,
}

/// The events a run can be queued for.
pub fn is_supported_event(event: &str) -> bool {
    matches!(event, "push" | "pull_request" | "workflow_dispatch")
}

pub(super) async fn resolve(
    pool: &db::DbPool,
    config: &CiConfig,
    context: Option<&crate::backend::cluster::NodeContext>,
    mut request: CiRunRequest,
    rerun: bool,
) -> Result<AdmittedRevision, AdmissionError> {
    if !is_supported_event(&request.event) {
        return Err(AdmissionError::Invalid(
            "CI event is unsupported; use workflow_dispatch for manual execution",
        ));
    }
    let pr_revision = request.event == "pull_request";
    let reference = if pr_revision || !request.branch.starts_with("refs/") {
        format!("refs/heads/{}", request.branch)
    } else {
        request.branch.clone()
    };
    if !is_safe_ref(if pr_revision {
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
    if actor.is_banned || actor.must_change_password {
        return Err(AdmissionError::Denied("CI actor is restricted"));
    }
    if !actor.is_admin && !config.trusted_actor_ids.contains(&actor.id) {
        return Err(AdmissionError::Denied(
            "CI host execution is not approved for this actor",
        ));
    }
    let repo = tokio::time::timeout(
        lookup_timeout,
        db::repositories::records::get_repository_by_id(pool, request.repo_id),
    )
    .await??
    .ok_or(AdmissionError::Unavailable)?;
    if repo.is_archived {
        return Err(AdmissionError::Conflict("CI repository is archived"));
    }
    let access = tokio::time::timeout(
        lookup_timeout,
        db::repositories::access::get_user_repo_access(pool, actor.id, repo.id),
    )
    .await??;
    if !actor.is_admin && !access.can_write() {
        return Err(AdmissionError::Denied(
            "CI repository execution permission denied",
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
        if environment.trim().is_empty()
            || environment.len() > 255
            || environment.chars().any(char::is_control)
        {
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
    if request.pipeline.includes_cd() && !actor.is_admin && !access.can_admin() {
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
            if accepted_push || pr_revision || rerun {
                &reference
            } else {
                &request.branch
            },
            request.head_sha.as_deref(),
            !accepted_push && !pr_revision && !rerun,
        )
        .await?;
    // Inputs belong to one workflow, so a run of several cannot take any.
    if dispatch && request.workflow_paths.len() > 1 && !submitted.is_empty() {
        return Err(AdmissionError::InvalidRequest(
            "workflow inputs need a run of one workflow",
        ));
    }
    let mut blobs = BTreeMap::new();
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
        let (definition, environment) =
            parse_admission_workflow(path.clone(), file.content, request.environment.clone())
                .await?;
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
        // Pull request filters name the base branch itself, even when it looks like a ref.
        let event_ref = if pr_revision {
            request.branch.clone()
        } else {
            commit.full_ref.clone()
        };
        if !definition.matches_event(&request.event, &event_ref) {
            return Err(AdmissionError::Invalid(
                "CI workflow does not support requested event/ref",
            ));
        }
        blobs.insert(path.clone(), file.blob_id);
    }
    if request.pipeline.includes_cd() && request.environment.is_none() {
        request.environment = Some("production".to_owned());
    }
    if !pr_revision {
        request.branch = commit.full_ref;
    }
    request.head_sha = Some(commit.sha);
    request.inputs = dispatch.then_some(run_inputs);
    Ok(AdmittedRevision {
        request,
        blobs: serde_json::to_value(blobs).map_err(|_| AdmissionError::Unavailable)?,
    })
}

async fn parse_admission_workflow(
    path: String,
    content: String,
    mut environment: Option<String>,
) -> Result<(super::workflows::WorkflowDefinition, Option<String>), AdmissionError> {
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
        for job in jobs.values() {
            if let Some(value) = job.get("environment") {
                let name = value
                    .as_str()
                    .or_else(|| value.get("name").and_then(serde_yaml::Value::as_str))
                    .ok_or(AdmissionError::Invalid(
                        "CI dynamic workflow environments are unsupported",
                    ))?;
                if name.is_empty()
                    || name.len() > 255
                    || name.contains("${{")
                    || name.chars().any(char::is_control)
                {
                    return Err(AdmissionError::Invalid(
                        "CI dynamic workflow environments are unsupported",
                    ));
                }
                if let Some(environment) = &environment {
                    if environment != name {
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
        Ok((definition, environment))
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
        && super::push::branch_name(&reference).chars().count() <= 255
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
        assert!(!is_safe_ref(&format!("refs/heads/{}", "a".repeat(256))));
        assert!(!is_safe_ref("refs/heads/a.lock"));
        assert!(!is_safe_owner("a b"));
        assert!(!is_safe_ref("refs/heads/a b"));
        assert!(!is_safe_ref("-option"));
    }

    #[tokio::test]
    async fn admission_parsing_checks_jobs_and_extracts_triggers_and_environment() {
        let path = ".github/workflows/unit-test.yml".to_owned();
        let content = "name: Test\non: workflow_dispatch\njobs:\n  test:\n    environment: staging\n    steps: [{run: echo ok}]\n".to_owned();
        let (definition, environment) =
            parse_admission_workflow(path.clone(), content.clone(), None)
                .await
                .unwrap();
        assert!(definition.triggers.workflow_dispatch);
        assert_eq!(environment.as_deref(), Some("staging"));
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
    async fn admission_waits_for_shared_parsing_capacity() {
        let permits = super::super::workflows::PARSING_SLOTS
            .acquire_many(2)
            .await
            .unwrap();
        let mut parse = Box::pin(parse_admission_workflow(
            ".github/workflows/ci.yml".into(),
            "on: push\njobs: {test: {steps: [{run: echo ok}]}}".into(),
            None,
        ));
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(20), &mut parse)
                .await
                .is_err()
        );
        drop(permits);
        assert!(parse.await.unwrap().0.triggers.push.is_some());
    }

    #[test]
    fn workflow_paths_are_bounded_ascii_regular_file_names() {
        assert!(is_safe_workflow_path(".github/workflows/test.yml"));
        assert!(is_safe_workflow_path(".github/workflows/unit-test.yml"));
        for bad in [
            ".github/workflows/unit test.yml",
            ".github/workflows/テスト.yml",
            ".github/workflows/a..yml",
            ".github/workflows/../test.yml",
            ".github/workflows/nested/test.yml",
            ".github/workflows/a.yml,b.yml",
            ".github/workflows/a.yml ",
        ] {
            assert!(!is_safe_workflow_path(bad));
        }
    }
}
