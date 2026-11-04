#[derive(Clone, Debug, sqlx::FromRow)]
pub struct Token {
    pub token_id: i64,
    pub client_id: String,
    pub scopes: String,
}

/// Native SQL fragments only: client authentication and token use share issuer liveness.
pub fn client_condition(client_sql: &str) -> String {
    format!("EXISTS(SELECT 1 FROM oauth_applications client JOIN users owner ON owner.id=client.owner_id
        WHERE client.client_id={client_sql} AND NOT owner.is_banned AND NOT owner.must_change_password
        AND (NOT client.is_github_app OR EXISTS(
            SELECT 1 FROM github_apps app JOIN users bot ON bot.id=app.bot_id JOIN users app_owner ON app_owner.id=app.owner_id
            WHERE app.client_id=client.client_id AND NOT bot.is_banned AND NOT bot.must_change_password
            AND NOT app_owner.is_banned AND NOT app_owner.must_change_password)))")
}

pub fn token_issuer_condition(token_sql: &str, cli_sql: &str) -> String {
    format!(
        "NOT EXISTS(SELECT 1 FROM oauth_app_tokens issued WHERE issued.token_id={token_sql}.id
        AND issued.client_id<>{cli_sql} AND NOT ({}))",
        client_condition("issued.client_id")
    )
}
