use super::inbox::json_action;
use super::inbox::native_action;
use super::inbox::notification_update;
use super::inbox::NotificationUpdateResponse;
use crate::backend::api::http::error::ApiError;
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
use std::collections::HashMap;
use std::sync::Arc;

pub async fn mark_notification_read(
    State(state): State<Arc<AppState>>,
    headers: axum::http::HeaderMap,
    Path(notification_id): Path<i64>,
) -> Result<Json<NotificationUpdateResponse>, (StatusCode, Json<ApiError>)> {
    let inbox = json_action(&state, &headers, || {
        Ok(NotificationInboxAction::Read { notification_id })
    })
    .await?;
    notification_update(&state, inbox).await
}

pub(crate) async fn notification_read(
    State(state): State<Arc<AppState>>,
    Path(notification_id): Path<i64>,
    headers: HeaderMap,
    OriginalUri(uri): OriginalUri,
    Form(form): Form<HashMap<String, String>>,
) -> Response {
    native_action(
        state,
        headers,
        uri,
        form,
        move |_| NotificationInboxAction::Read { notification_id },
        true,
    )
    .await
}
