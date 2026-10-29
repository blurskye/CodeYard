use crate::backend::api::http::session;
use crate::backend::api::http::session::{
    extract_session_id, extract_user_agent, generate_session_id,
};
use crate::backend::identity;
use axum::http::HeaderMap;
use chrono::{Duration, Utc};
pub(crate) struct LoginSessionRequest {
    old: Option<String>,
    invalid_initiator: bool,
    fresh: String,
    user_agent: Option<String>,
    client: Option<String>,
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
            client: session::client_address(headers).map(str::to_owned),
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
            client: self.client.as_deref(),
            expires_at: Utc::now() + Duration::days(session::session_duration_days()),
        }
    }
}
