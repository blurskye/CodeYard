use anyhow::Result;
pub async fn live_session(
    connection: &mut sqlx::SqliteConnection,
    actor: i64,
    session: &str,
) -> Result<bool> {
    Ok(sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM sessions WHERE id=?1 AND user_id=?2 AND expires_at>strftime('%Y-%m-%dT%H:%M:%f000Z','now'))")
        .bind(session).bind(actor).fetch_one(connection).await?)
}

pub async fn credential_live(
    tx: &mut sqlx::SqliteConnection,
    actor: i64,
    credential: &crate::queries::users::accounts::AccountCredential<'_>,
    write: bool,
) -> Result<bool> {
    use crate::queries::users::accounts::AccountCredential;
    Ok(match credential {
        AccountCredential::Session(session) => live_session(tx, actor, session).await?,
        AccountCredential::ForeignToken(id, hash) => crate::cells::authorization::sqlite(
            tx,
            &crate::cells::authorization::Query {
                kind: "token",
                id: Some(*id),
                hash: Some(hash),
                write,
            },
        )
        .await?
        .is_some_and(|credential| credential.user.id == actor),
        AccountCredential::Token(id, hash) => {
            // A token issued to an OAuth app ends with that app's availability.
            let issuer = crate::queries::oauth_apps::token_issuer_condition("t", "?5");
            sqlx::query_scalar(&format!("SELECT EXISTS(SELECT 1 FROM personal_access_tokens t WHERE t.id=?1 AND t.user_id=?2 AND (NOT ?4 OR t.scope='write') AND t.token_hash=?3 AND (t.expires_at IS NULL OR t.expires_at>strftime('%Y-%m-%dT%H:%M:%f000Z','now')) AND {issuer})"))
                .bind(id).bind(actor).bind(hash).bind(write).bind(codeyard_model::identity::CLI_CLIENT).fetch_one(&mut *tx).await?
        }
    })
}
