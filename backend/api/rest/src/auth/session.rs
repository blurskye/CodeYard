use axum::http::HeaderMap;
use chrono::Duration;
use chrono::Utc;
use codeyard_api_http::session::extract_session_id;
use codeyard_api_http::session::extract_user_agent;
use codeyard_api_http::session::generate_session_id;

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
        let invalid_initiator =
            old.is_none() && codeyard_api_http::session::has_session_cookie(headers);
        Self {
            old,
            invalid_initiator,
            fresh: generate_session_id(),
            user_agent: extract_user_agent(headers),
            client: codeyard_identity::client_address::client_address(headers)
                .map(str::to_owned),
        }
    }

    pub(crate) fn context(&self) -> codeyard_identity::sessions::SessionContext<'_> {
        use codeyard_identity::sessions::InitiatingSession;
        use codeyard_identity::sessions::SessionContext;
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
            expires_at: Utc::now()
                + Duration::days(codeyard_api_http::session::session_duration_days()),
        }
    }
}
