use super::super::responses::{map_job_info, JobInfo};
use super::{ensure_repo_read_access, missing, CiReadError};
use crate::backend::{
    api::{error::ApiError, session::get_current_user},
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

use crate::backend::api::context::PageContext;
use crate::backend::repositories::store::repo_store;
use crate::webui::pages::actions::{self as view, logs::LogLine};
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

fn map_run(run: db::models::CiRun, repository: &db::models::Repository) -> view::Run {
    view::Run {
        commit_status_context: run.commit_status_context(),
        id: run.run_id.to_string(),
        repository: repository.name.clone(),
        status: run.status,
        workflow_path: run.workflow_path,
        // Runs stored before short branch names were used still have the full ref.
        branch: crate::backend::ci::branch_name(&run.branch).to_owned(),
        event: run.event,
        pipeline: run.pipeline,
        mode: run.mode,
        head_sha: run.head_sha,
        created_at: run.created_at.timestamp(),
        started_at: run.started_at.map(|value| value.timestamp()),
        completed_at: run.completed_at.map(|value| value.timestamp()),
        // Raw executor/driver errors can contain credentials or internal paths.
        error_message: run
            .error_message
            .map(|_| "The runner recorded an error".to_string()),
        // Admission stores these as an object of strings; the page shows none if it cannot read them.
        inputs: run
            .workflow_inputs
            .and_then(|inputs| serde_json::from_value(inputs).ok())
            .unwrap_or_default(),
    }
}

/// A workflow as the Actions page lists it: its inputs only matter when it can be dispatched.
pub(crate) fn workflow_view(workflow: crate::backend::ci::WorkflowDefinition) -> view::Workflow {
    view::Workflow {
        path: workflow.path,
        name: workflow.name,
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
) -> view::WorkflowInput {
    use crate::backend::ci::dispatch_inputs::InputKind;
    view::WorkflowInput {
        name: input.name,
        description: input.description,
        kind: match input.kind {
            InputKind::Text => view::InputKind::Text,
            InputKind::Boolean => view::InputKind::Boolean,
            InputKind::Number => view::InputKind::Number,
            InputKind::Choice => view::InputKind::Choice,
            InputKind::Environment => view::InputKind::Environment,
        },
        required: input.required,
        default: input.default,
        options: input.options,
    }
}

pub(crate) fn bound_log_projection(
    envelope: &mut crate::webui::app::PageEnvelope,
) -> Result<(), serde_json::Error> {
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
) -> Option<&mut view::SelectedRun> {
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
    let mut can_dispatch = false;
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
        let definitions = match async {
            let refs = store.refs().await?;
            // A genuinely empty repository has no workflow tree yet. Do not
            // use a missing-ref error as proof of that condition.
            if refs.is_empty() {
                return Ok(Vec::new());
            }
            store.commit(&dispatch_branch).await?;
            crate::backend::ci::list_workflows_for_ref(&store, &dispatch_branch).await
        }
        .await
        {
            Ok(definitions) => definitions,
            Err(_) => {
                return ctx.error(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "Workflow definitions could not be read from this ref.",
                )
            }
        };
        workflows = definitions.into_iter().map(workflow_view).collect();
        if repository.owner_kind == "user"
            && (user.is_admin || repository.is_personal_owner(user.id))
        {
            can_dispatch = true;
        } else {
            can_dispatch = match db::repositories::access::get_user_repo_access(
                &ctx.state.pool,
                user.id,
                repository.id,
            )
            .await
            {
                Ok(access) => access.can_write(),
                Err(_) => return unavailable(&ctx),
            };
        }
    }
    let persisted_runs = match &repository {
        Some(repository) => {
            db::ci::list_ci_runs_for_repo(&ctx.state.pool, repository.id, run_limit as i64).await
        }
        None => db::ci::list_ci_runs_for_user(&ctx.state.pool, user.id, run_limit as i64).await,
    };
    let persisted_runs = match persisted_runs {
        Ok(runs) => runs,
        Err(_) => return unavailable(&ctx),
    };
    let mut runs = Vec::new();
    // A formerly triggered private run is not a continuing grant after access revocation.
    let mut authorized_repositories: HashMap<i64, Option<db::models::Repository>> = HashMap::new();
    if let Some(repository) = &repository {
        authorized_repositories.insert(repository.id, Some(repository.clone()));
    }
    for run in persisted_runs {
        if let std::collections::hash_map::Entry::Vacant(entry) =
            authorized_repositories.entry(run.repo_id)
        {
            let repo =
                match db::repositories::records::get_repository_by_id(&ctx.state.pool, run.repo_id)
                    .await
                {
                    Ok(repo) => repo,
                    Err(_) => return unavailable(&ctx),
                };
            let authorized = if let Some(repo) = repo {
                match super::ensure_repo_read_access(&ctx.state, user, &repo).await {
                    Ok(()) => Some(repo),
                    Err((StatusCode::NOT_FOUND, _)) | Err((StatusCode::FORBIDDEN, _)) => None,
                    Err((status, _)) => return read_error(&ctx, status),
                }
            } else {
                None
            };
            entry.insert(authorized);
        }
        if let Some(Some(repo)) = authorized_repositories.get(&run.repo_id) {
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
                workflows.push(view::Workflow {
                    path: path.to_string(),
                    name: view::workflow_name(path),
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
        let persisted_logs =
            match db::ci::list_ci_logs(&ctx.state.pool, run.id, log_limit as i64).await {
                Ok(lines) => lines,
                Err(_) => return unavailable(&ctx),
            };
        let fetched_lines = persisted_logs.len();
        let logs = persisted_logs
            .into_iter()
            .filter(|line| !crate::backend::ci::logs::is_internal_runner_log_line(&line.line))
            .map(|line| LogLine {
                id: line.id,
                stage: crate::backend::ci::logs::normalize_stage_name(&line.stage),
                line: line.line,
                level: line.level,
                timestamp: line.created_at.timestamp(),
            })
            .collect();
        let deployment = match db::ci::get_ci_deployment_by_run_id(&ctx.state.pool, run.id).await {
            Ok(Some(deployment)) if deployment.repo_id == run.repo_id => Some(view::Deployment {
                environment: deployment.environment,
                status: deployment.status,
                approved_at: deployment.approved_at.map(|value| value.timestamp()),
                started_at: deployment.started_at.map(|value| value.timestamp()),
                completed_at: deployment.completed_at.map(|value| value.timestamp()),
            }),
            Ok(None) => None,
            _ => return unavailable(&ctx),
        };
        Some(view::SelectedRun {
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
        match crate::backend::api::repos::chrome::load_chrome(&ctx, owner, name).await {
            Ok(chrome) => Some(chrome),
            Err(response) => return response,
        }
    } else {
        None
    };
    let model = view::ActionsPage {
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
        can_dispatch,
        run_limit,
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
    fn selected_run_cannot_cross_repository_scope() {
        assert!(run_matches_repository(9, Some(9)));
        assert!(!run_matches_repository(9, Some(10)));
        assert!(run_matches_repository(9, None));
    }
}
