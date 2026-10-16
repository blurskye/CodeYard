use crate::backend::api::auth::errors::session_failure;
use crate::backend::api::error::ApiError;
use crate::backend::api::session::extract_session_id;
use crate::backend::api::user::responses::{AuthResponse, AuthUserResponse as UserResponse};
use crate::backend::app::AppState;
use crate::backend::db::users::sessions as stored;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::Json;
use std::sync::Arc;

pub async fn me(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Result<Json<AuthResponse>, (StatusCode, Json<ApiError>)> {
    let user = if let Some(id) = extract_session_id(&headers) {
        stored::get_session_account(&state.pool, &id)
            .await
            .map_err(|_| session_failure())?
    } else {
        None
    };
    let user = user.filter(|user| !user.is_banned);
    Ok(Json(AuthResponse {
        success: user.is_some(),
        user: user.map(UserResponse::from),
        message: None,
    }))
}
