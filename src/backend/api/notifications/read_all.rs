use super::inbox::inbox_error;
use super::inbox::native_action;
use super::inbox::notification_session_denied;
use super::inbox::BulkUpdateResponse;
use crate::backend::api::http::error::ApiError;
use crate::backend::api::http::session::extract_session_id;
use crate::backend::api::http::session::get_current_user;
use crate::backend::app::AppState;
use crate::backend::db::notifications::records::NotificationInboxAction;
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
) -> Result<Json<BulkUpdateResponse>, (StatusCode, Json<ApiError>)> {
    let user = get_current_user(&state.pool, &headers).await?;

    let session_id = extract_session_id(&headers).ok_or_else(notification_session_denied)?;
    let inbox = crate::backend::notifications::inbox::inbox(
        &state.pool,
        &state.notification_broadcaster,
        &session_id,
        user.id,
        NotificationInboxAction::ReadAll,
    )
    .await
    .map_err(inbox_error)?;

    Ok(Json(BulkUpdateResponse {
        updated: inbox.updated,
        unread_count: inbox.unread_count,
    }))
}

pub(crate) async fn notification_read_all(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    OriginalUri(uri): OriginalUri,
    Form(form): Form<HashMap<String, String>>,
) -> Response {
    native_action(
        state,
        headers,
        uri,
        form,
        |_| NotificationInboxAction::ReadAll,
        false,
    )
    .await
}
