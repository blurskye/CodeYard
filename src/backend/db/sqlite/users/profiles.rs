use crate::backend::db::{
    models::User,
    users::{
        accounts::AccountCredential,
        profiles::{InvalidPublicEmail, ProfilePatch, ProfileSettings},
    },
};
use anyhow::Result;
use sqlx::SqlitePool;

pub async fn account_statistics(
    pool: &SqlitePool,
    user: i64,
) -> Result<crate::backend::db::users::profiles::AccountStatistics> {
    let readable = super::super::repositories::access::readable_condition();
    Ok(sqlx::query_as(&format!("SELECT
        (SELECT COUNT(*) FROM gists g JOIN repositories r ON r.id=g.repo_id WHERE r.owner_user_id=?1 AND g.ready AND g.is_public) AS public_gists,
        (SELECT COUNT(*) FROM gists g JOIN repositories r ON r.id=g.repo_id WHERE r.owner_user_id=?1 AND g.ready AND NOT g.is_public) AS private_gists,
        (SELECT COUNT(*) FROM repositories WHERE owner_kind='user' AND owner_user_id=?1 AND is_private) AS owned_private_repos,
        (SELECT COUNT(*) FROM repositories r JOIN users u ON u.id=?1 WHERE r.is_private AND {readable}) AS total_private_repos,
        (SELECT CAST(COALESCE(SUM(size_bytes),0)/1024 AS BIGINT) FROM repositories WHERE owner_kind='user' AND owner_user_id=?1) AS disk_usage,
        (SELECT COUNT(DISTINCT a.user_id) FROM repo_access a JOIN repositories r ON r.id=a.repo_id WHERE r.owner_kind='user' AND r.owner_user_id=?1 AND r.is_private AND a.user_id<>?1 AND a.access_level>=1) AS collaborators"))
        .bind(user).fetch_one(pool).await?)
}

pub async fn get_profile_settings(pool: &SqlitePool, user: i64) -> Result<ProfileSettings> {
    Ok(sqlx::query_as("SELECT p.company,p.twitter_username,p.hireable,p.public_email,p.primary_visibility,EXISTS(SELECT 1 FROM user_emails e WHERE e.user_id=p.user_id AND lower(e.email)=lower(p.public_email) AND e.verified) AS secondary_email_verified FROM user_profile_settings p WHERE p.user_id=?1")
        .bind(user).fetch_optional(pool).await?.unwrap_or_else(||ProfileSettings {primary_visibility:"private".into(), ..ProfileSettings::default()}))
}

pub async fn save_profile(
    pool: &SqlitePool,
    user: i64,
    credential: &AccountCredential<'_>,
    input: &ProfilePatch<'_>,
) -> Result<User> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    super::transitions::require_account(&mut tx, user, credential).await?;
    let current: User = sqlx::query_as("SELECT * FROM users WHERE id=?1")
        .bind(user)
        .fetch_one(&mut *tx)
        .await?;
    if let Some(email) = input.email.filter(|email| !email.is_empty()) {
        let secondary: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM user_emails WHERE user_id=?1 AND lower(email)=lower(?2) AND verified)").bind(user).bind(email).fetch_one(&mut *tx).await?;
        anyhow::ensure!(
            secondary
                || (current.email_verified
                    && current
                        .email
                        .as_deref()
                        .is_some_and(|value| value.eq_ignore_ascii_case(email))),
            InvalidPublicEmail
        );
    }
    let result = sqlx::query_as("UPDATE users SET display_name=COALESCE(?2,display_name),bio=COALESCE(?3,bio),location=COALESCE(?4,location),website=COALESCE(?5,website),updated_at=strftime('%Y-%m-%dT%H:%M:%f000Z','now') WHERE id=?1 RETURNING *")
        .bind(user).bind(input.display_name).bind(input.bio).bind(input.location).bind(input.website).fetch_one(&mut *tx).await?;
    sqlx::query("INSERT INTO user_profile_settings(user_id) VALUES(?1) ON CONFLICT DO NOTHING")
        .bind(user)
        .execute(&mut *tx)
        .await?;
    sqlx::query("UPDATE user_profile_settings SET company=COALESCE(?2,company),twitter_username=COALESCE(?3,twitter_username),hireable=COALESCE(?4,hireable),public_email=COALESCE(?5,public_email),primary_visibility=COALESCE(?6,CASE WHEN ?5='' THEN 'private' WHEN lower(?5)=(SELECT lower(email) FROM users WHERE id=?1) THEN 'public' ELSE primary_visibility END) WHERE user_id=?1")
        .bind(user).bind(input.company).bind(input.twitter_username).bind(input.hireable).bind(input.email).bind(input.primary_visibility).execute(&mut *tx).await?;
    tx.commit().await?;
    Ok(result)
}
