//! Adding pull requests to a stack and taking them out, from the JSON API and from the pull
//! request page.

use super::pull::{detail_response, PullRequestDetailResponse};
use crate::backend::api::http::context::PageContext;
use crate::backend::api::http::error::ApiError;
use crate::backend::api::http::session::get_current_user;
use crate::backend::api::repos::discussion::*;
use crate::backend::api::repos::forms::authorize_post;
use crate::backend::app::AppState;
use crate::backend::repositories::pull_requests::mutations;
use crate::backend::repositories::pull_requests::restack;
use crate::webui::pages::repos::issues::DetailDraft;
use crate::webui::pages::repos::issues::Kind;
use axum::extract::Form;
use axum::extract::Path;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::http::StatusCode;
use axum::http::Uri;
use axum::response::Response;
use axum::Json;
use std::collections::HashMap;
use std::sync::Arc;

pub async fn post_stack(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, repo, number)): Path<(String, String, i32)>,
) -> Result<Json<PullRequestDetailResponse>, (StatusCode, Json<ApiError>)> {
    let current_actor = get_current_user(&state.pool, &headers).await?;
    let pull_request = mutations::join_stack(
        &state.pool,
        &state.ci_runs,
        current_actor.id,
        owner,
        repo,
        number,
    )
    .await
    .map_err(super::operation_error)?;
    Ok(Json(detail_response(&state.pool, pull_request).await?))
}

pub async fn delete_stack(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, repo, number)): Path<(String, String, i32)>,
) -> Result<Json<PullRequestDetailResponse>, (StatusCode, Json<ApiError>)> {
    let current_actor = get_current_user(&state.pool, &headers).await?;
    let pull_request = mutations::unstack_pull_request(
        &state.pool,
        &state.ci_runs,
        current_actor.id,
        owner,
        repo,
        number,
    )
    .await
    .map_err(super::operation_error)?;
    Ok(Json(detail_response(&state.pool, pull_request).await?))
}

pub async fn post_rebase(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, repo, number)): Path<(String, String, i32)>,
) -> Result<Json<restack::RestackResponse>, (StatusCode, Json<ApiError>)> {
    let current_actor = get_current_user(&state.pool, &headers).await?;
    restack::rebase_stack(
        &state.pool,
        &state.ci_runs,
        current_actor.id,
        (owner, repo, number),
    )
    .await
    .map(Json)
    .map_err(super::operation_error)
}

/// The page answer for a stack change: back to the pull request, or the page with the refusal.
async fn page_answer(
    ctx: &PageContext,
    repo: &RepositoryData,
    number: i32,
    result: Result<(), (StatusCode, Json<ApiError>)>,
) -> ResultPage<Response> {
    match result {
        Ok(()) => Ok(ctx.redirect(&format!("{}/{}", repo.view.family_url(Kind::Pull), number))),
        Err(failure) => {
            let value = item(ctx, repo, Kind::Pull, number).await?;
            let draft = DetailDraft::default();
            let (status, message) = safe_api_error(&failure);
            render_detail(
                ctx,
                repo,
                Kind::Pull,
                &value,
                &draft,
                Some(&message),
                status,
            )
            .await
        }
    }
}

pub(crate) async fn native_join(
    State(state): State<Arc<AppState>>,
    Path((owner, name, number)): Path<(String, String, i32)>,
    headers: HeaderMap,
    uri: Uri,
    Form(form): Form<HashMap<String, String>>,
) -> ResultPage<Response> {
    let ctx = PageContext::load(state, headers, &uri).await?;
    authorize_post(&ctx, &form)?;
    let repo = repository(&ctx, &owner, &name).await?;
    let result = mutations::join_stack(
        &ctx.state.pool,
        &ctx.state.ci_runs,
        ctx.require_user()?.id,
        owner,
        name,
        number,
    )
    .await
    .map(|_| ())
    .map_err(crate::backend::api::repos::pulls::operation_error);
    page_answer(&ctx, &repo, number, result).await
}

pub(crate) async fn native_unstack(
    State(state): State<Arc<AppState>>,
    Path((owner, name, number)): Path<(String, String, i32)>,
    headers: HeaderMap,
    uri: Uri,
    Form(form): Form<HashMap<String, String>>,
) -> ResultPage<Response> {
    let ctx = PageContext::load(state, headers, &uri).await?;
    authorize_post(&ctx, &form)?;
    let repo = repository(&ctx, &owner, &name).await?;
    let result = mutations::unstack_pull_request(
        &ctx.state.pool,
        &ctx.state.ci_runs,
        ctx.require_user()?.id,
        owner,
        name,
        number,
    )
    .await
    .map(|_| ())
    .map_err(crate::backend::api::repos::pulls::operation_error);
    page_answer(&ctx, &repo, number, result).await
}

pub(crate) async fn native_rebase(
    State(state): State<Arc<AppState>>,
    Path((owner, name, number)): Path<(String, String, i32)>,
    headers: HeaderMap,
    uri: Uri,
    Form(form): Form<HashMap<String, String>>,
) -> ResultPage<Response> {
    let ctx = PageContext::load(state, headers, &uri).await?;
    authorize_post(&ctx, &form)?;
    let repo = repository(&ctx, &owner, &name).await?;
    let result = restack::rebase_stack(
        &ctx.state.pool,
        &ctx.state.ci_runs,
        ctx.require_user()?.id,
        (owner, name, number),
    )
    .await
    .map(|_| ())
    .map_err(super::operation_error);
    page_answer(&ctx, &repo, number, result).await
}
