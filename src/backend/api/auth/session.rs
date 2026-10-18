use crate::backend::api::auth::errors::{database_failure, session_failure};
use crate::backend::api::http::error::ApiError;
use crate::backend::api::http::session;
use crate::backend::api::http::session::{
    extract_session_id, extract_user_agent, generate_session_id,
};
use crate::backend::app::AppState;
use crate::backend::db::users::sessions as stored;
use crate::backend::{db, identity};
use axum::extract::State;
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::IntoResponse;
use axum::Json;
use chrono::{Duration, Utc};
use serde::Serialize;
use std::sync::Arc;

#[derive(Debug, Serialize)]
pub struct SessionResponse {
    pub session_id: String,
    pub authenticated: bool,
}

pub async fn create_session(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Result<impl IntoResponse, (StatusCode, Json<ApiError>)> {
    if let Some(id) = extract_session_id(&headers) {
        if let Some(session) = stored::get_browser_session(&state.pool, &id)
            .await
            .map_err(|_| session_failure())?
        {
            let authenticated = if let Some(user) = session.user_id {
                db::users::accounts::get_user_by_id(&state.pool, user)
                    .await
                    .map_err(|_| database_failure())?
                    .is_some_and(|user| !user.is_banned)
            } else {
                false
            };
            return Ok((
                StatusCode::OK,
                [(
                    header::SET_COOKIE,
                    session::build_session_cookie(&id, session::session_duration_days()),
                )],
                Json(SessionResponse {
                    session_id: id,
                    authenticated,
                }),
            ));
        }
    }
    let id = generate_session_id();
    stored::create_browser_session(
        &state.pool,
        &id,
        extract_user_agent(&headers).as_deref(),
        None,
        Utc::now() + Duration::days(session::session_duration_days()),
    )
    .await
    .map_err(|_| session_failure())?;
    let cookie = session::build_session_cookie(&id, session::session_duration_days());
    Ok((
        StatusCode::CREATED,
        [(header::SET_COOKIE, cookie)],
        Json(SessionResponse {
            session_id: id,
            authenticated: false,
        }),
    ))
}

pub(crate) struct LoginSessionRequest {
    old: Option<String>,
    invalid_initiator: bool,
    fresh: String,
    user_agent: Option<String>,
}

impl LoginSessionRequest {
    pub(crate) fn from_headers(headers: &HeaderMap) -> Self {
        let old = extract_session_id(headers);
        let invalid_initiator = old.is_none() && session::has_session_cookie(headers);
        Self {
            old,
            invalid_initiator,
            fresh: generate_session_id(),
            user_agent: extract_user_agent(headers),
        }
    }

    pub(crate) fn context(&self) -> identity::sessions::SessionContext<'_> {
        use identity::sessions::{InitiatingSession, SessionContext};
        let initiating = if self.invalid_initiator {
            InitiatingSession::Invalid
        } else if let Some(old) = self.old.as_deref() {
            InitiatingSession::Supplied(old)
        } else {
            InitiatingSession::Missing
        };
        SessionContext {
            initiating,
            fresh_id: &self.fresh,
            user_agent: self.user_agent.as_deref(),
            expires_at: Utc::now() + Duration::days(session::session_duration_days()),
        }
    }
}
