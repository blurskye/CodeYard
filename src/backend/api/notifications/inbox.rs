use crate::backend::api::context::PageContext;
use crate::backend::api::error::ApiError;
use crate::backend::api::session::extract_session_id;
use crate::backend::api::session::get_current_user;
use crate::backend::api::user::responses::PublicUserResponse as UserResponse;
use crate::backend::app::AppState;
use crate::backend::db;
use crate::webui::pages::notifications as view;
use axum::extract::OriginalUri;
use axum::extract::Query;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::http::StatusCode;
use axum::http::Uri;
use axum::response::Response;
use axum::Json;
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use db::notifications::records::NotificationCursor;
use serde::Deserialize;
use serde::Serialize;
use std::collections::HashMap;
use std::collections::HashSet;
use std::sync::Arc;
#[derive(Serialize, Clone)]
pub struct NotificationResponse {
    pub id: i64,
    pub kind: String,
    pub title: String,
    pub body: Option<String>,
    pub link: Option<String>,
    pub actor: Option<UserResponse>,
    pub repo_id: Option<i64>,
    pub metadata: serde_json::Value,
    pub read_at: Option<String>,
    pub created_at: String,
}

#[derive(Serialize)]
pub struct NotificationsListResponse {
    pub notifications: Vec<NotificationResponse>,
    pub unread_count: i64,
    pub next_cursor: Option<String>,
}

#[derive(Deserialize)]
pub struct NotificationsQuery {
    pub limit: Option<i64>,
    pub unread_only: Option<bool>,
    pub before: Option<String>,
}

fn decode_cursor(value: Option<&str>) -> Result<Option<NotificationCursor>, ()> {
    value
        .map(|value| {
            if value.len() > 512 {
                return Err(());
            }
            let bytes = URL_SAFE_NO_PAD.decode(value).map_err(|_| ())?;
            let cursor: NotificationCursor = serde_json::from_slice(&bytes).map_err(|_| ())?;
            if cursor.id <= 0 || chrono::DateTime::parse_from_rfc3339(&cursor.created_at).is_err() {
                return Err(());
            }
            Ok(cursor)
        })
        .transpose()
}

fn encode_cursor(cursor: NotificationCursor) -> String {
    URL_SAFE_NO_PAD.encode(serde_json::to_vec(&cursor).expect("notification cursor"))
}

#[derive(Serialize)]
pub struct MarkNotificationReadResponse {
    pub notification: NotificationResponse,
    pub unread_count: i64,
}

#[derive(Serialize)]
pub struct MarkAllNotificationsReadResponse {
    pub updated: u64,
    pub unread_count: i64,
}

pub(crate) fn map_notification_response(
    notification: db::models::Notification,
    actor: Option<UserResponse>,
) -> NotificationResponse {
    NotificationResponse {
        id: notification.id,
        kind: notification.kind,
        title: notification.title,
        body: notification.body,
        link: notification.link,
        actor,
        repo_id: notification.repo_id,
        metadata: notification.metadata,
        read_at: notification.read_at.map(|value| value.to_rfc3339()),
        created_at: notification.created_at.to_rfc3339(),
    }
}

pub(crate) fn notification_unavailable(_: anyhow::Error) -> (StatusCode, Json<ApiError>) {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        Json(ApiError::new(
            "unavailable",
            "Notification storage is temporarily unavailable",
        )),
    )
}

pub(crate) async fn require_notification_csrf(
    state: &Arc<AppState>,
    headers: &axum::http::HeaderMap,
) -> Result<(), (StatusCode, Json<ApiError>)> {
    let token = if headers.get_all("x-csrf-token").iter().count() == 1 {
        headers.get("x-csrf-token").and_then(|v| v.to_str().ok())
    } else {
        None
    };
    let context = crate::backend::api::context::PageContext::load(
        state.clone(),
        headers.clone(),
        &axum::http::Uri::from_static("/notifications"),
    )
    .await
    .map_err(|_| {
        notification_unavailable(anyhow::anyhow!("Current notification session unavailable"))
    })?;
    let form = HashMap::from([("csrf_token".to_owned(), token.unwrap_or("").to_owned())]);
    context.check_csrf(&form).map_err(|_| {
        (
            StatusCode::FORBIDDEN,
            Json(ApiError::new(
                "csrf_error",
                "A current same-origin CSRF token is required",
            )),
        )
    })
}

