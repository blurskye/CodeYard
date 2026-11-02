use chrono::{DateTime, Utc};
use sqlx::FromRow;

/// A webhook as stored. The secret stays encrypted; only the delivery path opens it.
#[derive(Debug, Clone, FromRow)]
pub struct Webhook {
    pub username: Option<String>,
    pub encrypted_password: Option<Vec<u8>>,
    pub password_nonce: Option<Vec<u8>>,
    pub id: i64,
    /// App hooks have a separate store because native hooks have exactly one repo/org owner.
    #[sqlx(default)]
    pub app_id: Option<i64>,
    pub repo_id: Option<i64>,
    pub org_id: Option<i64>,
    pub payload_url: String,
    pub content_type: String,
    pub encrypted_secret: Option<Vec<u8>>,
    pub secret_nonce: Option<Vec<u8>>,
    /// Comma-separated event names, or `*` for every event.
    pub events: String,
    pub active: bool,
    pub ssl_verify: bool,
    pub created_by_id: Option<i64>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// One recorded delivery attempt. The headers and body are what was sent; the response
/// fields are empty when no response came back.
#[derive(Debug, Clone, FromRow)]
pub struct WebhookDelivery {
    pub redelivery: bool,
    pub id: i64,
    pub webhook_id: i64,
    pub guid: String,
    pub event: String,
    pub action: Option<String>,
    pub status_code: Option<i32>,
    pub duration_ms: i32,
    pub error: Option<String>,
    pub request_headers: String,
    pub request_body: String,
    pub response_headers: Option<String>,
    pub response_body: Option<String>,
    pub delivered_at: DateTime<Utc>,
}

impl WebhookDelivery {
    /// A delivery succeeded when the receiver answered 2xx, as GitHub counts it. Redirects are
    /// not followed, so a 3xx answer never reached the receiver; no answer at all is a failure.
    pub fn succeeded(&self) -> bool {
        self.status_code
            .is_some_and(|code| (200..300).contains(&code))
    }
}
