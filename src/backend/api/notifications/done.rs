use super::inbox::inbox_error;
use super::inbox::native_action;
use super::inbox::notification_session_denied;
use super::inbox::notification_update;
use super::inbox::require_notification_csrf;
use super::inbox::NotificationUpdateResponse;
use crate::backend::api::http::error::ApiError;
use crate::backend::api::http::session::extract_session_id;
use crate::backend::api::http::session::get_current_user;
use crate::backend::app::AppState;
use crate::backend::db::notifications::records::NotificationInboxAction;
use axum::extract::OriginalUri;
use axum::extract::Path;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::http::StatusCode;
use axum::response::Response;
use axum::Form;
use axum::Json;
use serde::Deserialize;
use std::collections::HashMap;
use std::sync::Arc;

#[derive(Deserialize)]
pub struct DoneRequest {
    pub done: bool,
}

/// PATCH /api/notifications/:id/done marks the notification done, or moves it back to the inbox.
pub async fn set_notification_done(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(notification_id): Path<i64>,
    Json(request): Json<DoneRequest>,
) -> Result<Json<NotificationUpdateResponse>, (StatusCode, Json<ApiError>)> {
    let user = get_current_user(&state.pool, &headers).await?;
    require_notification_csrf(&state, &headers).await?;

    let session_id = extract_session_id(&headers).ok_or_else(notification_session_denied)?;
    let inbox = crate::backend::notifications::inbox::inbox(
        &state.pool,
        &state.notification_broadcaster,
        &session_id,
        user.id,
        NotificationInboxAction::Done {
            notification_id,
            done: request.done,
        },
    )
    .await
    .map_err(inbox_error)?;
    notification_update(&state, inbox).await
}

/// POST /notifications/:id/done from a native form. `done=1` marks done, `done=0` undoes.
pub(crate) async fn notification_done(
    State(state): State<Arc<AppState>>,
    Path(notification_id): Path<i64>,
    headers: HeaderMap,
    OriginalUri(uri): OriginalUri,
    Form(form): Form<HashMap<String, String>>,
) -> Response {
    let done = form
        .get("done")
        .is_some_and(|value| value == "1" || value == "true");
    native_action(
        state,
        headers,
        uri,
        form,
        move |_| NotificationInboxAction::Done {
            notification_id,
            done,
        },
        true,
    )
    .await
}
