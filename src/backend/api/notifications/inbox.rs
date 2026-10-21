use crate::backend::api::http::context::PageContext;
use crate::backend::api::http::error::ApiError;
use crate::backend::api::http::session::extract_session_id;
use crate::backend::api::http::session::get_current_user;
use crate::backend::api::user::responses::PublicUserResponse as UserResponse;
use crate::backend::app::AppState;
use crate::backend::db;
use crate::backend::notifications::reasons;
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
use db::notifications::records::{
    NotificationCursor, NotificationFilter, NotificationInbox, NotificationInboxAction,
    NotificationView, ViewCounts,
};
use serde::Serialize;
use std::collections::{BTreeMap, HashMap, HashSet};
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
    pub saved_at: Option<String>,
    pub done_at: Option<String>,
    pub created_at: String,
}

#[derive(Serialize)]
pub struct NotificationsListResponse {
    pub notifications: Vec<NotificationResponse>,
    pub unread_count: i64,
    pub view_counts: ViewCounts,
    pub next_cursor: Option<String>,
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

/// The changed notification and the unread count, after an acknowledgment or a mark.
#[derive(Serialize)]
pub struct NotificationUpdateResponse {
    pub notification: NotificationResponse,
    pub unread_count: i64,
}

/// The rows changed by a bulk action, and the unread count after it.
#[derive(Serialize)]
pub struct BulkUpdateResponse {
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
        saved_at: notification.saved_at.map(|value| value.to_rfc3339()),
        done_at: notification.done_at.map(|value| value.to_rfc3339()),
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

pub(crate) fn notification_session_denied() -> (StatusCode, Json<ApiError>) {
    (
        StatusCode::UNAUTHORIZED,
        Json(ApiError::new(
            "unauthorized",
            "A current notification session is required",
        )),
    )
}

/// The fields that choose a notification list: the tab, the filters and the page position.
/// Page queries, native forms and JSON requests all send them as text, so they parse here.
pub(crate) struct ListParams {
    pub view: NotificationView,
    pub repository: Option<i64>,
    /// The reason name, one of `reasons::REASONS`.
    pub reason: Option<&'static str>,
    pub query: Option<String>,
    pub unread_only: bool,
    pub before: Option<NotificationCursor>,
}

impl ListParams {
    pub(crate) fn parse(fields: &HashMap<String, String>) -> Result<Self, &'static str> {
        let text = |name: &str| {
            fields
                .get(name)
                .map(|value| value.trim())
                .filter(|value| !value.is_empty())
        };
        let view = match text("view") {
            None | Some("inbox") => NotificationView::Inbox,
            Some("saved") => NotificationView::Saved,
            Some("done") => NotificationView::Done,
            Some(_) => return Err("Invalid notification view"),
        };
        let repository = match text("repo") {
            None => None,
            Some(value) => Some(
                value
                    .parse::<i64>()
                    .ok()
                    .filter(|id| *id > 0)
                    .ok_or("Invalid notification repository")?,
            ),
        };
        let reason = match text("reason") {
            None => None,
            Some(name) => Some(
                reasons::REASONS
                    .iter()
                    .find(|(known, _, _)| *known == name)
                    .map(|(known, _, _)| *known)
                    .ok_or("Invalid notification reason")?,
            ),
        };
        let query = text("q").map(str::to_owned);
        if query
            .as_deref()
            .is_some_and(|query| query.chars().count() > 200)
        {
            return Err("Search text is too long");
        }
        let unread_only = text("unread_only").is_some_and(|value| value == "1" || value == "true");
        let before = decode_cursor(fields.get("before").map(String::as_str))
            .map_err(|()| "Invalid notification cursor")?;
        Ok(Self {
            view,
            repository,
            reason,
            query,
            unread_only,
            before,
        })
    }

    pub(crate) fn filter(&self) -> NotificationFilter {
        NotificationFilter {
            view: self.view,
            repository_id: self.repository,
            kinds: self
                .reason
                .and_then(reasons::kinds)
                .unwrap_or_default()
                .iter()
                .map(|kind| (*kind).to_owned())
                .collect(),
            query: self.query.clone(),
            unread_only: self.unread_only,
        }
    }

    /// The page position as it travels in a URL or a form, if this list has one.
    pub(crate) fn before_token(&self) -> Option<String> {
        self.before.clone().map(encode_cursor)
    }
}

/// The page size of the JSON list: 30 by default, 1 to 100.
fn limit_field(fields: &HashMap<String, String>) -> Result<i64, &'static str> {
    match fields.get("limit") {
        None => Ok(30),
        Some(value) => value
            .parse::<i64>()
            .map(|limit| limit.clamp(1, 100))
            .map_err(|_| "Invalid notification limit"),
    }
}

