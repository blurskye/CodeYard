//! Native forms on a run's page: cancel a queued or running run, or run it again at the same
//! commit. Both need write access to the repository, and both check the CSRF token. The run must
//! belong to the repository named in the URL.
use crate::backend::{
    api::http::context::PageContext, app::AppState, ci::runs::DispatchRequest, db,
    repositories::access,
};
use axum::{
    extract::{Form, OriginalUri, Path, State},
    http::{HeaderMap, StatusCode},
    response::Response,
};
use std::{collections::HashMap, sync::Arc};
use uuid::Uuid;

/// The repository, and the run in it, when the signed-in viewer may write to the repository.
async fn writable_run(
    ctx: &PageContext,
    owner: &str,
    name: &str,
    run_id: &str,
) -> Result<(db::models::Repository, db::models::CiRun), Response> {
    let user = ctx.require_user()?;
    let run_id = Uuid::parse_str(run_id)
        .map_err(|_| ctx.error(StatusCode::BAD_REQUEST, "Run identifier must be a UUID."))?;
    let repository = db::repositories::records::get_repository(&ctx.state.pool, owner, name)
        .await
        .ok()
        .flatten()
        .ok_or_else(|| ctx.error(StatusCode::NOT_FOUND, "Repository not found."))?;
    let run = db::ci::get_ci_run_by_run_id(&ctx.state.pool, run_id)
        .await
        .ok()
        .flatten()
        .filter(|run| run.repo_id == repository.id)
        .ok_or_else(|| ctx.error(StatusCode::NOT_FOUND, "Run not found in this repository."))?;
    let allowed = access::can_write(&ctx.state.pool, user, &repository)
        .await
        .map_err(|_| {
            ctx.error(
                StatusCode::SERVICE_UNAVAILABLE,
                "Actions data is temporarily unavailable. Please try again.",
            )
        })?;
    if !allowed {
        return Err(ctx.error(StatusCode::FORBIDDEN, "Write access is required."));
    }
    Ok((repository, run))
}

fn run_page(owner: &str, name: &str, run_id: Uuid) -> String {
    format!(
        "/{}/{}/actions?job={run_id}",
        crate::webui::encode_component(owner),
        crate::webui::encode_component(name)
    )
}

pub(crate) async fn cancel_run(
    State(state): State<Arc<AppState>>,
    Path((owner, name, run_id)): Path<(String, String, String)>,
    headers: HeaderMap,
    OriginalUri(uri): OriginalUri,
    Form(form): Form<HashMap<String, String>>,
) -> Response {
    let ctx = match PageContext::load(state, headers, &uri).await {
        Ok(ctx) => ctx,
        Err(response) => return response,
    };
    if let Err(response) = ctx.check_csrf(&form) {
        return response;
    }
    let (_, run) = match writable_run(&ctx, &owner, &name, &run_id).await {
        Ok(found) => found,
        Err(response) => return response,
    };
    let actor = ctx
        .require_user()
        .expect("writable_run checked the user")
        .id;
    match ctx.state.ci_runs.cancel_run(run.run_id, actor).await {
        Ok(_) => ctx.redirect(&run_page(&owner, &name, run.run_id)),
        Err(error) => {
            let (status, axum::Json(error)) = super::admission_error(error);
            ctx.error(status, &error.message)
        }
    }
}

/// Queues the same workflow again at the same commit, with the inputs the run was given.
/// Pull request runs are not run again yet: their pull request link is not part of the request.
pub(crate) async fn rerun_run(
    State(state): State<Arc<AppState>>,
    Path((owner, name, run_id)): Path<(String, String, String)>,
    headers: HeaderMap,
    OriginalUri(uri): OriginalUri,
    Form(form): Form<HashMap<String, String>>,
) -> Response {
    let ctx = match PageContext::load(state, headers, &uri).await {
        Ok(ctx) => ctx,
        Err(response) => return response,
    };
    if let Err(response) = ctx.check_csrf(&form) {
        return response;
    }
    let (_, run) = match writable_run(&ctx, &owner, &name, &run_id).await {
        Ok(found) => found,
        Err(response) => return response,
    };
    let Some(head_sha) = run.head_sha.clone() else {
        return ctx.error(StatusCode::CONFLICT, "This run has no commit to run again.");
    };
    if run.event == "pull_request" {
        return ctx.error(
            StatusCode::CONFLICT,
            "Pull request runs cannot be run again yet. Push a new commit or open the pull request again.",
        );
    }
    let inputs = run.workflow_inputs.as_ref().and_then(|value| {
        serde_json::from_value::<std::collections::BTreeMap<String, serde_json::Value>>(
            value.clone(),
        )
        .ok()
    });
    // A run of several workflows is run again as its pipeline, which picks up the same set.
    let workflow_path = (!run.workflow_path.contains(',')).then(|| run.workflow_path.clone());
    let actor = ctx
        .require_user()
        .expect("writable_run checked the user")
        .id;
    let request = DispatchRequest {
        repo_owner: owner.clone(),
        repo_name: name.clone(),
        branch: run.branch.clone(),
        event: run.event.clone(),
        head_sha: Some(head_sha),
        workflow_path,
        pipeline: Some(run.pipeline.clone()),
        environment: run.environment.clone(),
        inputs,
    };
    match ctx.state.ci_runs.dispatch_run(actor, request).await {
        Ok(new_run) => ctx.redirect(&run_page(&owner, &name, new_run.run_id)),
        Err(error) => {
            let (status, axum::Json(error)) = super::admission_error(error);
            ctx.error(status, &error.message)
        }
    }
}
