use crate::backend::db::oauth_apps::{Application, Exchange, Grant, Token};
use crate::backend::identity::{oauth_apps, tokens::token_hash};
use anyhow::Result;
use chrono::Utc;
use sqlx::SqlitePool;

#[expect(
    clippy::too_many_arguments,
    reason = "Registration stores the explicit validated application fields."
)]
pub async fn register(
    pool: &SqlitePool,
    actor: i64,
    session: &str,
    client_id: &str,
    secret_hash: &str,
    name: &str,
    homepage: &str,
    redirect_uri: &str,
) -> Result<Option<Application>> {
    let mut tx = pool.begin().await?;

    let app = sqlx::query_as("INSERT INTO oauth_applications(owner_id,client_id,secret_hash,name,homepage,redirect_uri) SELECT id,$3,$4,$5,$6,$7 FROM users WHERE id=$1 AND NOT is_banned AND NOT must_change_password AND EXISTS(SELECT 1 FROM sessions WHERE id=$2 AND user_id=$1 AND expires_at>strftime('%Y-%m-%dT%H:%M:%f000Z','now')) RETURNING *")
        .bind(actor).bind(session).bind(client_id).bind(secret_hash).bind(name).bind(homepage).bind(redirect_uri).fetch_optional(&mut *tx).await?;
    tx.commit().await?;
    Ok(app)
}

pub async fn application(pool: &SqlitePool, client_id: &str) -> Result<Option<Application>> {
    Ok(
        sqlx::query_as("SELECT * FROM oauth_applications WHERE client_id=$1")
            .bind(client_id)
            .fetch_optional(pool)
            .await?,
    )
}

pub async fn applications(pool: &SqlitePool, actor: i64) -> Result<Vec<Application>> {
    Ok(
        sqlx::query_as("SELECT * FROM oauth_applications WHERE owner_id=$1 ORDER BY id")
            .bind(actor)
            .fetch_all(pool)
            .await?,
    )
}

pub async fn save_grant(pool: &SqlitePool, grant: &Grant) -> Result<()> {
    let mut tx = pool.begin().await?;
    sqlx::query("DELETE FROM oauth_app_grants WHERE expires_at<$1")
        .bind(Utc::now().timestamp())
        .execute(&mut *tx)
        .await?;
    sqlx::query("INSERT INTO oauth_app_grants(hash,client_id,user_code,scopes,expires_at,user_id,session_id,kind,redirect_uri,state,challenge) VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11)")
        .bind(&grant.hash).bind(&grant.client_id).bind(&grant.user_code).bind(&grant.scopes).bind(grant.expires_at).bind(grant.user_id).bind(&grant.session_id).bind(&grant.kind).bind(&grant.redirect_uri).bind(&grant.state).bind(&grant.challenge).execute(&mut *tx).await?;
    tx.commit().await?;
    Ok(())
}

pub async fn device_grant(pool: &SqlitePool, code: &str) -> Result<Option<Grant>> {
    Ok(sqlx::query_as("SELECT * FROM oauth_app_grants WHERE user_code=$1 AND kind='device' AND user_id IS NULL AND NOT denied AND expires_at>$2").bind(code).bind(Utc::now().timestamp()).fetch_optional(pool).await?)
}

pub async fn approve(
    pool: &SqlitePool,
    code: &str,
    actor: i64,
    session: &str,
    denied: bool,
) -> Result<bool> {
    let mut tx = pool.begin().await?;

    let changed = sqlx::query("UPDATE oauth_app_grants SET user_id=$2,session_id=$3,denied=$4 WHERE user_code=$1 AND kind='device' AND user_id IS NULL AND NOT denied AND expires_at>$5 AND EXISTS(SELECT 1 FROM users u JOIN sessions s ON s.user_id=u.id WHERE u.id=$2 AND s.id=$3 AND NOT u.is_banned AND NOT u.must_change_password AND s.expires_at>strftime('%Y-%m-%dT%H:%M:%f000Z','now'))")
        .bind(code).bind(actor).bind(session).bind(denied).bind(Utc::now().timestamp()).execute(&mut *tx).await?.rows_affected()==1;
    tx.commit().await?;
    Ok(changed)
}

