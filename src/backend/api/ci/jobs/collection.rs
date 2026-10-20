use super::super::responses::{map_job_info, JobInfo};
use super::{ensure_repo_read_access, missing, CiReadError};
use crate::backend::{
    api::{http::error::ApiError, http::session::get_current_user},
    app::AppState,
    db,
};
use axum::{
    extract::{OriginalUri, Path, Query, State},
    http::{HeaderMap, StatusCode},
    response::Response,
    Json,
};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

#[derive(Debug, Deserialize)]
pub struct ListJobsQuery {
    pub owner: Option<String>,
    pub repo: Option<String>,
    pub limit: Option<i64>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ListJobsResponse {
    pub jobs: Vec<JobInfo>,
}

pub async fn list_ci_jobs(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Query(query): Query<ListJobsQuery>,
) -> Result<Json<ListJobsResponse>, CiReadError> {
    let user = get_current_user(&state.pool, &headers).await?;
    let limit = query.limit.unwrap_or(50).clamp(1, 200);
    if query.owner.is_some() != query.repo.is_some() {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(ApiError::new(
                "validation_error",
                "owner and repo must be supplied together",
            )),
        ));
    }
    let runs = if let (Some(owner), Some(repo)) = (query.owner.as_ref(), query.repo.as_ref()) {
        let repo = db::repositories::records::get_repository(&state.pool, owner, repo)
            .await
            .map_err(super::unavailable)?
            .ok_or_else(missing)?;
        ensure_repo_read_access(&state, &user, &repo).await?;
        db::ci::list_ci_runs_for_repo(&state.pool, repo.id, limit)
            .await
            .map_err(super::unavailable)?
    } else {
        // Current repository visibility is in SQL, BEFORE LIMIT; historical
        // triggering is not continuing private-resource authority.
        db::ci::list_ci_runs_for_user(&state.pool, user.id, limit)
            .await
            .map_err(super::unavailable)?
    };
    Ok(Json(ListJobsResponse {
        jobs: runs.into_iter().map(map_job_info).collect(),
    }))
}

use crate::backend::api::http::context::PageContext;
use crate::backend::repositories::store::repo_store;
use crate::webui::pages::ci as view;
use crate::webui::pages::repos::actions::{self as view_actions, logs::LogLine};
use axum::http::Uri;
use std::collections::{BTreeMap, HashMap};
pub(crate) async fn global_actions(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    OriginalUri(uri): OriginalUri,
) -> Response {
    actions(state, headers, uri, None).await
}

pub(crate) async fn repository_actions(
    State(state): State<Arc<AppState>>,
    Path((owner, repo)): Path<(String, String)>,
    headers: HeaderMap,
    OriginalUri(uri): OriginalUri,
) -> Response {
    actions(state, headers, uri, Some((owner, repo))).await
}

fn unavailable(ctx: &PageContext) -> Response {
    ctx.error(
        StatusCode::SERVICE_UNAVAILABLE,
        "Actions data is temporarily unavailable. Please try again.",
    )
}

fn read_error(ctx: &PageContext, status: StatusCode) -> Response {
    if status.is_server_error() {
        unavailable(ctx)
    } else {
        ctx.error(
            status,
            "The requested repository or run is not available to your account.",
        )
    }
}

#[expect(
    clippy::result_large_err,
    reason = "HTML adapters preserve complete Axum responses under the PageContext contract."
)]
fn query_model(ctx: &PageContext) -> Result<BTreeMap<String, String>, Response> {
    let mut query = BTreeMap::new();
    for key in [
        "job",
        "view",
        "workflow_job",
        "workflow",
        "event",
        "status",
        "branch",
        "q",
        "log_q",
        "wrap",
        "limit",
        "log_limit",
        "ref",
        "actor",
        "before",
    ] {
        if let Some(value) = ctx.query.get(key).filter(|value| !value.is_empty()) {
            if value.len() > 500 || value.contains(['\r', '\n', '\0']) {
                return Err(ctx.error(
                    StatusCode::BAD_REQUEST,
                    "Actions filter is too long or contains unsupported characters.",
                ));
            }
            query.insert(key.to_string(), value.clone());
        }
    }
    Ok(query)
}

