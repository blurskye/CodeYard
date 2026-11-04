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
use serde::Deserialize;
use std::collections::HashMap;
use std::sync::Arc;

#[derive(Deserialize)]
pub struct SaveRequest {
    pub saved: bool,
}

/// PATCH /api/notifications/:id/save sets or clears the saved mark.
pub async fn set_notification_saved(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(notification_id): Path<i64>,
    Json(request): Json<SaveRequest>,
) -> Result<Json<NotificationUpdateResponse>, (StatusCode, Json<ApiError>)> {
    let inbox = json_action(&state, &headers, || {
        Ok(NotificationInboxAction::Save {
            notification_id,
            saved: request.saved,
        })
    })
    .await?;
    notification_update(&state, inbox).await
}

/// POST /notifications/:id/save from a native form. `saved=1` saves, `saved=0` unsaves.
pub(crate) async fn notification_save(
    State(state): State<Arc<AppState>>,
    Path(notification_id): Path<i64>,
    headers: HeaderMap,
    OriginalUri(uri): OriginalUri,
    Form(form): Form<HashMap<String, String>>,
) -> Response {
    let saved = form
        .get("saved")
        .is_some_and(|value| value == "1" || value == "true");
    native_action(
        state,
        headers,
        uri,
        form,
        move |_| NotificationInboxAction::Save {
            notification_id,
            saved,
        },
        true,
    )
    .await
}
