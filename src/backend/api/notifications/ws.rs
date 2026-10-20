use crate::backend::api::http::error::ApiError;
use crate::backend::api::http::session::get_current_user;
use crate::backend::app::AppState;
use crate::backend::db;
use crate::backend::notifications::broadcast::SocketSlot;
use crate::backend::notifications::current_event;
use crate::backend::notifications::NotificationBroadcaster;
use crate::backend::notifications::NotificationEvent;
use axum::extract::ws::Message;
use axum::extract::ws::WebSocket;
use axum::extract::State;
use axum::extract::WebSocketUpgrade;
use axum::http::header;
use axum::http::HeaderMap;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use futures_util::{stream::StreamExt, SinkExt};
use std::sync::Arc;
use std::time::Duration;
use tracing::info;

/// Largest JSON event sent on the socket. A bigger event keeps its ID and link and drops
/// its body. The notification list still returns the full body.
const MAX_EVENT_JSON: usize = 60 * 1024;

pub async fn handle_socket(
    socket: WebSocket,
    broadcaster: std::sync::Arc<NotificationBroadcaster>,
    pool: db::DbPool,
    session_id: String,
    user_id: i64,
    _slot: SocketSlot,
) {
    let (mut sender, mut receiver) = socket.split();
    let mut rx = broadcaster.subscribe();
    let mut heartbeat = tokio::time::interval_at(
        tokio::time::Instant::now() + Duration::from_secs(15),
        Duration::from_secs(15),
    );
    heartbeat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut awaiting_pong = false;
    info!(user_id, "Notification WebSocket client connected");
    loop {
        tokio::select! {
            message = receiver.next() => {
                match message {
                    Some(Ok(Message::Pong(payload))) if payload.as_slice() == b"heartbeat" => awaiting_pong = false,
                    Some(Ok(Message::Ping(_) | Message::Pong(_))) => {},
                    _ => break,
                }
            }
            _ = heartbeat.tick() => {
                if awaiting_pong { break; }
                if !matches!(tokio::time::timeout(Duration::from_secs(5), sender.send(Message::Ping(b"heartbeat".to_vec()))).await, Ok(Ok(()))) { break; }
                awaiting_pong = true;
                let identity = tokio::time::timeout(Duration::from_secs(5), db::users::sessions::get_session_user(&pool, &session_id)).await;
                if !matches!(identity, Ok(Ok(Some(user))) if user.id == user_id) { break; }
            }
            event = rx.recv() => {
                let Ok(event) = event else { break }; // lag/closed requires reconnect/refetch
                let projected = tokio::time::timeout(Duration::from_secs(5), current_event(&pool, &session_id, user_id, &event)).await;
                let mut event = match projected {
                    Ok(Ok(Some(event))) => event,
                    Ok(Ok(None)) => continue,
                    _ => break, // revoked or unknown authority: never forward cached payload
                };
                let Ok(mut json) = serde_json::to_string(&event) else { break };
                if json.len() > MAX_EVENT_JSON {
                    if let NotificationEvent::Created { body, .. } = &mut event {
                        *body = None;
                    }
                    let Ok(smaller) = serde_json::to_string(&event) else { break };
                    json = smaller;
                }
                if json.len() > MAX_EVENT_JSON { break; }
                if !matches!(tokio::time::timeout(Duration::from_secs(5), sender.send(Message::Text(json))).await, Ok(Ok(()))) { break; }
            }
        }
    }
    // One owned future, no abandoned sibling task or native lock over socket IO.
    info!(user_id, "Notification WebSocket client disconnected");
}

pub(crate) async fn notifications_ws_handler(
    ws: WebSocketUpgrade,
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Response {
    if headers.get_all(header::ORIGIN).iter().count() != 1
        || !crate::backend::api::http::origin::same_origin_submission(&headers)
    {
        return (
            StatusCode::FORBIDDEN,
            Json(ApiError::new(
                "forbidden",
                "A same-origin notification connection is required",
            )),
        )
            .into_response();
    }
    match get_current_user(&state.pool, &headers).await {
        Ok(user) => {
            let Some(session_id) = crate::backend::api::http::session::extract_session_id(&headers)
            else {
                return StatusCode::UNAUTHORIZED.into_response();
            };
            let Some(slot) = state.notification_broadcaster.open_socket(user.id) else {
                return (
                    StatusCode::TOO_MANY_REQUESTS,
                    Json(ApiError::new(
                        "rate_limited",
                        "Too many notification connections are open",
                    )),
                )
                    .into_response();
            };
            let broadcaster = state.notification_broadcaster.clone();
            let pool = state.pool.clone();
            ws.max_message_size(4096)
                .max_frame_size(4096)
                .write_buffer_size(0)
                .max_write_buffer_size(65536)
                .on_upgrade(move |socket| {
                    handle_socket(socket, broadcaster, pool, session_id, user.id, slot)
                })
                .into_response()
        }
        Err((status, body)) => (status, body).into_response(),
    }
}