#[expect(
    clippy::too_many_arguments,
    reason = "OAuth exchange binds each credential to the original authorization request."
)]
pub async fn exchange(
    pool: &SqlitePool,
    hash: &str,
    client_id: &str,
    kind: &str,
    redirect_uri: Option<&str>,
    verifier: &str,
    state: Option<&str>,
    value: &str,
) -> Result<Exchange> {
    let mut tx = pool.begin().await?;
    sqlx::query("UPDATE oauth_app_grants SET last_poll=last_poll WHERE hash=$1")
        .bind(hash)
        .execute(&mut *tx)
        .await?;
    let grant: Option<Grant> =
        sqlx::query_as("SELECT * FROM oauth_app_grants WHERE hash=$1 AND client_id=$2 ")
            .bind(hash)
            .bind(client_id)
            .fetch_optional(&mut *tx)
            .await?;
    let Some(grant) = grant else {
        return Ok(Exchange::Invalid);
    };
    let now = Utc::now().timestamp();
    if grant.kind != kind {
        return Ok(Exchange::Invalid);
    }
    if grant.expires_at <= now {
        return Ok(Exchange::Expired);
    }
    if grant.denied {
        return Ok(Exchange::Denied);
    }
    if grant.kind == "web"
        && (redirect_uri.is_some_and(|uri| uri != grant.redirect_uri)
            || state.is_some_and(|state| state != grant.state)
            || !oauth_apps::verify_challenge(&grant.challenge, verifier))
    {
        return Ok(Exchange::Invalid);
    }
    if grant.kind == "device" {
        let interval = grant.interval
            + if grant.last_poll > 0 && now - grant.last_poll < grant.interval {
                5
            } else {
                0
            };
        sqlx::query("UPDATE oauth_app_grants SET last_poll=$2,interval=$3 WHERE hash=$1")
            .bind(hash)
            .bind(now)
            .bind(interval)
            .execute(&mut *tx)
            .await?;
        if interval != grant.interval {
            tx.commit().await?;
            return Ok(Exchange::SlowDown(interval));
        }
    }
    let Some(actor) = grant.user_id else {
        tx.commit().await?;
        return Ok(Exchange::Pending);
    };

    let token_id: Option<i64> = sqlx::query_scalar("INSERT INTO personal_access_tokens(user_id,name,token_hash,scope) SELECT u.id,'OAuth: ' || $3,$4,CASE WHEN EXISTS(SELECT 1 FROM github_apps WHERE client_id=$3 AND access_level>=3) THEN 'write' ELSE $5 END FROM users u JOIN sessions s ON s.user_id=u.id WHERE u.id=$1 AND s.id=$2 AND NOT u.is_banned AND NOT u.must_change_password AND s.expires_at>strftime('%Y-%m-%dT%H:%M:%f000Z','now') RETURNING id")
        .bind(actor).bind(&grant.session_id).bind(client_id).bind(token_hash(value)).bind(if oauth_apps::write_scope(&grant.scopes) { "write" } else { "read" }).fetch_optional(&mut *tx).await?;
    let Some(token_id) = token_id else {
        return Ok(Exchange::Denied);
    };
    sqlx::query("INSERT INTO oauth_app_tokens(token_id,client_id,scopes) VALUES($1,$2,$3)")
        .bind(token_id)
        .bind(client_id)
        .bind(&grant.scopes)
        .execute(&mut *tx)
        .await?;
    sqlx::query("DELETE FROM oauth_app_grants WHERE hash=$1")
        .bind(hash)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(Exchange::Token(grant.scopes))
}

pub async fn token(pool: &SqlitePool, id: i64) -> Result<Option<Token>> {
    Ok(
        sqlx::query_as("SELECT * FROM oauth_app_tokens WHERE token_id=$1")
            .bind(id)
            .fetch_optional(pool)
            .await?,
    )
}