pub(crate) fn invalid_query(message: &str) -> (StatusCode, Json<ApiError>) {
    (
        StatusCode::BAD_REQUEST,
        Json(ApiError::new("invalid_query", message)),
    )
}

pub async fn list_notifications(
    State(state): State<Arc<AppState>>,
    headers: axum::http::HeaderMap,
    Query(fields): Query<HashMap<String, String>>,
) -> Result<Json<NotificationsListResponse>, (StatusCode, Json<ApiError>)> {
    let user = get_current_user(&state.pool, &headers).await?;
    let params = ListParams::parse(&fields).map_err(invalid_query)?;
    let limit = limit_field(&fields).map_err(invalid_query)?;
    let session_id = extract_session_id(&headers).ok_or_else(notification_session_denied)?;
    let inbox = crate::backend::notifications::inbox::inbox(
        &state.pool,
        &state.notification_broadcaster,
        &session_id,
        user.id,
        NotificationInboxAction::List {
            filter: params.filter(),
            limit,
            before: params.before.clone(),
        },
    )
    .await
    .map_err(inbox_error)?;
    let NotificationInbox {
        notifications,
        next_cursor,
        unread_count,
        counts,
        ..
    } = inbox;
    let view_counts = counts.unwrap_or_default().views;

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
        view_counts,
        next_cursor: next_cursor.map(encode_cursor),
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

/// The JSON answer to one changed notification: the notification with its actor, and the
/// unread count. A change to a notification the recipient cannot see is a 404.
pub(crate) async fn notification_update(
    state: &Arc<AppState>,
    inbox: NotificationInbox,
) -> Result<Json<NotificationUpdateResponse>, (StatusCode, Json<ApiError>)> {
    let unread_count = inbox.unread_count;
    let notification = inbox.notifications.into_iter().next().ok_or_else(|| {
        (
            StatusCode::NOT_FOUND,
            Json(ApiError::new("not_found", "Notification not found")),
        )
    })?;
    let actor = match notification.actor_id {
        Some(actor_id) => db::users::accounts::get_user_by_id(&state.pool, actor_id)
            .await
            .map_err(notification_unavailable)?
            .map(UserResponse::from),
        None => None,
    };
    Ok(Json(NotificationUpdateResponse {
        notification: map_notification_response(notification, actor),
        unread_count,
    }))
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
    let Some(session_id) = crate::backend::api::http::session::extract_session_id(&ctx.headers)
    else {
        return ctx.error(
            StatusCode::UNAUTHORIZED,
            "A current notification session is required",
        );
    };
    let params = match ListParams::parse(&ctx.query) {
        Ok(params) => params,
        Err(message) => return ctx.error(StatusCode::BAD_REQUEST, message),
    };
    let inbox = match db::notifications::records::notification_inbox(
        &ctx.state.pool,
        &session_id,
        user.id,
        NotificationInboxAction::List {
            filter: params.filter(),
            limit: 100,
            before: params.before.clone(),
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
    let NotificationInbox {
        notifications,
        next_cursor,
        unread_count,
        counts,
        ..
    } = inbox;
    let counts = counts.unwrap_or_default();
    let names: BTreeMap<i64, String> = counts.repositories.iter().cloned().collect();
    let actor_ids: Vec<_> = notifications
        .iter()
        .filter_map(|item| item.actor_id)
        .collect::<HashSet<_>>()
        .into_iter()
        .collect();
    let actors: HashMap<i64, String> =
        match db::users::accounts::get_users_by_ids(&ctx.state.pool, &actor_ids).await {
            Ok(actors) => actors
                .into_iter()
                .map(|actor| (actor.id, actor.username))
                .collect(),
            Err(_) => return unavailable(&ctx),
        };
    let mut repositories: Vec<view::RepositoryOption> = counts
        .repositories
        .into_iter()
        .map(|(id, name)| view::RepositoryOption { id, name })
        .collect();
    repositories.sort_by(|a, b| a.name.cmp(&b.name));
    let older = next_cursor.map(|cursor| {
        view::notifications_href(
            params.view.as_str(),
            params.repository,
            params.reason,
            params.query.as_deref(),
            params.unread_only,
            Some(&encode_cursor(cursor)),
        )
    });
    let model = view::NotificationsPage {
        csrf_token: ctx.csrf_token.clone(),
        notifications: notifications
            .into_iter()
            .map(|item| view::Notification {
                id: item.id,
                subject: item
                    .metadata
                    .get("subject")
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_owned),
                reason: reasons::label_for_kind(&item.kind).map(str::to_owned),
                title: item.title,
                body: item
                    .body
                    .as_deref()
                    .map(crate::webui::components::body_preview),
                link: item.link,
                read: item.read_at.is_some(),
                saved: item.saved_at.is_some(),
                done: item.done_at.is_some(),
                created_at: item.created_at.to_rfc3339(),
                actor: item.actor_id.and_then(|id| actors.get(&id).cloned()),
                repository: item.repo_id.and_then(|id| names.get(&id).cloned()),
            })
            .collect(),
        unread_count,
        view: params.view.as_str().to_owned(),
        counts: view::ViewCounts {
            inbox: counts.views.inbox,
            saved: counts.views.saved,
            done: counts.views.done,
        },
        repository: params.repository,
        repositories,
        reason: params.reason.map(str::to_owned),
        reasons: reasons::REASONS
            .iter()
            .map(|(slug, label, _)| view::ReasonOption {
                slug: (*slug).to_owned(),
                label: (*label).to_owned(),
            })
            .collect(),
        query: params.query.clone(),
        unread_only: params.unread_only,
        before: params.before_token(),
        older,
    };
    ctx.page_model(
        "Notifications",
        crate::webui::app::Page::Notifications(model),
    )
}

/// Runs one change from a native form, then returns to the list the form came from. The
/// action sees the form's list state, so a bulk change can use the same filters. `must_match`
/// makes a notification that the recipient cannot see a 404.
pub(crate) async fn native_action(
    state: Arc<AppState>,
    headers: HeaderMap,
    uri: Uri,
    form: HashMap<String, String>,
    action: impl FnOnce(&ListParams) -> NotificationInboxAction,
    must_match: bool,
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
    let Some(session_id) = crate::backend::api::http::session::extract_session_id(&ctx.headers)
    else {
        return ctx.error(
            StatusCode::UNAUTHORIZED,
            "A current notification session is required",
        );
    };
    let params = match ListParams::parse(&form) {
        Ok(params) => params,
        Err(message) => return ctx.error(StatusCode::BAD_REQUEST, message),
    };
    let action = action(&params);
    // A bulk change can empty the page it came from, so it returns to the first page.
    let before = if matches!(action, NotificationInboxAction::DoneAll { .. }) {
        None
    } else {
        params.before_token()
    };
    let target = view::notifications_href(
        params.view.as_str(),
        params.repository,
        params.reason,
        params.query.as_deref(),
        params.unread_only,
        before.as_deref(),
    );
    let result = crate::backend::notifications::inbox::inbox(
        &ctx.state.pool,
        &ctx.state.notification_broadcaster,
        &session_id,
        user_id,
        action,
    )
    .await;
    match result {
        Ok(inbox) if !must_match || !inbox.notifications.is_empty() => ctx.redirect(&target),
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

    #[test]
    fn list_fields_parse_to_one_filter_and_refuse_unknown_values() {
        let fields = |pairs: &[(&str, &str)]| -> HashMap<String, String> {
            pairs
                .iter()
                .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
                .collect()
        };
        let params = ListParams::parse(&fields(&[
            ("view", "saved"),
            ("repo", "7"),
            ("reason", "comment"),
            ("q", "  build "),
            ("unread_only", "1"),
        ]))
        .unwrap();
        let filter = params.filter();
        assert_eq!(filter.view, NotificationView::Saved);
        assert_eq!(filter.repository_id, Some(7));
        assert_eq!(filter.kinds.len(), 3);
        assert_eq!(filter.query.as_deref(), Some("build"));
        assert!(filter.unread_only);
        assert_eq!(
            ListParams::parse(&fields(&[("view", "archive")]))
                .err()
                .unwrap(),
            "Invalid notification view"
        );
        assert!(ListParams::parse(&fields(&[("repo", "0")])).is_err());
        assert!(ListParams::parse(&fields(&[("reason", "nonsense")])).is_err());
        let assigned = ListParams::parse(&fields(&[("reason", "assigned")])).unwrap();
        assert_eq!(assigned.filter().kinds, vec!["assigned".to_owned()]);
        assert!(ListParams::parse(&fields(&[("before", "not-a-cursor")])).is_err());
        let long = "x".repeat(201);
        assert!(ListParams::parse(&fields(&[("q", long.as_str())])).is_err());
        let defaults = ListParams::parse(&fields(&[])).unwrap().filter();
        assert_eq!(defaults, NotificationFilter::default());
    }
}
