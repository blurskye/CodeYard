//! Native forms on a run's page: cancel a queued or running run, or run it again at the same
//! commit. Both check CSRF and use the run service's action-specific authority checks. The run
//! must belong to the readable repository named in the URL.
use crate::backend::{api::http::context::PageContext, app::AppState, db, repositories::access};
use axum::{
    extract::{Form, OriginalUri, Path, State},
    http::{HeaderMap, StatusCode},
    response::Response,
};
use std::{collections::HashMap, sync::Arc};
use uuid::Uuid;

/// Scope the run before applying its action-specific authorization.
async fn readable_run(
    ctx: &mut PageContext,
    owner: &str,
    name: &str,
    run_id: &str,
) -> Result<(db::models::Repository, db::models::CiRun), Response> {
    ctx.return_to = Some(Uuid::parse_str(run_id).map(|id| run_page(owner, name, id)).unwrap_or_else(|_| format!("/{}/{}/actions", crate::webui::encode_component(owner), crate::webui::encode_component(name))));
    let user = ctx.require_user()?;
    let run_id = Uuid::parse_str(run_id)
        .map_err(|_| ctx.error(StatusCode::BAD_REQUEST, "Run identifier must be a UUID."))?;
    let unavailable = || {
        ctx.error(
            StatusCode::SERVICE_UNAVAILABLE,
            "Actions data is temporarily unavailable.",
        )
    };
    let repository = db::repositories::records::get_repository(&ctx.state.pool, owner, name)
        .await
        .map_err(|_| unavailable())?
        .ok_or_else(|| ctx.error(StatusCode::NOT_FOUND, "Repository not found."))?;
    if !access::can_read(&ctx.state.pool, Some(user), &repository)
        .await
        .map_err(|_| unavailable())?
    {
        return Err(ctx.error(StatusCode::NOT_FOUND, "Repository not found."));
    }
    let run = db::ci::get_ci_run_by_run_id(&ctx.state.pool, run_id)
        .await
        .map_err(|_| unavailable())?
        .filter(|run| run.repo_id == repository.id)
        .ok_or_else(|| ctx.error(StatusCode::NOT_FOUND, "Run not found in this repository."))?;
    Ok((repository, run))
}

pub(crate) async fn download_artifact(
    State(state): State<Arc<AppState>>,
    Path((owner, name, run_id, artifact)): Path<(String, String, String, i64)>,
    headers: HeaderMap,
    OriginalUri(uri): OriginalUri,
) -> Response {
    use axum::response::IntoResponse;
    let mut ctx = match PageContext::load(state.clone(), headers.clone(), &uri).await {
        Ok(ctx) => ctx,
        Err(response) => return response,
    };
    let (_, run) = match readable_run(&mut ctx, &owner, &name, &run_id).await {
        Ok(run) => run,
        Err(response) => return response,
    };
    let Some(session) = crate::backend::api::http::session::extract_session_id(&ctx.headers) else {
        return ctx.error(StatusCode::UNAUTHORIZED, "A live session is required.");
    };
    let actor = ctx.user.as_ref().expect("readable run").id;
    let object = match db::action_objects::artifact_for_reader(&state.pool, actor, &session, run.repo_id, run.id, artifact).await {
        Ok(Some(object)) => object,
        Ok(_) => return ctx.error(StatusCode::NOT_FOUND, "Artifact not found."),
        Err(_) => return ctx.error(StatusCode::SERVICE_UNAVAILABLE, "Artifacts unavailable."),
    };
    match super::results::download(state, object, &headers).await {
        Ok(response) => response,
        Err(error) => error.into_response(),
    }
}

pub(crate) async fn delete_artifact(
    State(state): State<Arc<AppState>>,
    Path((owner, name, run_id, artifact)): Path<(String, String, String, i64)>,
    headers: HeaderMap,
    OriginalUri(uri): OriginalUri,
    Form(form): Form<HashMap<String, String>>,
) -> Response {
    let mut ctx = match PageContext::load(state, headers, &uri).await {
        Ok(ctx) => ctx,
        Err(response) => return response,
    };
    let (_, run) = match readable_run(&mut ctx, &owner, &name, &run_id).await {
        Ok(run) => run,
        Err(response) => return response,
    };
    if let Err(response) = ctx.check_csrf(&form) { return response; }
    let actor = ctx.user.as_ref().expect("readable run").id;
    let credential = crate::backend::api::http::session::extract_session_id(&ctx.headers).map(db::ci::CiCredential::Session);
    match db::action_objects::delete_artifact(&ctx.state.pool, artifact, run.id, Some(actor), credential.as_ref()).await
    {
        Ok(true) => ctx.redirect(&run_page(&owner, &name, run.run_id)),
        Ok(false) => ctx.error(
            StatusCode::FORBIDDEN,
            "Repository write access is required.",
        ),
        Err(_) => ctx.error(StatusCode::SERVICE_UNAVAILABLE, "Artifacts unavailable."),
    }
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
    let mut ctx = match PageContext::load(state, headers, &uri).await {
        Ok(ctx) => ctx,
        Err(response) => return response,
    };
    let (_, run) = match readable_run(&mut ctx, &owner, &name, &run_id).await {
        Ok(found) => found,
        Err(response) => return response,
    };
    if let Err(response) = ctx.check_csrf(&form) { return response; }
    let actor = ctx
        .require_user()
        .expect("readable_run checked the user")
        .id;
    match ctx.state.ci_runs.cancel_run(run.run_id, actor, crate::backend::api::http::session::extract_session_id(&ctx.headers).map(db::ci::CiCredential::Session)).await {
        Ok(_) => ctx.redirect(&run_page(&owner, &name, run.run_id)),
        Err(error) => {
            let (status, axum::Json(error)) = super::admission_error(error);
            ctx.error(status, &error.message)
        }
    }
}

/// Queues the same workflow again at the same commit, with the inputs the run was given. A pull
/// request run starts a new attempt at its pull request revision, admitted as the pull request is
/// now: the current actor needs CI rights, and a fork head needs its approval.
pub(crate) async fn rerun_run(
    State(state): State<Arc<AppState>>,
    Path((owner, name, run_id)): Path<(String, String, String)>,
    headers: HeaderMap,
    OriginalUri(uri): OriginalUri,
    Form(form): Form<HashMap<String, String>>,
) -> Response {
    let mut ctx = match PageContext::load(state, headers, &uri).await {
        Ok(ctx) => ctx,
        Err(response) => return response,
    };
    let (_, run) = match readable_run(&mut ctx, &owner, &name, &run_id).await {
        Ok(found) => found,
        Err(response) => return response,
    };
    if let Err(response) = ctx.check_csrf(&form) { return response; }
    let actor = ctx
        .require_user()
        .expect("readable_run checked the user")
        .id;
    match ctx.state.ci_runs.rerun_run(run.run_id, actor, crate::backend::api::http::session::extract_session_id(&ctx.headers).map(db::ci::CiCredential::Session)).await {
        Ok(new_run) => ctx.redirect(&run_page(&owner, &name, new_run.run_id)),
        Err(error) => {
            let (status, axum::Json(error)) = super::admission_error(error);
            ctx.error(status, &error.message)
        }
    }
}
