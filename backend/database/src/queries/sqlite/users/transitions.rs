use anyhow::Result;
pub async fn live_session(
    connection: &mut sqlx::SqliteConnection,
    actor: i64,
    session: &str,
) -> Result<bool> {
    let live: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM sessions WHERE id=?1 AND user_id=?2 AND expires_at>strftime('%Y-%m-%dT%H:%M:%f000Z','now'))")
        .bind(session).bind(actor).fetch_one(&mut *connection).await?;
    if live {
        return Ok(true);
    }
    // Native session rows cannot belong to stubs; a foreign session needs a current home proof.
    let hash = codeyard_security::token_hash::token_hash(session);
    Ok(crate::cells::authorization::sqlite(
        connection,
        &crate::cells::authorization::Query {
            kind: "session",
            id: None,
            hash: Some(&hash),
            write: true,
        },
    )
    .await?
    .is_some_and(|credential| credential.user.id == actor))
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
            sqlx::query_scalar(sqlx::AssertSqlSafe(format!("SELECT EXISTS(SELECT 1 FROM personal_access_tokens t WHERE t.id=?1 AND t.user_id=?2 AND (NOT ?4 OR t.scope='write') AND t.token_hash=?3 AND (t.expires_at IS NULL OR t.expires_at>strftime('%Y-%m-%dT%H:%M:%f000Z','now')) AND {issuer})")))
                .bind(id).bind(actor).bind(hash).bind(write).bind(codeyard_types::identity::CLI_CLIENT).fetch_one(&mut *tx).await?
        }
    })
}

/// BEGIN IMMEDIATE already serializes native key writers with directory cleanup.
pub async fn lock_directory_keys(
    _connection: &mut sqlx::SqliteConnection,
    _keys: &[codeyard_types::cells::Key],
) -> Result<()> {
    Ok(())
}
