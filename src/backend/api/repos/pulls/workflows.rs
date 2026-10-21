use super::pull::{detail_response, PullRequestDetailResponse};
use crate::backend::api::http::context::PageContext;
use crate::backend::api::http::error::ApiError;
use crate::backend::api::http::session::get_current_user;
use crate::backend::api::repos::discussion::*;
use crate::backend::api::repos::forms::{authorize_post, field};
use crate::backend::app::AppState;
use crate::backend::repositories::pull_requests::workflows::RunWorkflowsRequest;
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
pub async fn run_pull_request_workflows(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, repo, number)): Path<(String, String, i32)>,
    Json(request): Json<RunWorkflowsRequest>,
) -> Result<Json<PullRequestDetailResponse>, (StatusCode, Json<ApiError>)> {
    let current_actor = get_current_user(&state.pool, &headers).await?;
    let pull = crate::backend::repositories::pull_requests::workflows::run_pull_request_workflows(
        &state.pool,
        &state.ci_runs,
        current_actor.id,
        owner,
        repo,
        number,
        request,
    )
    .await
    .map_err(super::operation_error)?;
    Ok(Json(detail_response(&state.pool, pull).await?))
}

pub(crate) async fn native_run_workflows(
    State(state): State<Arc<AppState>>,
    Path((owner, name, number)): Path<(String, String, i32)>,
    headers: HeaderMap,
    uri: Uri,
    Form(form): Form<HashMap<String, String>>,
) -> ResultPage<Response> {
    let ctx = PageContext::load(state, headers, &uri).await?;
    authorize_post(&ctx, &form)?;
    let repo = repository(&ctx, &owner, &name).await?;
    match crate::backend::repositories::pull_requests::workflows::run_pull_request_workflows(
        &ctx.state.pool,
        &ctx.state.ci_runs,
        ctx.require_user()?.id,
        owner,
        name,
        number,
        crate::backend::repositories::pull_requests::workflows::RunWorkflowsRequest {
            expected_head: field(&form, "expected_head"),
        },
    )
    .await
    .map_err(crate::backend::api::repos::pulls::operation_error)
    {
        Ok(_) => Ok(ctx.redirect(&format!("{}/{}", repo.view.family_url(Kind::Pull), number))),
        Err(failure) => {
            let (status, message) = safe_api_error(&failure);
            let value = item(&ctx, &repo, Kind::Pull, number).await?;
            render_detail(
                &ctx,
                &repo,
                Kind::Pull,
                &value,
                &DetailDraft::default(),
                Some(&message),
                status,
            )
            .await
        }
    }
}