pub(crate) fn notification_session_denied() -> (StatusCode, Json<ApiError>) {
    (
        StatusCode::UNAUTHORIZED,
        Json(ApiError::new(
            "unauthorized",
            "A current notification session is required",
        )),
    )
}

pub async fn list_notifications(
    State(state): State<Arc<AppState>>,
    headers: axum::http::HeaderMap,
    Query(query): Query<NotificationsQuery>,
) -> Result<Json<NotificationsListResponse>, (StatusCode, Json<ApiError>)> {
    let user = get_current_user(&state.pool, &headers).await?;
    let limit = query.limit.unwrap_or(30).clamp(1, 100);
    let unread_only = query.unread_only.unwrap_or(false);
    let before = decode_cursor(query.before.as_deref()).map_err(|_| {
        (
            StatusCode::BAD_REQUEST,
            Json(ApiError::new(
                "invalid_cursor",
                "Invalid notification cursor",
            )),
        )
    })?;

    let session_id = extract_session_id(&headers).ok_or_else(notification_session_denied)?;
    let inbox = crate::backend::notifications::inbox::inbox(
        &state.pool,
        &state.notification_broadcaster,
        &session_id,
        user.id,
        db::notifications::records::NotificationInboxAction::List {
            limit,
            unread_only,
            before,
        },
    )
    .await
    .map_err(inbox_error)?;
    let notifications = inbox.notifications;
    let unread_count = inbox.unread_count;

    let actor_ids: Vec<i64> = notifications
        .iter()
        .filter_map(|notification| notification.actor_id)
        .collect::<HashSet<i64>>()
        .into_iter()
        .collect();
    let actor_map: HashMap<i64, UserResponse> =
        db::users::accounts::get_users_by_ids(&state.pool, &actor_ids)
            .await
            .map_err(notification_unavailable)?
            .into_iter()
            .map(|actor| (actor.id, UserResponse::from(actor)))
            .collect();

    let response_items = notifications
        .into_iter()
        .map(|notification| {
            let actor = notification
                .actor_id
                .and_then(|actor_id| actor_map.get(&actor_id).cloned());
            map_notification_response(notification, actor)
        })
        .collect();

    Ok(Json(NotificationsListResponse {
        notifications: response_items,
        unread_count,
        next_cursor: inbox.next_cursor.map(encode_cursor),
    }))
}

pub(crate) fn inbox_error(
    error: crate::backend::notifications::inbox::InboxError,
) -> (StatusCode, Json<ApiError>) {
    match error {
        crate::backend::notifications::inbox::InboxError::Denied => notification_session_denied(),
        crate::backend::notifications::inbox::InboxError::Unavailable => {
            notification_unavailable(anyhow::anyhow!("unavailable"))
        }
    }
}

pub(crate) async fn notification_list(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    OriginalUri(uri): OriginalUri,
) -> Response {
    let ctx = match PageContext::load(state, headers, &uri).await {
        Ok(ctx) => ctx,
        Err(response) => return response,
    };
    let user = match ctx.require_user() {
        Ok(user) => user,
        Err(response) => return response,
    };
    let unread_only = ctx
        .query
        .get("unread_only")
        .is_some_and(|value| value == "1" || value == "true");
    let Some(session_id) = crate::backend::api::session::extract_session_id(&ctx.headers) else {
        return ctx.error(
            StatusCode::UNAUTHORIZED,
            "A current notification session is required",
        );
    };
    let before = match decode_cursor(ctx.query.get("before").map(String::as_str)) {
        Ok(before) => before,
        Err(()) => return ctx.error(StatusCode::BAD_REQUEST, "Invalid notification cursor"),
    };
    // The validated position travels in the page model and its native forms.
    let current = before.clone().map(encode_cursor);
    let inbox = match db::notifications::records::notification_inbox(
        &ctx.state.pool,
        &session_id,
        user.id,
        db::notifications::records::NotificationInboxAction::List {
            limit: 100,
            unread_only,
            before,
        },
    )
    .await
    {
        Ok(Some(inbox)) => inbox,
        Ok(None) => {
            return ctx.error(
                StatusCode::UNAUTHORIZED,
                "Your session expired. Sign in again to view notifications.",
            )
        }
        Err(_) => return unavailable(&ctx),
    };
    let items = inbox.notifications;
    let unread_count = inbox.unread_count;
    // A page names each distinct actor once, so its picture needs one account lookup per actor.
    let mut actors: HashMap<i64, String> = HashMap::new();
    for actor_id in items
        .iter()
        .filter_map(|item| item.actor_id)
        .collect::<HashSet<_>>()
    {
        if let Ok(Some(actor)) =
            db::users::accounts::get_user_by_id(&ctx.state.pool, actor_id).await
        {
            actors.insert(actor_id, actor.username);
        }
    }
    let model = view::NotificationsPage {
        csrf_token: ctx.csrf_token.clone(),
        unread_count,
        unread_only,
        before: current,
        older: inbox.next_cursor.map(|cursor| {
            format!(
                "/notifications?unread_only={unread_only}&before={}",
                encode_cursor(cursor)
            )
        }),
        notifications: items
            .into_iter()
            .map(|item| view::Notification {
                id: item.id,
                title: item.title,
                body: item.body,
                link: item.link.filter(|link| view::safe_notification_link(link)),
                read: item.read_at.is_some(),
                created_at: item.created_at.to_rfc3339(),
                actor: item.actor_id.and_then(|id| actors.get(&id).cloned()),
            })
            .collect(),
    };
    ctx.page_model(
        "Notifications",
        crate::webui::app::Page::Actions(crate::webui::pages::actions::Page::Notifications(model)),
    )
}

