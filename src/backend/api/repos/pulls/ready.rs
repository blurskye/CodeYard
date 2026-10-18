use crate::backend::api::http::context::PageContext;
use crate::backend::api::http::error::ApiError;
use crate::backend::api::http::session::get_current_user;
use crate::backend::api::repos::pulls::pull::*;
use crate::backend::api::user::responses::PublicUserResponse;
use crate::backend::app::AppState;
use crate::backend::db;
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
pub async fn mark_pull_request_ready(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, repo, number)): Path<(String, String, i32)>,
) -> Result<Json<PullRequestDetailResponse>, (StatusCode, Json<ApiError>)> {
    let current_actor = get_current_user(&state.pool, &headers).await?;
    let pull_request =
        crate::backend::repositories::pull_requests::mutations::mark_pull_request_ready(
            &state.pool,
            &state.ci_runs,
            current_actor.id,
            owner,
            repo,
            number,
        )
        .await
        .map_err(super::operation_error)?;
    let author = db::users::accounts::get_user_by_id(&state.pool, pull_request.author_id)
        .await
        .map_err(crate::backend::api::http::error::discussion_write_error)?;

    Ok(Json(
        detail_response(
            &state.pool,
            pull_request,
            author.map(PublicUserResponse::from),
        )
        .await?,
    ))
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
    match crate::backend::repositories::pull_requests::mutations::mark_pull_request_ready(
        &ctx.state.pool,
        &ctx.state.ci_runs,
        ctx.require_user()?.id,
        owner,
        name,
        number,
    )
    .await
    .map_err(crate::backend::api::repos::pulls::operation_error)
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
