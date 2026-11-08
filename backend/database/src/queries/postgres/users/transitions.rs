use anyhow::Result;
use sqlx::PgConnection;
pub async fn lock_accounts_shared(connection: &mut PgConnection, accounts: &[i64]) -> Result<()> {
    account_locks(connection, accounts, true).await
}

pub async fn account_locks(
    connection: &mut PgConnection,
    accounts: &[i64],
    shared: bool,
) -> Result<()> {
    let mut accounts = accounts.to_vec();
    accounts.sort_unstable();
    accounts.dedup();
    let lock = if shared {
        "pg_advisory_xact_lock_shared"
    } else {
        "pg_advisory_xact_lock"
    };
    for account in accounts {
        sqlx::query(sqlx::AssertSqlSafe(format!("SELECT {lock}(hashtextextended(COALESCE(current_schema(),'') || ':account:' || $1::bigint::text,0))")))
            .bind(account).execute(&mut *connection).await?;
    }
    Ok(())
}

/// The account row is locked by the caller before retaining its session.
pub async fn live_session(
    connection: &mut PgConnection,
    actor: i64,
    session: &str,
) -> Result<bool> {
    let expiry: Option<chrono::DateTime<chrono::Utc>> =
        sqlx::query_scalar("SELECT expires_at FROM sessions WHERE id=$1 AND user_id=$2 FOR SHARE")
            .bind(session)
            .bind(actor)
            .fetch_optional(&mut *connection)
            .await?;
    let live: bool = sqlx::query_scalar("SELECT COALESCE($1::timestamptz>clock_timestamp(),FALSE)")
        .bind(expiry)
        .fetch_one(&mut *connection)
        .await?;
    if live {
        return Ok(true);
    }
    // Native session rows cannot belong to stubs; a foreign session needs a current home proof.
    let hash = codeyard_security::token_hash::token_hash(session);
    Ok(crate::cells::authorization::postgres(
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

/// The caller retains account/role authority before locking its originating credential.
pub async fn credential_live(
    tx: &mut sqlx::PgConnection,
    actor: i64,
    credential: &crate::queries::users::accounts::AccountCredential<'_>,
    write: bool,
) -> Result<bool> {
    use crate::queries::users::accounts::AccountCredential;
    Ok(match credential {
        AccountCredential::Session(session) => live_session(tx, actor, session).await?,
        AccountCredential::ForeignToken(id, hash) => crate::cells::authorization::postgres(
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
            crate::queries::postgres::oauth_apps::lock_token_issuer(tx, *id).await?;
            // Installation revocation takes this row before deleting its personal tokens.
            let _: Option<i64> = sqlx::query_scalar("SELECT i.id FROM github_app_tokens t JOIN github_app_installations i ON i.id=t.installation_id WHERE t.token_id=$1 FOR SHARE OF i")
                .bind(id).fetch_optional(&mut *tx).await?;
            // A token issued to an OAuth app ends with that app's availability.
            let issuer = crate::queries::oauth_apps::token_issuer_condition("t", "$5");
            let expiry: Option<Option<chrono::DateTime<chrono::Utc>>> = sqlx::query_scalar(sqlx::AssertSqlSafe(format!("SELECT t.expires_at FROM personal_access_tokens t WHERE t.id=$1 AND t.user_id=$2 AND (NOT $4 OR t.scope='write') AND t.token_hash=$3 AND {issuer} FOR SHARE OF t")))
                .bind(id).bind(actor).bind(hash).bind(write).bind(codeyard_types::identity::CLI_CLIENT).fetch_optional(&mut *tx).await?;
            match expiry {
                Some(None) => true,
                Some(Some(expiry)) => {
                    sqlx::query_scalar("SELECT $1::timestamptz>clock_timestamp()")
                        .bind(expiry)
                        .fetch_one(&mut *tx)
                        .await?
                }
                None => false,
            }
        }
    })
}

/// Shared by personal/deploy-key enrollment and post-commit directory cleanup.
pub async fn lock_fingerprints(connection: &mut PgConnection, fingerprints: &[&str]) -> Result<()> {
    let keys: Vec<i64> = sqlx::query_scalar("SELECT DISTINCT hashtextextended(fingerprint,37000003) AS key FROM unnest($1::text[]) fingerprint ORDER BY key")
        .bind(fingerprints).fetch_all(&mut *connection).await?;
    for key in keys {
        sqlx::query("SELECT pg_advisory_xact_lock($1)")
            .bind(key)
            .execute(&mut *connection)
            .await?;
    }
    Ok(())
}

/// Primary and secondary email writers retain these locks through their committing transaction.
pub async fn lock_emails(connection: &mut PgConnection, emails: &[&str]) -> Result<()> {
    let keys: Vec<i64> = sqlx::query_scalar(
        "SELECT DISTINCT hashtextextended(COALESCE(current_schema(),'') || ':email:' || lower(email),0) AS key FROM unnest($1::text[]) email ORDER BY key",
    ).bind(emails).fetch_all(&mut *connection).await?;
    for key in keys {
        sqlx::query("SELECT pg_advisory_xact_lock($1)")
            .bind(key)
            .execute(&mut *connection)
            .await?;
    }
    Ok(())
}

pub async fn lock_directory_keys(
    connection: &mut PgConnection,
    keys: &[codeyard_types::cells::Key],
) -> Result<()> {
    use codeyard_types::cells::KeyKind;
    let emails: Vec<_> = keys
        .iter()
        .filter(|key| key.kind() == KeyKind::Email)
        .map(|key| key.value())
        .collect();
    let fingerprints: Vec<_> = keys
        .iter()
        .filter(|key| key.kind() == KeyKind::Ssh)
        .map(|key| key.value())
        .collect();
    lock_emails(connection, &emails).await?;
    lock_fingerprints(connection, &fingerprints).await
}
