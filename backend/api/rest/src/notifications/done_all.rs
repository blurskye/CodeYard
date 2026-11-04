use crate::notifications::inbox::invalid_query;
use crate::notifications::inbox::json_action;
use crate::notifications::inbox::native_action;
use crate::notifications::inbox::BulkUpdateResponse;
use crate::notifications::inbox::ListParams;
use axum::extract::OriginalUri;
use axum::extract::Query;
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

/// POST /api/notifications/done-all marks done every notification of the list named by the
/// query string (view, repo, reason, q, unread_only).
pub async fn mark_all_notifications_done(
    State(state): State<Arc<AppState>>,
    headers: axum::http::HeaderMap,
    Query(fields): Query<HashMap<String, String>>,
) -> Result<Json<BulkUpdateResponse>, (StatusCode, Json<ApiError>)> {
    let inbox = json_action(&state, &headers, || {
        let params = ListParams::parse(&fields).map_err(invalid_query)?;
        Ok(NotificationInboxAction::DoneAll {
            filter: params.filter(),
        })
    })
    .await?;

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
