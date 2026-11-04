#[derive(Clone, Debug, sqlx::FromRow)]
pub struct Token {
    pub token_id: i64,
    pub installation_id: i64,
    pub permissions: String,
    pub repositories: String,
    pub user_token: bool,
}