#[expect(
    clippy::result_large_err,
    reason = "HTML adapters preserve complete Axum responses under the PageContext contract."
)]
fn bounded_limit(
    ctx: &PageContext,
    key: &str,
    default: usize,
    max: usize,
) -> Result<usize, Response> {
    match ctx.query.get(key).filter(|value| !value.is_empty()) {
        None => Ok(default),
        Some(value) => value
            .parse::<usize>()
            .map(|value| value.clamp(1, max))
            .map_err(|_| {
                ctx.error(
                    StatusCode::BAD_REQUEST,
                    "Run/log limit must be a positive number.",
                )
            }),
    }
}

pub(crate) fn map_run(
    run: db::models::CiRun,
    repository: &db::models::Repository,
) -> view_actions::Run {
    // A pull request run is stored on its base branch, which admission uses. The page shows the head
    // branch of the pull request, which the run kept in its event.
    let head_branch = run
        .pull_request_event
        .as_ref()
        .filter(|_| run.event == "pull_request")
        .and_then(|event| event["head_ref"].as_str());
    // Runs stored before short branch names were used still have the full ref.
    let branch = head_branch
        .unwrap_or_else(|| {
            if run.event == "pull_request" {
                &run.branch
            } else {
                crate::backend::ci::branch_name(&run.branch)
            }
        })
        .to_owned();
    view_actions::Run {
        commit_status_context: run.commit_status_context(),
        id: run.run_id.to_string(),
        repository: repository.name.clone(),
        status: run.status,
        workflow_path: run.workflow_path,
        branch,
        event: run.event,
        pipeline: run.pipeline,
        mode: run.mode,
        head_sha: run.head_sha,
        created_at: run.created_at.timestamp(),
        started_at: run.started_at.map(|value| value.timestamp()),
        completed_at: run.completed_at.map(|value| value.timestamp()),
        error_message: crate::backend::ci::visible_run_error(run.error_message.as_deref()),
        // Admission stores these as an object of strings; the page shows none if it cannot read them.
        inputs: run
            .workflow_inputs
            .and_then(|inputs| serde_json::from_value(inputs).ok())
            .unwrap_or_default(),
    }
}

/// A workflow as the Actions page lists it: its inputs only matter when it can be dispatched.
pub(crate) fn workflow_view(
    workflow: crate::backend::ci::WorkflowDefinition,
) -> view_actions::Workflow {
    view_actions::Workflow {
        path: workflow.path,
        name: workflow.name.chars().take(256).collect(),
        dispatchable: workflow.triggers.workflow_dispatch,
        inputs: workflow
            .triggers
            .dispatch_inputs
            .into_iter()
            .map(workflow_input_view)
            .collect(),
    }
}

fn workflow_input_view(
    input: crate::backend::ci::dispatch_inputs::DispatchInput,
) -> view_actions::WorkflowInput {
    use crate::backend::ci::dispatch_inputs::InputKind;
    view_actions::WorkflowInput {
        name: input.name,
        description: input.description.chars().take(1024).collect(),
        kind: match input.kind {
            InputKind::Text => view_actions::InputKind::Text,
            InputKind::Boolean => view_actions::InputKind::Boolean,
            InputKind::Number => view_actions::InputKind::Number,
            InputKind::Choice => view_actions::InputKind::Choice,
            InputKind::Environment => view_actions::InputKind::Environment,
        },
        required: input.required,
        default: input.default,
        options: input.options,
    }
}

