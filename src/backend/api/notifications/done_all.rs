use super::inbox::inbox_error;
use super::inbox::invalid_query;
use super::inbox::native_action;
use super::inbox::notification_session_denied;
use super::inbox::require_notification_csrf;
use super::inbox::BulkUpdateResponse;
use super::inbox::ListParams;
use crate::backend::api::http::error::ApiError;
use crate::backend::api::http::session::extract_session_id;
use crate::backend::api::http::session::get_current_user;
use crate::backend::app::AppState;
use crate::backend::db::notifications::records::NotificationInboxAction;
use axum::extract::OriginalUri;
use axum::extract::Query;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::http::StatusCode;
use axum::response::Response;
use axum::Form;
use axum::Json;
use std::collections::HashMap;
use std::sync::Arc;

/// POST /api/notifications/done-all marks done every notification of the list named by the
/// query string (view, repo, reason, q, unread_only).
pub async fn mark_all_notifications_done(
    State(state): State<Arc<AppState>>,
    headers: axum::http::HeaderMap,
    Query(fields): Query<HashMap<String, String>>,
) -> Result<Json<BulkUpdateResponse>, (StatusCode, Json<ApiError>)> {
    let user = get_current_user(&state.pool, &headers).await?;
    require_notification_csrf(&state, &headers).await?;

    let params = ListParams::parse(&fields).map_err(invalid_query)?;
    let session_id = extract_session_id(&headers).ok_or_else(notification_session_denied)?;
    let inbox = crate::backend::notifications::inbox::inbox(
        &state.pool,
        &state.notification_broadcaster,
        &session_id,
        user.id,
        NotificationInboxAction::DoneAll {
            filter: params.filter(),
        },
    )
    .await
    .map_err(inbox_error)?;

    Ok(Json(BulkUpdateResponse {
        updated: inbox.updated,
        unread_count: inbox.unread_count,
    }))
}

pub(crate) async fn notification_done_all(
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
        |params| NotificationInboxAction::DoneAll {
            filter: params.filter(),
        },
        false,
    )
    .await
}
