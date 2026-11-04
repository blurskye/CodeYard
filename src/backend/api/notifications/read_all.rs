use crate::backend::api::http::error::ApiError;
use crate::backend::api::notifications::inbox::json_action;
use crate::backend::api::notifications::inbox::native_action;
use crate::backend::api::notifications::inbox::BulkUpdateResponse;
use crate::backend::app::AppState;
use axum::extract::OriginalUri;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::http::StatusCode;
use axum::response::Response;
use axum::Form;
use axum::Json;
use codeyard_db_repos::notifications::records::NotificationInboxAction;
use std::collections::HashMap;
use std::sync::Arc;

pub async fn mark_all_notifications_read(
    State(state): State<Arc<AppState>>,
    headers: axum::http::HeaderMap,
) -> Result<Json<BulkUpdateResponse>, (StatusCode, Json<ApiError>)> {
    let inbox = json_action(&state, &headers, || Ok(NotificationInboxAction::ReadAll)).await?;

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