pub async fn owned_token(
    pool: &SqlitePool,
    client: &str,
    hash: &str,
) -> Result<Option<crate::backend::db::models::PersonalAccessToken>> {
    Ok(sqlx::query_as("SELECT t.id,t.user_id,t.name,t.scope,t.expires_at,t.created_at FROM personal_access_tokens t JOIN oauth_app_tokens o ON o.token_id=t.id JOIN users u ON u.id=t.user_id WHERE o.client_id=$1 AND t.token_hash=$2 AND (t.expires_at IS NULL OR t.expires_at>strftime('%Y-%m-%dT%H:%M:%f000Z','now')) AND NOT u.is_banned AND NOT u.must_change_password")
        .bind(client).bind(hash).fetch_optional(pool).await?)
}

pub async fn reset_token(
    pool: &SqlitePool,
    client: &str,
    hash: &str,
    new_hash: &str,
) -> Result<Option<crate::backend::db::models::PersonalAccessToken>> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let old: Option<crate::backend::db::models::PersonalAccessToken> = sqlx::query_as("SELECT t.id,t.user_id,t.name,t.scope,t.expires_at,t.created_at FROM personal_access_tokens t JOIN oauth_app_tokens o ON o.token_id=t.id JOIN users u ON u.id=t.user_id WHERE o.client_id=$1 AND t.token_hash=$2 AND (t.expires_at IS NULL OR t.expires_at>strftime('%Y-%m-%dT%H:%M:%f000Z','now')) AND NOT u.is_banned AND NOT u.must_change_password")
        .bind(client).bind(hash).fetch_optional(&mut *tx).await?;
    let Some(old) = old else { return Ok(None); };
    let replacement: crate::backend::db::models::PersonalAccessToken = sqlx::query_as("INSERT INTO personal_access_tokens(user_id,name,scope,expires_at,token_hash) VALUES($1,$2,$3,$4,$5) RETURNING id,user_id,name,scope,expires_at,created_at")
        .bind(old.user_id).bind(&old.name).bind(&old.scope).bind(old.expires_at).bind(new_hash).fetch_one(&mut *tx).await?;
    sqlx::query("INSERT INTO oauth_app_tokens(token_id,client_id,scopes) SELECT $2,client_id,scopes FROM oauth_app_tokens WHERE token_id=$1")
        .bind(old.id).bind(replacement.id).execute(&mut *tx).await?;
    sqlx::query("INSERT INTO github_app_tokens(token_id,installation_id,permissions,repositories,user_token) SELECT $2,installation_id,permissions,repositories,user_token FROM github_app_tokens WHERE token_id=$1")
        .bind(old.id).bind(replacement.id).execute(&mut *tx).await?;
    sqlx::query("DELETE FROM personal_access_tokens WHERE id=$1").bind(old.id).execute(&mut *tx).await?;
    tx.commit().await?;
    Ok(Some(replacement))
}

pub async fn revoke(
    pool: &SqlitePool,
    client: &str,
    hash: &str,
    authorization: bool,
) -> Result<()> {
    let mut tx = pool.begin().await?;

    sqlx::query("UPDATE personal_access_tokens SET scope=scope WHERE token_hash=$1")
        .bind(hash)
        .execute(&mut *tx)
        .await?;
    let user: Option<i64>=sqlx::query_scalar("SELECT t.user_id FROM personal_access_tokens t JOIN oauth_app_tokens o ON o.token_id=t.id WHERE o.client_id=$1 AND t.token_hash=$2").bind(client).bind(hash).fetch_optional(&mut *tx).await?;
    if let Some(user) = user {
        if authorization {
            sqlx::query("DELETE FROM oauth_app_grants WHERE client_id=$1 AND user_id=$2")
                .bind(client)
                .bind(user)
                .execute(&mut *tx)
                .await?;
            sqlx::query("DELETE FROM personal_access_tokens WHERE user_id=$2 AND id IN (SELECT token_id FROM oauth_app_tokens WHERE client_id=$1)").bind(client).bind(user).execute(&mut *tx).await?;
        } else {
            sqlx::query("DELETE FROM personal_access_tokens WHERE token_hash=$2 AND id IN (SELECT token_id FROM oauth_app_tokens WHERE client_id=$1)").bind(client).bind(hash).execute(&mut *tx).await?;
        }
    }
    tx.commit().await?;
    Ok(())
}
