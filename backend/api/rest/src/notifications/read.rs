use crate::notifications::inbox::json_action;
use crate::notifications::inbox::native_action;
use crate::notifications::inbox::notification_update;
use crate::notifications::inbox::NotificationUpdateResponse;
use axum::extract::OriginalUri;
use axum::extract::Path;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::http::StatusCode;
use axum::response::Response;
use axum::Form;
use axum::Json;
use codeyard_api_http::error::ApiError;
use codeyard_app::app::AppState;
use codeyard_db_repos::notifications::records::NotificationInboxAction;
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
