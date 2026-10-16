use super::inbox::inbox_error;
use super::inbox::mark_read;
use super::inbox::notification_session_denied;
use super::inbox::require_notification_csrf;
use super::inbox::MarkAllNotificationsReadResponse;
use crate::backend::api::error::ApiError;
use crate::backend::api::session::extract_session_id;
use crate::backend::api::session::get_current_user;
use crate::backend::app::AppState;
use crate::backend::db;
use axum::extract::OriginalUri;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::http::StatusCode;
use axum::response::Response;
use axum::Form;
use axum::Json;
use std::collections::HashMap;
use std::sync::Arc;
pub async fn mark_all_notifications_read(
    State(state): State<Arc<AppState>>,
    headers: axum::http::HeaderMap,
) -> Result<Json<MarkAllNotificationsReadResponse>, (StatusCode, Json<ApiError>)> {
    let user = get_current_user(&state.pool, &headers).await?;
    require_notification_csrf(&state, &headers).await?;

    let session_id = extract_session_id(&headers).ok_or_else(notification_session_denied)?;
    let inbox = crate::backend::notifications::inbox::inbox(
        &state.pool,
        &state.notification_broadcaster,
        &session_id,
        user.id,
        db::notifications::records::NotificationInboxAction::ReadAll,
    )
    .await
    .map_err(inbox_error)?;
    let updated = inbox.updated;
    let unread_count = inbox.unread_count;

    Ok(Json(MarkAllNotificationsReadResponse {
        updated,
        unread_count,
    }))
}

pub(crate) async fn notification_read_all(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    OriginalUri(uri): OriginalUri,
    Form(form): Form<HashMap<String, String>>,
) -> Response {
    mark_read(state, headers, uri, form, None).await
}
