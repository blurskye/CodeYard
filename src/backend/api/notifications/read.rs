use super::inbox::inbox_error;
use super::inbox::map_notification_response;
use super::inbox::mark_read;
use super::inbox::notification_session_denied;
use super::inbox::notification_unavailable;
use super::inbox::require_notification_csrf;
use super::inbox::MarkNotificationReadResponse;
use crate::backend::api::error::ApiError;
use crate::backend::api::session::extract_session_id;
use crate::backend::api::session::get_current_user;
use crate::backend::api::user::responses::PublicUserResponse as UserResponse;
use crate::backend::app::AppState;
use crate::backend::db;
use axum::extract::OriginalUri;
use axum::extract::Path;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::http::StatusCode;
use axum::response::Response;
use axum::Form;
use axum::Json;
use std::collections::HashMap;
use std::sync::Arc;
pub async fn mark_notification_read(
    State(state): State<Arc<AppState>>,
    headers: axum::http::HeaderMap,
    Path(notification_id): Path<i64>,
) -> Result<Json<MarkNotificationReadResponse>, (StatusCode, Json<ApiError>)> {
    let user = get_current_user(&state.pool, &headers).await?;
    require_notification_csrf(&state, &headers).await?;

    let session_id = extract_session_id(&headers).ok_or_else(notification_session_denied)?;
    let inbox = crate::backend::notifications::inbox::inbox(
        &state.pool,
        &state.notification_broadcaster,
        &session_id,
        user.id,
        db::notifications::records::NotificationInboxAction::Read { notification_id },
    )
    .await
    .map_err(inbox_error)?;
    let unread_count = inbox.unread_count;
    let notification = inbox.notifications.into_iter().next().ok_or_else(|| {
        (
            StatusCode::NOT_FOUND,
            Json(ApiError::new("not_found", "Notification not found")),
        )
    })?;

    let actor = if let Some(actor_id) = notification.actor_id {
        db::users::accounts::get_user_by_id(&state.pool, actor_id)
            .await
            .map_err(notification_unavailable)?
            .map(UserResponse::from)
    } else {
        None
    };

    Ok(Json(MarkNotificationReadResponse {
        notification: map_notification_response(notification, actor),
        unread_count,
    }))
}

pub(crate) async fn notification_read(
    State(state): State<Arc<AppState>>,
    Path(notification_id): Path<i64>,
    headers: HeaderMap,
    OriginalUri(uri): OriginalUri,
    Form(form): Form<HashMap<String, String>>,
) -> Response {
    mark_read(state, headers, uri, form, Some(notification_id)).await
}
