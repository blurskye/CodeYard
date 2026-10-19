use chrono::{DateTime, Utc};
use sqlx::FromRow;

/// A webhook as stored. The secret stays encrypted; only the delivery path opens it.
#[derive(Debug, Clone, FromRow)]
pub struct Webhook {
    pub id: i64,
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
