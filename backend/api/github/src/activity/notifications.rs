//! Token-native projections and mutations of the same persisted inbox used by the web UI.
use crate::empty;
use axum::body::Bytes;
use axum::extract::rejection::QueryRejection;
use axum::extract::OriginalUri;
use axum::extract::Path;
use axum::extract::Query;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::http::HeaderValue;
use axum::http::StatusCode;
use axum::http::Uri;
use axum::response::IntoResponse;
use axum::response::Response;
use axum::routing::delete;
use axum::routing::get;
use axum::routing::patch;
use axum::routing::put;
use axum::routing::MethodRouter;
use axum::Json;
use chrono::DateTime;
use chrono::Utc;
use codeyard_api_http::shared::github::pagination::Pagination;
use codeyard_app::api_shared::github::repository;
use codeyard_app::app::AppState;
use codeyard_db_repos::notifications::records::NotificationInboxAction;
use codeyard_db_repos::notifications::threads::ThreadAction;
use codeyard_db_repos::notifications::threads::ThreadFilter;
use codeyard_db_repos::notifications::threads::ThreadInbox;
use codeyard_model::github_error::Error;
use codeyard_model::github_error::Result;
use codeyard_notifications::inbox::InboxError;
use serde::Deserialize;
use std::sync::Arc;