pub(crate) async fn mark_read(
    state: Arc<AppState>,
    headers: HeaderMap,
    uri: Uri,
    form: HashMap<String, String>,
    notification_id: Option<i64>,
) -> Response {
    let ctx = match PageContext::load(state, headers, &uri).await {
        Ok(ctx) => ctx,
        Err(response) => return response,
    };
    let user_id = match ctx.require_user() {
        Ok(user) => user.id,
        Err(response) => return response,
    };
    if let Err(response) = ctx.check_csrf(&form) {
        return response;
    }
    let Some(session_id) = crate::backend::api::session::extract_session_id(&ctx.headers) else {
        return ctx.error(
            StatusCode::UNAUTHORIZED,
            "A current notification session is required",
        );
    };
    let before = match decode_cursor(form.get("before").map(String::as_str)) {
        Ok(before) => before,
        Err(()) => return ctx.error(StatusCode::BAD_REQUEST, "Invalid notification cursor"),
    };
    let unread_only = form
        .get("unread_only")
        .is_some_and(|value| value == "1" || value == "true");
    // Acknowledging a row on an older page returns to that same page.
    let target = match (unread_only, before) {
        (true, Some(cursor)) => format!(
            "/notifications?unread_only=1&before={}",
            encode_cursor(cursor)
        ),
        (true, None) => "/notifications?unread_only=1".to_owned(),
        (false, Some(cursor)) => format!("/notifications?before={}", encode_cursor(cursor)),
        (false, None) => "/notifications".to_owned(),
    };
    let action = match notification_id {
        Some(notification_id) => {
            db::notifications::records::NotificationInboxAction::Read { notification_id }
        }
        None => db::notifications::records::NotificationInboxAction::ReadAll,
    };
    let result = crate::backend::notifications::inbox::inbox(
        &ctx.state.pool,
        &ctx.state.notification_broadcaster,
        &session_id,
        user_id,
        action,
    )
    .await;
    match result {
        Ok(inbox) if notification_id.is_none() || !inbox.notifications.is_empty() => {
            ctx.redirect(&target)
        }
        Ok(_) => ctx.error(
            StatusCode::NOT_FOUND,
            "Notification not found for your account.",
        ),
        Err(error) => {
            let status = match error {
                crate::backend::notifications::inbox::InboxError::Denied => {
                    StatusCode::UNAUTHORIZED
                }
                crate::backend::notifications::inbox::InboxError::Unavailable => {
                    StatusCode::SERVICE_UNAVAILABLE
                }
            };
            ctx.error(
                status,
                if status.is_server_error() {
                    "Notification update could not be confirmed. Refresh before retrying."
                } else {
                    "Notification not found for your account."
                },
            )
        }
    }
}

fn unavailable(ctx: &PageContext) -> Response {
    ctx.error(
        StatusCode::SERVICE_UNAVAILABLE,
        "Notifications are temporarily unavailable. Please try again.",
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn notification_storage_error_never_discloses_driver_or_credentials() {
        let (status, Json(error)) = notification_unavailable(anyhow::anyhow!(
            "SECRET_DRIVER_TOKEN postgres://private:password@internal/path"
        ));
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(error.error, "unavailable");
        let serialized = serde_json::to_string(&error).unwrap();
        assert!(!serialized.contains("SECRET_DRIVER_TOKEN"));
        assert!(!serialized.contains("password@internal"));
    }
}
