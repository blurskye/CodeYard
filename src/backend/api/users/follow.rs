use crate::backend::api::http::context::PageContext;
use crate::backend::api::http::error::ApiError;
use crate::backend::api::http::session::{get_current_user, get_optional_user};
use crate::backend::api::user::responses::{field, mutation_guard, public_api_message, Fields};
use crate::backend::api::users::profile::profile_response;
use crate::backend::app::AppState;
use crate::backend::db;
use crate::backend::identity::following;
use crate::backend::identity::following::{FollowError, FollowStatus};
use axum::extract::{Form, Path, State};
use axum::http::{HeaderMap, StatusCode, Uri};
use axum::response::Response;
use axum::Json;
use serde::Serialize;
use std::sync::Arc;

#[derive(Serialize)]
pub struct UserFollowResponse {
    pub following: bool,
    pub followers_count: i64,
    pub following_count: i64,
}
impl From<FollowStatus> for UserFollowResponse {
    fn from(value: FollowStatus) -> Self {
        Self {
            following: value.following,
            followers_count: value.followers_count,
            following_count: value.following_count,
        }
    }
}

pub(crate) fn follow_failure(error: FollowError) -> (StatusCode, Json<ApiError>) {
    let (status, code) = match error {
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

pub async fn check_user_follow(
    State(state): State<Arc<AppState>>,
    Path(username): Path<String>,
    headers: HeaderMap,
) -> Result<Json<UserFollowResponse>, (StatusCode, Json<ApiError>)> {
    let target = db::users::accounts::get_user_by_username(&state.pool, &username)
        .await
        .map_err(|_| follow_failure(FollowError::Unavailable))?
        .ok_or_else(|| follow_failure(FollowError::Missing))?;
    let viewer = get_optional_user(&state.pool, &headers).await?;
    Ok(Json(
        following::status(&state.pool, target.id, viewer.map(|user| user.id))
            .await
            .map_err(follow_failure)?
            .into(),
    ))
}

pub async fn follow_user(
    State(state): State<Arc<AppState>>,
    Path(username): Path<String>,
    headers: HeaderMap,
) -> Result<Json<UserFollowResponse>, (StatusCode, Json<ApiError>)> {
    let actor = get_current_user(&state.pool, &headers).await?;
    Ok(Json(
        following::change(
            &state.pool,
            &state.notification_broadcaster,
            actor.id,
            &username,
            true,
        )
        .await
        .map_err(follow_failure)?
        .into(),
    ))
}

pub async fn unfollow_user(
    State(state): State<Arc<AppState>>,
    Path(username): Path<String>,
    headers: HeaderMap,
) -> Result<Json<UserFollowResponse>, (StatusCode, Json<ApiError>)> {
    let actor = get_current_user(&state.pool, &headers).await?;
    Ok(Json(
        following::change(
            &state.pool,
            &state.notification_broadcaster,
            actor.id,
            &username,
            false,
        )
        .await
        .map_err(follow_failure)?
        .into(),
    ))
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
    let result = following::change(
        &ctx.state.pool,
        &ctx.state.notification_broadcaster,
        actor,
        &username,
        follow,
    )
    .await
    .map_err(follow_failure);
    match result {
        Ok(_) => ctx.redirect(&format!("/{}", crate::webui::encode_component(&username))),
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
