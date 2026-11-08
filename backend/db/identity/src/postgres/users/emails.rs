use crate::email::secondary_link_condition;
use crate::users::emails::EmailAddress;
use crate::users::emails::EmailRefusal;
use anyhow::Result;
use codeyard_database::pool::PgPool;
use codeyard_database::queries::users::accounts::AccountCredential;

const LIST_EMAILS: &str = "SELECT e.email,e.verified FROM user_emails e JOIN users u ON u.id=e.user_id WHERE e.user_id=$1 AND lower(e.email)<>lower(COALESCE(u.email,'')) ORDER BY e.email";

pub async fn list_emails(pool: &PgPool, user: i64) -> Result<Vec<EmailAddress>> {
    Ok(sqlx::query_as(LIST_EMAILS)
        .bind(user)
        .fetch_all(pool)
        .await?)
}

pub async fn list_account_emails(
    pool: &PgPool,
    user: i64,
    credential: &AccountCredential<'_>,
) -> Result<Vec<EmailAddress>> {
    let mut tx = pool.begin().await?;
    crate::postgres::users::transitions::require_account_exclusive(&mut tx, user, credential)
        .await?;
    let emails = sqlx::query_as(LIST_EMAILS)
        .bind(user)
        .fetch_all(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(emails)
}

pub async fn change_emails(
    pool: &PgPool,
    user: i64,
    credential: &AccountCredential<'_>,
    emails: &[String],
    add: bool,
) -> Result<()> {
    let mut tx = pool.begin().await?;
    crate::postgres::users::transitions::require_account_exclusive(&mut tx, user, credential)
        .await?;
    // Lock the complete address set before changing any, matching the primary trigger.
    let addresses: Vec<&str> = emails.iter().map(String::as_str).collect();
    crate::postgres::users::transitions::lock_emails(&mut tx, &addresses).await?;
    let primary: Option<String> = sqlx::query_scalar("SELECT email FROM users WHERE id=$1")
        .bind(user)
        .fetch_one(&mut *tx)
        .await?;
    for email in emails {
        if primary
            .as_deref()
            .is_some_and(|value| value.eq_ignore_ascii_case(email))
        {
            anyhow::ensure!(add, EmailRefusal::Primary);
            continue;
        }
        if add {
            let taken: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM users WHERE lower(email)=lower($1) AND id<>$2) OR EXISTS(SELECT 1 FROM user_emails WHERE lower(email)=lower($1) AND user_id<>$2)")
                .bind(email).bind(user).fetch_one(&mut *tx).await?;
            anyhow::ensure!(!taken, EmailRefusal::Taken);
            sqlx::query(
                "INSERT INTO user_emails(user_id,email) VALUES($1,$2) ON CONFLICT DO NOTHING",
            )
            .bind(user)
            .bind(email)
            .execute(&mut *tx)
            .await?;
        } else {
            sqlx::query("DELETE FROM user_emails WHERE user_id=$1 AND lower(email)=lower($2)")
                .bind(user)
                .bind(email)
                .execute(&mut *tx)
                .await?;
            sqlx::query("UPDATE user_profile_settings SET public_email='' WHERE user_id=$1 AND lower(public_email)=lower($2)").bind(user).bind(email).execute(&mut *tx).await?;
        }
    }
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM user_emails WHERE user_id=$1")
        .bind(user)
        .fetch_one(&mut *tx)
        .await?;
    anyhow::ensure!(count <= 100, EmailRefusal::Limit);
    let keys = if let Some(directory) = pool.directory() {
        let config = directory.config();
        config.validate()?;
        anyhow::ensure!(emails.len() <= config.maximum_keys, EmailRefusal::Limit);
        emails
            .iter()
            .map(|email| {
                codeyard_types::cells::Key::new(codeyard_types::cells::KeyKind::Email, email)
            })
            .collect::<Result<Vec<_>>>()?
    } else {
        Vec::new()
    };
    if add && pool.directory().is_some() {
        anyhow::ensure!(
            codeyard_database::cells::directory::postgres::user_id(pool, &mut tx, user, &keys)
                .await?,
            EmailRefusal::Taken
        );
        crate::postgres::users::transitions::require_account_exclusive(&mut tx, user, credential)
            .await?;
    }
    if !add && !keys.is_empty() {
        codeyard_database::cells::outbox::postgres::release(
            pool,
            &mut tx,
            codeyard_types::cells::Object {
                kind: codeyard_types::cells::ObjectKind::User,
                id: user,
            },
            &keys,
        )
        .await?;
    }
    tx.commit().await?;
    if !add && !keys.is_empty() {
        codeyard_database::cells::directory::postgres::finish_release(
            pool,
            codeyard_types::cells::Object {
                kind: codeyard_types::cells::ObjectKind::User,
                id: user,
            },
            &keys,
        )
        .await;
    }
    Ok(())
}

/// Stores the code for a secondary address and queues its mail, in one transaction. False when
/// the address was sent a code less than a minute ago, and nothing is queued.
pub async fn save_verification(
    pool: &PgPool,
    user: i64,
    email: &str,
    hash: &str,
    subject: &str,
    body_text: &str,
) -> Result<bool> {
    let mut tx = pool.begin().await?;
    if !crate::postgres::email::lock_link_recipient(&mut tx, user).await? {
        return Ok(false);
    }
    let saved = sqlx::query("UPDATE user_emails SET verification_hash=$3,verification_expires_at=clock_timestamp()+INTERVAL '1 day',last_sent_at=clock_timestamp() WHERE user_id=$1 AND lower(email)=lower($2) AND NOT verified AND (last_sent_at IS NULL OR last_sent_at<clock_timestamp()-INTERVAL '60 seconds') AND EXISTS(SELECT 1 FROM users WHERE id=$1 AND NOT is_banned AND NOT must_change_password)")
        .bind(user).bind(email).bind(hash).execute(&mut *tx).await?.rows_affected()==1;
    if saved {
        sqlx::query("DELETE FROM email_outbox WHERE link_user_id=$1 AND link_purpose='secondary' AND lower(to_address)=lower($2) AND sent_at IS NULL")
            .bind(user).bind(email).execute(&mut *tx).await?;
        sqlx::query("INSERT INTO email_outbox(to_address,subject,body_text,link_user_id,link_purpose,link_token_hash) VALUES ($1,$2,$3,$4,'secondary',$5)")
            .bind(email).bind(subject).bind(body_text).bind(user).bind(hash)
            .execute(&mut *tx).await?;
    }
    tx.commit().await?;
    Ok(saved)
}

pub async fn verification_valid(pool: &PgPool, hash: &str) -> Result<bool> {
    Ok(sqlx::query_scalar(sqlx::AssertSqlSafe(format!("SELECT EXISTS(SELECT 1 FROM user_emails e JOIN users u ON u.id=e.user_id WHERE e.verification_hash=$1 AND {})", secondary_link_condition(true))))
        .bind(hash).fetch_one(pool).await?)
}

pub async fn verify_email(pool: &PgPool, hash: &str) -> Result<bool> {
    Ok(sqlx::query("UPDATE user_emails SET verified=TRUE,verification_hash=NULL,verification_expires_at=NULL WHERE verification_hash=$1 AND verification_expires_at>clock_timestamp() AND NOT verified AND EXISTS(SELECT 1 FROM users WHERE id=user_emails.user_id AND NOT is_banned)").bind(hash).execute(pool).await?.rows_affected()==1)
}