pub(crate) fn bound_log_projection(
    envelope: &mut crate::webui::app::PageEnvelope,
) -> Result<(), serde_json::Error> {
    if let crate::webui::app::Page::Actions(view::Page::Actions(model)) = &mut envelope.page {
        let mut bytes = 0usize;
        for workflow in &mut model.workflows {
            let size = serde_json::to_vec(workflow)?.len();
            if bytes.saturating_add(size) > 2 * 1024 * 1024 {
                workflow.inputs.clear();
                workflow.dispatchable = false;
                model.workflow_warning = Some("Some workflow inputs exceed the page size limit. Use the API to dispatch those workflows.".into());
                bytes += serde_json::to_vec(workflow)?.len();
            } else {
                bytes += size;
            }
        }
    }
    let Some(selected) = selected_run_mut(envelope) else {
        return Ok(());
    };
    let logs = std::mem::take(&mut selected.logs);
    // Count the actual JSON envelope, not raw log bytes: escaping can expand a line.
    let mut bytes = serde_json::to_vec(envelope)?.len();
    let selected = selected_run_mut(envelope).expect("selected run");
    // Keep the newest lines that fit; the browser refuses larger page models.
    for line in logs.into_iter().rev() {
        let size = serde_json::to_vec(&line)?.len() + usize::from(!selected.logs.is_empty());
        if bytes.saturating_add(size) > crate::webui::bootstrap::MAX_PAGE_MODEL_BYTES {
            selected.logs_truncated = true;
            break;
        }
        bytes += size;
        selected.logs.push(line);
    }
    selected.logs.reverse();
    Ok(())
}

fn selected_run_mut(
    envelope: &mut crate::webui::app::PageEnvelope,
) -> Option<&mut view_actions::SelectedRun> {
    match &mut envelope.page {
        crate::webui::app::Page::Actions(view::Page::Actions(model)) => model.selected.as_mut(),
        _ => None,
    }
}

fn run_matches_repository(run_repo_id: i64, scoped_repo_id: Option<i64>) -> bool {
    scoped_repo_id.is_none_or(|repo_id| repo_id == run_repo_id)
}