// These ten operations document no JSON representation for errors.
fn response(result: Result<Response>) -> Response {
    result.unwrap_or_else(|error| empty(error.0))
}
fn polling(mut response: Response) -> Response {
    response
        .headers_mut()
        .insert("x-poll-interval", HeaderValue::from_static("60"));
    response
}
async fn act(state: &AppState, headers: &HeaderMap, action: ThreadAction) -> Result<ThreadInbox> {
    let actor = codeyard_app::api_shared::github::auth::authenticate(state, headers, true)
        .await?
        .ok_or_else(codeyard_app::api_shared::github::auth::required)?;
    if action.requires_write() {
        actor.require_write()?;
    }
    codeyard_notifications::inbox::token_inbox(
        &state.pool,
        &state.notification_broadcaster,
        actor.token.id,
        actor.user.id,
        NotificationInboxAction::Threads(action),
    )
    .await
    .map_err(|error| match error {
        InboxError::Denied => codeyard_app::api_shared::github::auth::required(),
        InboxError::Unavailable => Error::unavailable(()),
    })?
    .threads
    .ok_or_else(|| Error::unavailable(()))
}
fn thread_id(id: &str) -> Result<i64> {
    id.parse::<i64>()
        .ok()
        .filter(|id| *id > 0)
        .ok_or_else(Error::missing)
}
#[derive(Deserialize)]
struct Parameters {
    page: Option<usize>,
    per_page: Option<usize>,
    #[serde(default)]
    all: bool,
    #[serde(default)]
    participating: bool,
    since: Option<DateTime<Utc>>,
    before: Option<DateTime<Utc>>,
}
async fn collection(
    state: &AppState,
    headers: &HeaderMap,
    namespace: Option<(String, String)>,
    query: std::result::Result<Query<Parameters>, QueryRejection>,
    uri: Uri,
) -> Result<Response> {
    let Query(query) = query.map_err(|_| Error::invalid("Invalid notification query"))?;
    let repository_id = match namespace {
        Some((owner, name)) => Some(repository(state, headers, &owner, &name, false).await?.0.id),
        None => None,
    };
    let page = Pagination {
        page: query.page,
        per_page: query.per_page,
    };
    let (offset, limit) = page.window()?;
    let inbox = act(
        state,
        headers,
        ThreadAction::List(ThreadFilter {
            repository_id,
            all: query.all,
            participating: query.participating,
            since: query.since,
            before: query.before,
            limit: (limit + 1) as i64,
            offset: offset as i64,
        }),
    )
    .await?;
    if !inbox.found {
        return Err(Error::missing());
    }
    let more = inbox.threads.len() > limit;
    let values = inbox
        .threads
        .iter()
        .take(limit)
        .map(codeyard_app::webhooks::payloads::notification_thread_json)
        .collect();
    Ok(polling(page.response(&uri, values, more)?))
}
async fn list(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    query: std::result::Result<Query<Parameters>, QueryRejection>,
    OriginalUri(uri): OriginalUri,
) -> Response {
    response(collection(&state, &headers, None, query, uri).await)
}
async fn list_repo(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(namespace): Path<(String, String)>,
    query: std::result::Result<Query<Parameters>, QueryRejection>,
    OriginalUri(uri): OriginalUri,
) -> Response {
    response(collection(&state, &headers, Some(namespace), query, uri).await)
}
#[derive(Default, Deserialize)]
struct Mark {
    last_read_at: Option<DateTime<Utc>>,
    read: Option<bool>,
}
async fn mark(
    state: &AppState,
    headers: &HeaderMap,
    namespace: Option<(String, String)>,
    body: Bytes,
) -> Result<Response> {
    let input: Mark = if body.is_empty() {
        Mark::default()
    } else {
        serde_json::from_slice(&body)
            .map_err(|_| Error::invalid("Invalid notification read request"))?
    };
    let repository_id = match namespace {
        Some((owner, name)) => Some(repository(state, headers, &owner, &name, false).await?.0.id),
        None => None,
    };
    let read = if repository_id.is_some() {
        true
    } else {
        input.read.unwrap_or(true)
    };
    let result = act(
        state,
        headers,
        ThreadAction::ReadAll {
            repository_id,
            last_read_at: input.last_read_at.or_else(|| Some(Utc::now())),
            read,
        },
    )
    .await?;
    if !result.found {
        return Err(Error::missing());
    }
    Ok(empty(StatusCode::RESET_CONTENT))
}
async fn mark_all(State(state): State<Arc<AppState>>, headers: HeaderMap, body: Bytes) -> Response {
    response(mark(&state, &headers, None, body).await)
}
async fn mark_repo(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(namespace): Path<(String, String)>,
    body: Bytes,
) -> Response {
    response(mark(&state, &headers, Some(namespace), body).await)
}
async fn get_thread(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    response(
        async {
            let mut inbox = act(&state, &headers, ThreadAction::Get(thread_id(&id)?)).await?;
            let view = inbox.threads.pop().ok_or_else(Error::missing)?;
            Ok(polling(
                Json(codeyard_app::webhooks::payloads::notification_thread_json(
                    &view,
                ))
                .into_response(),
            ))
        }
        .await,
    )
}
async fn change_thread(
    state: &AppState,
    headers: &HeaderMap,
    id: &str,
    action: fn(i64) -> ThreadAction,
    status: StatusCode,
) -> Result<Response> {
    let result = act(state, headers, action(thread_id(id)?)).await?;
    if !result.found {
        return Err(Error::missing());
    }
    Ok(empty(status))
}
async fn read_thread(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    response(
        change_thread(
            &state,
            &headers,
            &id,
            ThreadAction::Read,
            StatusCode::RESET_CONTENT,
        )
        .await,
    )
}
async fn done_thread(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    response(
        change_thread(
            &state,
            &headers,
            &id,
            ThreadAction::Done,
            StatusCode::NO_CONTENT,
        )
        .await,
    )
}
fn subscription(inbox: ThreadInbox) -> Result<Response> {
    let thread = inbox.thread.ok_or_else(Error::missing)?;
    let subscription = inbox.subscription.ok_or_else(Error::missing)?;
    Ok(Json(
        codeyard_app::webhooks::payloads::notification_subscription_json(&thread, &subscription),
    )
    .into_response())
}
async fn get_subscription(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    response(
        async {
            subscription(
                act(
                    &state,
                    &headers,
                    ThreadAction::GetSubscription(thread_id(&id)?),
                )
                .await?,
            )
        }
        .await,
    )
}
#[derive(Default, Deserialize)]
struct Subscription {
    #[serde(default)]
    ignored: bool,
}
async fn set_subscription(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(id): Path<String>,
    body: Bytes,
) -> Response {
    response(
        async {
            let input: Subscription = if body.is_empty() {
                Subscription::default()
            } else {
                serde_json::from_slice(&body)
                    .map_err(|_| Error::invalid("Invalid thread subscription"))?
            };
            subscription(
                act(
                    &state,
                    &headers,
                    ThreadAction::SetSubscription {
                        thread_id: thread_id(&id)?,
                        subscribed: !input.ignored,
                        ignored: input.ignored,
                    },
                )
                .await?,
            )
        }
        .await,
    )
}
async fn delete_subscription(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    response(
        change_thread(
            &state,
            &headers,
            &id,
            ThreadAction::DeleteSubscription,
            StatusCode::NO_CONTENT,
        )
        .await,
    )
}
pub(super) fn handlers() -> Vec<(&'static str, MethodRouter<Arc<AppState>>)> {
    vec![
        (
            "activity/list-notifications-for-authenticated-user",
            get(list),
        ),
        ("activity/mark-notifications-as-read", put(mark_all)),
        ("activity/get-thread", get(get_thread)),
        ("activity/mark-thread-as-read", patch(read_thread)),
        ("activity/mark-thread-as-done", delete(done_thread)),
        (
            "activity/get-thread-subscription-for-authenticated-user",
            get(get_subscription),
        ),
        ("activity/set-thread-subscription", put(set_subscription)),
        (
            "activity/delete-thread-subscription",
            delete(delete_subscription),
        ),
        (
            "activity/list-repo-notifications-for-authenticated-user",
            get(list_repo),
        ),
        ("activity/mark-repo-notifications-as-read", put(mark_repo)),
    ]
}
