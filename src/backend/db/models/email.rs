use sqlx::FromRow;

/// The saved mail server. The password stays encrypted; only `backend::email` opens it.
#[derive(Debug, Clone, FromRow)]
pub struct EmailSettings {
    pub host: String,
    pub port: i32,
    pub security: String,
    pub username: String,
    pub encrypted_password: Option<Vec<u8>>,
    pub password_nonce: Option<Vec<u8>>,
    pub from_name: String,
    pub from_address: String,
    pub reply_to: String,
}

/// A claimed outbox row. `attempts` counts this attempt.
#[derive(Debug, Clone, FromRow)]
pub struct OutboxMessage {
    pub id: i64,
    pub to_address: String,
    pub subject: String,
    pub body_text: String,
    pub attempts: i32,
}
