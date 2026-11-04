use crate::discussion::expected;
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
use codeyard_app::app::AppState;
use codeyard_webui::pages::repos::issues::DetailDraft;
use codeyard_webui::pages::repos::issues::Kind;
use std::collections::HashMap;
use std::sync::Arc;

pub async fn mark_pull_request_ready(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, repo, number)): Path<(String, String, i32)>,
) -> Result<Json<PullRequestDetailResponse>, (StatusCode, Json<ApiError>)> {
    let current_actor = get_current_user(&state.pool, &headers).await?;
    let pull_request =
        codeyard_app::repositories::pull_requests::mutations::mark_pull_request_ready(
            &state.pool,
            &state.ci_runs,
            current_actor.id,
            owner,
            repo,
            number,
            None,
        )
        .await
        .map_err(crate::pulls::operation_error)?;
    Ok(Json(detail_response(&state.pool, pull_request).await?))
}

pub(crate) async fn native_ready(
    State(state): State<Arc<AppState>>,
    Path((owner, name, number)): Path<(String, String, i32)>,
    headers: HeaderMap,
    uri: Uri,
    Form(form): Form<HashMap<String, String>>,
) -> ResultPage<Response> {
    let ctx = PageContext::load(state, headers, &uri).await?;
    authorize_post(&ctx, &form)?;
    let repo = repository(&ctx, &owner, &name).await?;
    let value = item(&ctx, &repo, Kind::Pull, number).await?;
    let draft = DetailDraft::default();
    if let Err(message) = expected(&form, &value) {
        return render_detail(
            &ctx,
            &repo,
            Kind::Pull,
            &value,
            &draft,
            Some(message),
            StatusCode::CONFLICT,
        )
        .await;
    }
    match codeyard_app::repositories::pull_requests::mutations::mark_pull_request_ready(
        &ctx.state.pool,
        &ctx.state.ci_runs,
        ctx.require_user()?.id,
        owner,
        name,
        number,
        form.get("expected_updated").map(String::as_str),
    )
    .await
    .map_err(crate::pulls::operation_error)
    {
        Ok(_) => Ok(ctx.redirect(&format!("{}/{}", repo.view.family_url(Kind::Pull), number))),
        Err(failure) => {
            let (status, message) = safe_api_error(&failure);
            render_detail(
                &ctx,
                &repo,
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
