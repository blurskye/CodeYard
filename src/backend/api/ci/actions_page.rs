use super::responses::{map_run, workflow_view};
use crate::backend::api::http::context::PageContext;
use crate::backend::repositories::store::repo_store;
use crate::backend::{app::AppState, db};
use crate::webui::pages::ci as view;
use crate::webui::pages::repos::actions::{self as view_actions, logs::LogLine};
use axum::{
    extract::{OriginalUri, Path, State},
    http::{HeaderMap, StatusCode, Uri},
    response::Response,
};
use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

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
            super::jobs::ensure_repo_read_access(&ctx.state, user, &repository).await
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
            if let Err((status, _)) =
                super::jobs::ensure_repo_read_access(&ctx.state, user, &repo).await
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
            super::jobs::ensure_repo_read_access(&ctx.state, user, &run_repository).await
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
    fn selected_run_cannot_cross_repository_scope() {
        assert!(run_matches_repository(9, Some(9)));
        assert!(!run_matches_repository(9, Some(10)));
        assert!(run_matches_repository(9, None));
    }
}
