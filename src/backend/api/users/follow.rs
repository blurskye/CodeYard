use crate::backend::api::http::context::PageContext;
use crate::backend::api::http::error::ApiError;
use crate::backend::api::user::responses::field;
use crate::backend::api::user::responses::mutation_guard;
use crate::backend::api::user::responses::public_api_message;
use crate::backend::api::user::responses::Fields;
use crate::backend::api::users::profile::profile_response;
use crate::backend::app::AppState;

use crate::backend::identity::following::FollowError;
use axum::extract::{Form, Path, State};
use axum::http::{HeaderMap, StatusCode, Uri};
use axum::response::Response;
use axum::Json;
use std::sync::Arc;

pub(crate) fn follow_failure(error: FollowError) -> (StatusCode, Json<ApiError>) {
    let (status, code) = match error {
        FollowError::Restricted => (StatusCode::FORBIDDEN, "forbidden"),
        FollowError::Missing => (StatusCode::NOT_FOUND, "not_found"),
        FollowError::SelfFollow | FollowError::SelfUnfollow => {
            (StatusCode::BAD_REQUEST, "validation_error")
        }
        FollowError::Unavailable | FollowError::CountsUnavailableAfterSave => {
            (StatusCode::SERVICE_UNAVAILABLE, "unavailable")
        }
    };
    (status, Json(ApiError::new(code, &error.to_string())))
}

pub(crate) async fn follow_post(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    uri: Uri,
    Path(username): Path<String>,
    Form(form): Form<Fields>,
) -> Response {
    let ctx = match PageContext::load(state, headers, &uri).await {
        Ok(ctx) => ctx,
        Err(response) => return response,
    };
    if let Err(response) = mutation_guard(&ctx, &form, false) {
        return response;
    }
    let follow = match field(&form, "action") {
        "follow" => true,
        "unfollow" => false,
        _ => {
            return profile_response(
                &ctx,
                &username,
                StatusCode::BAD_REQUEST,
                Some("Choose follow or unfollow."),
                1,
            )
            .await
        }
    };
    let actor = ctx
        .user
        .as_ref()
        .expect("mutation guard checked account")
        .id;
    let session =
        crate::backend::api::http::session::extract_session_id(&ctx.headers).unwrap_or_default();
    let credential = crate::backend::db::users::accounts::AccountCredential::Session(&session);
    let result = crate::backend::identity::following::change(
        &ctx.state.pool,
        &ctx.state.notification_broadcaster,
        actor,
        &credential,
        &username,
        follow,
    )
    .await
    .map_err(follow_failure);
    match result {
        Ok(_) => ctx.redirect(&codeyard_webui::profile_path(&username)),
        Err(error) => {
            profile_response(
                &ctx,
                &username,
                error.0,
                Some(public_api_message(&error.1 .0)),
                1,
            )
            .await
        }
    }
}
