use crate::pool::PgPool;
use crate::queries::oauth_apps::Token;
use anyhow::Result;
use sqlx::PgConnection;
/// Credentials discover their issuer after resource locks. Do not wait in the
/// reverse account order: a concurrent issuer restriction makes this effect retry.
pub(crate) async fn lock_token_issuer(connection: &mut PgConnection, token: i64) -> Result<()> {
    sqlx::query("SELECT id FROM users WHERE id IN (SELECT c.owner_id FROM oauth_app_tokens t JOIN oauth_applications c ON c.client_id=t.client_id WHERE t.token_id=$1 UNION SELECT a.owner_id FROM github_app_tokens t JOIN github_app_installations i ON i.id=t.installation_id JOIN github_apps a ON a.id=i.app_id WHERE t.token_id=$1 UNION SELECT a.bot_id FROM github_app_tokens t JOIN github_app_installations i ON i.id=t.installation_id JOIN github_apps a ON a.id=i.app_id WHERE t.token_id=$1 UNION SELECT a.owner_id FROM oauth_app_tokens t JOIN github_apps a ON a.client_id=t.client_id WHERE t.token_id=$1 UNION SELECT a.bot_id FROM oauth_app_tokens t JOIN github_apps a ON a.client_id=t.client_id WHERE t.token_id=$1) ORDER BY id FOR SHARE NOWAIT")
        .bind(token).execute(connection).await?;
    Ok(())
}

pub async fn token(pool: &PgPool, id: i64) -> Result<Option<Token>> {
    Ok(
        sqlx::query_as("SELECT * FROM oauth_app_tokens WHERE token_id=$1")
            .bind(id)
            .fetch_optional(pool)
            .await?,
    )
}