async fn actions(
    state: Arc<AppState>,
    headers: HeaderMap,
    uri: Uri,
    scope: Option<(String, String)>,
) -> Response {
    let ctx = match PageContext::load(state, headers, &uri).await {
        Ok(ctx) => ctx,
        Err(response) => return response,
    };
    let user = match ctx.require_user() {
        Ok(user) => user,
        Err(response) => return response,
    };
    let query = match query_model(&ctx) {
        Ok(query) => query,
        Err(response) => return response,
    };
    let run_limit = match bounded_limit(&ctx, "limit", 100, 200) {
        Ok(limit) => limit,
        Err(response) => return response,
    };
    let log_limit = match bounded_limit(&ctx, "log_limit", 2000, 5000) {
        Ok(limit) => limit,
        Err(response) => return response,
    };
    let repository = if let Some((owner, repo)) = &scope {
        let repository =
            match db::repositories::records::get_repository(&ctx.state.pool, owner, repo).await {
                Ok(Some(repository)) => repository,
                Ok(None) => return ctx.error(StatusCode::NOT_FOUND, "Repository not found."),
                Err(_) => return unavailable(&ctx),
            };
        if let Err((status, _)) =
            super::ensure_repo_read_access(&ctx.state, user, &repository).await
        {
            return read_error(&ctx, status);
        }
        Some(repository)
    } else {
        None
    };
    // A global selection uses the repository's native action routes after authorization.
    if repository.is_none() {
        if let Some(job) = query.get("job") {
            let run_id = match uuid::Uuid::parse_str(job) {
                Ok(id) => id,
                Err(_) => {
                    return ctx.error(StatusCode::BAD_REQUEST, "Run identifier must be a UUID.")
                }
            };
            let run = match db::ci::get_ci_run_by_run_id(&ctx.state.pool, run_id).await {
                Ok(Some(run)) => run,
                Ok(None) => return ctx.error(StatusCode::NOT_FOUND, "Run not found."),
                Err(_) => return unavailable(&ctx),
            };
            let repo =
                match db::repositories::records::get_repository_by_id(&ctx.state.pool, run.repo_id)
                    .await
                {
                    Ok(Some(repo)) => repo,
                    Ok(None) => return ctx.error(StatusCode::NOT_FOUND, "Repository not found."),
                    Err(_) => return unavailable(&ctx),
                };
            if let Err((status, _)) = super::ensure_repo_read_access(&ctx.state, user, &repo).await
            {
                return read_error(&ctx, status);
            }
            let owner =
                match db::repositories::records::repository_owner_name(&ctx.state.pool, repo.id)
                    .await
                {
                    Ok(Some(owner)) => owner,
                    _ => return unavailable(&ctx),
                };
            let path = format!(
                "/{}/{}/actions",
                crate::webui::encode_component(&owner),
                crate::webui::encode_component(&repo.name)
            );
            return ctx.redirect(&view_actions::query_url(&path, &query, &[]));
        }
    }
    let (base_path, title) = scope
        .as_ref()
        .map(|(owner, repo)| {
            (
                format!(
                    "/{}/{}/actions",
                    crate::webui::encode_component(owner),
                    crate::webui::encode_component(repo)
                ),
                format!("{owner}/{repo} · Actions"),
            )
        })
        .unwrap_or_else(|| ("/ci".into(), "CI/CD runs".into()));
    let mut workflows = Vec::new();
    let mut can_manage_runs = false;
    let mut can_cancel = false;
    let mut workflow_warning = None;
    let mut dispatch_branch = "main".to_string();
    if let Some(repository) = &repository {
        dispatch_branch = query
            .get("ref")
            .cloned()
            .unwrap_or_else(|| repository.default_branch.clone());
        if !crate::backend::ci::admission::is_safe_ref(&dispatch_branch) {
            return ctx.error(StatusCode::BAD_REQUEST, "Invalid workflow ref.");
        }
        let store = repo_store(repository, ctx.state.cluster.as_ref());
        let definitions = match tokio::time::timeout(std::time::Duration::from_secs(5), async {
            let refs = store.refs().await?;
            // A genuinely empty repository has no workflow tree yet. Do not
            // use a missing-ref error as proof of that condition.
            if refs.is_empty() {
                return Ok(Vec::new());
            }
            store.commit(&dispatch_branch).await?;
            crate::backend::ci::list_workflows_for_ref(&store, &dispatch_branch).await
        })
        .await
        {
            Ok(Ok(definitions)) => definitions,
            _ => {
                workflow_warning = Some("Workflow definitions could not be read from this ref. Run history and logs are still available; workflow dispatch is disabled.".to_owned());
                Vec::new()
            }
        };
        workflows = definitions.into_iter().map(workflow_view).collect();
        can_manage_runs = match ctx
            .state
            .ci_runs
            .actor_may_discover_workflows(repository.id, user.id)
            .await
        {
            Ok(allowed) => allowed,
            Err(_) => return unavailable(&ctx),
        };
    }
    let before = match query.get("before").map(|value| value.parse::<i64>()) {
        None => 0,
        Some(Ok(value)) if value > 0 => value,
        _ => return ctx.error(StatusCode::BAD_REQUEST, "Invalid run cursor."),
    };
    let value = |key: &str| query.get(key).map(String::as_str).unwrap_or("");
    let status = if value("status").is_empty() {
        ""
    } else {
        view_actions::logs::normalize_status(value("status"))
    };
    let filters = db::ci::CiRunFilter {
        workflow: value("workflow").to_owned(),
        event: value("event").to_owned(),
        status: status.to_owned(),
        branch: value("branch").to_owned(),
        actor: value("actor").to_owned(),
        text: value("q").to_owned(),
        before: (before > 0).then_some(before),
    };
    let mut persisted_runs = match db::ci::list_filtered_ci_runs(
        &ctx.state.pool,
        user.id,
        repository.as_ref().map(|repo| repo.id),
        &filters,
        run_limit as i64 + 1,
    )
    .await
    {
        Ok(runs) => runs,
        Err(_) => return unavailable(&ctx),
    };
    let next_before = if persisted_runs.len() > run_limit {
        persisted_runs.truncate(run_limit);
        persisted_runs.last().map(|run| run.id)
    } else {
        None
    };
    let mut runs = Vec::new();
    // Recheck current visibility for the bounded set in one database snapshot.
    let authorized_repositories: HashMap<_, _> = if let Some(repository) = &repository {
        HashMap::from([(repository.id, repository.clone())])
    } else {
        let ids: Vec<_> = persisted_runs
            .iter()
            .map(|run| run.repo_id)
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter()
            .collect();
        match db::repositories::access::readable_repositories_by_ids(&ctx.state.pool, user.id, &ids)
            .await
        {
            Ok(repositories) => repositories
                .into_iter()
                .map(|repo| (repo.id, repo))
                .collect(),
            Err(_) => return unavailable(&ctx),
        }
    };
    for run in persisted_runs {
        if let Some(repo) = authorized_repositories.get(&run.repo_id) {
            runs.push(map_run(run, repo));
        }
    }
    // Retain historical workflow navigation even when the file was removed from this ref.
    for run in &runs {
        for path in run
            .workflow_path
            .split(',')
            .map(str::trim)
            .filter(|path| !path.is_empty())
        {
            if !workflows.iter().any(|workflow| workflow.path == path) {
                workflows.push(view_actions::Workflow {
                    path: path.to_string(),
                    name: view_actions::workflow_name(path),
                    dispatchable: false,
                    inputs: Vec::new(),
                });
            }
        }
    }
    let selected = if let Some(job) = query.get("job") {
        let run_id = match uuid::Uuid::parse_str(job) {
            Ok(id) => id,
            Err(_) => return ctx.error(StatusCode::BAD_REQUEST, "Run identifier must be a UUID."),
        };
        let run = match db::ci::get_ci_run_by_run_id(&ctx.state.pool, run_id).await {
            Ok(Some(run)) => run,
            Ok(None) => return ctx.error(StatusCode::NOT_FOUND, "Run not found."),
            Err(_) => return unavailable(&ctx),
        };
        if !run_matches_repository(run.repo_id, repository.as_ref().map(|repo| repo.id)) {
            return ctx.error(StatusCode::NOT_FOUND, "Run not found in this repository.");
        }
        let run_repository =
            match db::repositories::records::get_repository_by_id(&ctx.state.pool, run.repo_id)
                .await
            {
                Ok(Some(repo)) => repo,
                Ok(None) => return ctx.error(StatusCode::NOT_FOUND, "Repository not found."),
                Err(_) => return unavailable(&ctx),
            };
        if let Err((status, _)) =
            super::ensure_repo_read_access(&ctx.state, user, &run_repository).await
        {
            return read_error(&ctx, status);
        }
        let access = match db::repositories::access::get_user_repo_access(
            &ctx.state.pool,
            user.id,
            run_repository.id,
        )
        .await
        {
            Ok(access) => access,
            Err(_) => return unavailable(&ctx),
        };
        let personal_admin = user.is_admin
            && run_repository.owner_kind == "user"
            && run_repository.owner_org_id.is_none();
        can_cancel = !user.is_banned
            && !user.must_change_password
            && db::ci::may_cancel_run(user.id, run.triggered_by_id, personal_admin, access);
        if run.pipeline != "ci" || run.environment.is_some() {
            can_manage_runs &= personal_admin || access.can_admin();
        }
        let persisted_logs =
            match db::ci::list_ci_logs(&ctx.state.pool, run.id, log_limit as i64).await {
                Ok(lines) => lines,
                Err(_) => return unavailable(&ctx),
            };
        let fetched_lines = persisted_logs.len();
        let logs = persisted_logs
            .into_iter()
            .map(|line| LogLine {
                id: line.id,
                stage: crate::backend::ci::logs::normalize_stage_name(&line.stage),
                line: line.line,
                level: line.level,
                timestamp: line.created_at.timestamp(),
            })
            .collect();
        let deployment = match db::ci::get_ci_deployment_by_run_id(&ctx.state.pool, run.id).await {
            Ok(Some(deployment)) if deployment.repo_id == run.repo_id => {
                Some(view_actions::Deployment {
                    environment: deployment.environment,
                    status: deployment.status,
                    approved_at: deployment.approved_at.map(|value| value.timestamp()),
                    started_at: deployment.started_at.map(|value| value.timestamp()),
                    completed_at: deployment.completed_at.map(|value| value.timestamp()),
                })
            }
            Ok(None) => None,
            _ => return unavailable(&ctx),
        };
        Some(view_actions::SelectedRun {
            run: map_run(run, &run_repository),
            logs,
            deployment,
            fetched_lines,
            logs_truncated: false,
        })
    } else {
        None
    };
    // The path only identifies the scope; the shared loader independently checks
    // read access before projecting engagement/visibility metadata for the browser.
    let repo_chrome = if let Some((owner, name)) = &scope {
        match crate::backend::api::repos::chrome::load_database(&ctx, owner, name).await {
            Ok(loaded) => Some(loaded.chrome),
            Err(response) => return response,
        }
    } else {
        None
    };
    let model = view_actions::ActionsPage {
        repository: scope,
        repo_chrome,
        title: title.clone(),
        base_path,
        csrf_token: ctx.csrf_token.clone(),
        query,
        runs,
        workflows,
        selected,
        dispatch_branch,
        can_dispatch: can_manage_runs && workflow_warning.is_none(),
        can_manage_runs,
        can_cancel,
        workflow_warning,
        run_limit,
        next_before,
        log_limit,
    };
    ctx.page_model(
        &title,
        crate::webui::app::Page::Actions(view::Page::Actions(Box::new(model))),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn workflow_metadata_fits_the_model_budget_without_a_selected_run() {
        use crate::backend::ci::{
            dispatch_inputs::{DispatchInput, InputKind},
            workflows::{WorkflowDefinition, WorkflowTriggers},
        };
        let workflow = workflow_view(WorkflowDefinition {
            path: ".github/workflows/ci.yml".into(),
            name: "quote".repeat(100_000),
            triggers: WorkflowTriggers {
                workflow_dispatch: true,
                dispatch_inputs: vec![
                    DispatchInput {
                        name: "target".into(),
                        description: "\"".repeat(200_000),
                        kind: InputKind::Choice,
                        required: false,
                        default: None,
                        options: vec!["\"".repeat(2048); 25]
                    };
                    10
                ],
                ..Default::default()
            },
        });
        assert!(workflow.inputs[0].description.len() <= 1024);
        let model = view_actions::ActionsPage {
            repository: None,
            repo_chrome: None,
            title: "Actions".into(),
            base_path: "/ci".into(),
            csrf_token: "token".into(),
            query: BTreeMap::new(),
            runs: vec![],
            workflows: vec![workflow; 64],
            selected: None,
            dispatch_branch: "main".into(),
            can_dispatch: true,
            can_manage_runs: true,
            can_cancel: false,
            workflow_warning: None,
            run_limit: 100,
            next_before: None,
            log_limit: 2000,
        };
        let mut envelope = crate::webui::app::PageEnvelope {
            title: "Actions".into(),
            location: "/ci".into(),
            chrome: Default::default(),
            page: crate::webui::app::Page::Actions(view::Page::Actions(Box::new(model))),
        };
        bound_log_projection(&mut envelope).unwrap();
        assert!(
            serde_json::to_vec(&envelope).unwrap().len()
                < crate::webui::bootstrap::MAX_PAGE_MODEL_BYTES
        );
        let crate::webui::app::Page::Actions(view::Page::Actions(model)) = envelope.page else {
            panic!("Actions")
        };
        assert!(model.workflow_warning.is_some());
        assert!(model
            .workflows
            .iter()
            .any(|workflow| !workflow.dispatchable && workflow.inputs.is_empty()));
    }

    #[test]
    fn selected_run_cannot_cross_repository_scope() {
        assert!(run_matches_repository(9, Some(9)));
        assert!(!run_matches_repository(9, Some(10)));
        assert!(run_matches_repository(9, None));
    }
}
