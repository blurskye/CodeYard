use crate::discussion::item;
use crate::discussion::render_detail;
use crate::discussion::repository;
use crate::discussion::safe_api_error;
use crate::discussion::ResultPage;
use crate::pulls::pull::detail_response;
use crate::pulls::pull::PullRequestDetailResponse;
use axum::extract::Form;
use axum::extract::Path;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::http::StatusCode;
use axum::http::Uri;
use axum::response::Response;
use axum::Json;
use codeyard_api_http::context::PageContext;
use codeyard_api_http::error::ApiError;
use codeyard_api_http::session::get_current_user;
use codeyard_api_http::shared::repos::forms::authorize_post;
use codeyard_api_http::shared::repos::forms::field;
use codeyard_app::app::AppState;
use codeyard_repos_core::repositories::pull_requests::workflows::RunWorkflowsRequest;
use codeyard_webui::pages::repos::issues::DetailDraft;
use codeyard_webui::pages::repos::issues::Kind;
use std::collections::HashMap;
use std::sync::Arc;

pub async fn run_pull_request_workflows(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, repo, number)): Path<(String, String, i32)>,
    Json(request): Json<RunWorkflowsRequest>,
) -> Result<Json<PullRequestDetailResponse>, (StatusCode, Json<ApiError>)> {
    let current_actor = get_current_user(&state.pool, &headers).await?;
    let repository =
        crate::access::readable_repository(&state.pool, &owner, &repo, &headers).await?;
    let pull = codeyard_repos_core::repositories::pull_requests::workflows::run_pull_request_workflows(
        &state.pool,
        &state.ci_runs,
        current_actor.id,
        &repository,
        number,
        request,
    )
    .await
    .map_err(crate::pulls::operation_error)?;
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
    match codeyard_repos_core::repositories::pull_requests::workflows::run_pull_request_workflows(
        &ctx.state.pool,
        &ctx.state.ci_runs,
        ctx.require_user()?.id,
        &repo.record,
        number,
        codeyard_repos_core::repositories::pull_requests::workflows::RunWorkflowsRequest {
            expected_head: field(&form, "expected_head"),
        },
    )
    .await
    .map_err(crate::pulls::operation_error)
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
