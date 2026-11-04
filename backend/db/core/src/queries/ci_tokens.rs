pub const BOT: &str = "github-actions[bot]";

#[derive(sqlx::FromRow)]
pub struct JobToken {
    pub run_id: i64,
    pub repo_id: i64,
    pub permissions: String,
}
