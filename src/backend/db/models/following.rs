use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::FromRow;

#[derive(Debug, Clone, FromRow, Serialize, Deserialize)]
pub struct Follow {
    pub follower_id: i64,
    pub following_id: i64,
    pub created_at: DateTime<Utc>,
}
