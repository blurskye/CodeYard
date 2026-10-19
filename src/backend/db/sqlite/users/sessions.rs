use crate::backend::db::models::{BrowserSession, OAuthState, Session, User};
use anyhow::Result;
use chrono::{DateTime, Duration, Utc};
use sqlx::SqlitePool;
use subtle::ConstantTimeEq;

pub async fn create_session(
    pool: &SqlitePool,
    session_id: &str,
    user_id: i64,
    user_agent: Option<&str>,
    ip_address: Option<&str>,
    expires_at: DateTime<Utc>,
) -> Result<Session> {
    let result = sqlx::query_as::<_, Session>(
        r#"
        INSERT INTO sessions (id, user_id, user_agent, ip_address, expires_at)
        VALUES (?1, ?2, ?3, ?4, ?5)
        RETURNING id,user_id,user_agent,ip_address,expires_at,created_at
        "#,
    )
    .bind(session_id)
    .bind(user_id)
    .bind(user_agent)
    .bind(crate::backend::db::sqlite::inet::encode(ip_address)?)
    .bind(crate::backend::db::sqlite::time::encode(expires_at))
    .fetch_one(pool)
    .await?;

    Ok(result)
}

pub async fn get_session(pool: &SqlitePool, session_id: &str) -> Result<Option<Session>> {
    let session = sqlx::query_as::<_, Session>(
        "SELECT s.id, s.user_id, s.user_agent, s.ip_address, s.expires_at, s.created_at FROM sessions s JOIN users u ON u.id=s.user_id WHERE s.id = ?1 AND s.expires_at > strftime('%Y-%m-%dT%H:%M:%f000Z','now') AND NOT u.is_banned",
    )
    .bind(session_id)
    .fetch_optional(pool)
    .await?;

    Ok(session)
}

/// Fresh session identity for password-change UI/operation only. Resource ingress
/// must use the ordinary current-user policy or get_session_user below.
pub async fn get_session_account(pool: &SqlitePool, session_id: &str) -> Result<Option<User>> {
    let user = sqlx::query_as::<_, User>(
        r#"
        SELECT u.* FROM users u
        INNER JOIN sessions s ON u.id = s.user_id
        WHERE s.id = ?1 AND s.expires_at > strftime('%Y-%m-%dT%H:%M:%f000Z','now') AND NOT u.is_banned
        "#,
    )
    .bind(session_id)
    .fetch_optional(pool)
    .await?;

    Ok(user)
}

pub async fn get_session_user(pool: &SqlitePool, session_id: &str) -> Result<Option<User>> {
    Ok(get_session_account(pool, session_id)
        .await?
        .filter(|user| !user.must_change_password))
}

pub async fn delete_session(pool: &SqlitePool, session_id: &str) -> Result<()> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    sqlx::query("DELETE FROM sessions WHERE id = ?1")
        .bind(session_id)
        .execute(&mut *tx)
        .await?;
    sqlx::query("DELETE FROM anonymous_sessions WHERE id = ?1")
        .bind(session_id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(())
}

pub async fn save_oauth_state(
    pool: &SqlitePool,
    state: &str,
    pkce_verifier: &str,
    provider: &str,
    redirect_uri: Option<&str>,
) -> Result<()> {
    sqlx::query(
        r#"
        INSERT INTO oauth_states (state, pkce_verifier, provider, redirect_uri, expires_at)
        VALUES (?1, ?2, ?3, ?4,strftime('%Y-%m-%dT%H:%M:%f000Z','now','+10 minutes'))
        "#,
    )
    .bind(state)
    .bind(pkce_verifier)
    .bind(provider)
    .bind(redirect_uri)
    .execute(pool)
    .await?;

    Ok(())
}

pub async fn get_oauth_state(pool: &SqlitePool, state: &str) -> Result<Option<OAuthState>> {
    let oauth_state = sqlx::query_as::<_, OAuthState>(
        "SELECT * FROM oauth_states WHERE state = ?1 AND expires_at > strftime('%Y-%m-%dT%H:%M:%f000Z','now')",
    )
    .bind(state)
    .fetch_optional(pool)
    .await?;

    Ok(oauth_state)
}

pub async fn save_bound_oauth_state(
    pool: &SqlitePool,
    state: &str,
    verifier: &str,
    provider: &str,
    redirect: &str,
    binding: &str,
) -> Result<()> {
    sqlx::query("INSERT INTO oauth_states (state,pkce_verifier,provider,redirect_uri,browser_binding,expires_at) VALUES (?1,?2,?3,?4,?5,strftime('%Y-%m-%dT%H:%M:%f000Z','now','+10 minutes'))")
        .bind(state).bind(verifier).bind(provider).bind(redirect).bind(binding).execute(pool).await?;
    Ok(())
}

pub async fn consume_bound_oauth_state(
    pool: &SqlitePool,
    state: &str,
    provider: &str,
    binding: &str,
    redirect: &str,
) -> Result<Option<OAuthState>> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let stored = sqlx::query_as::<_, OAuthState>("SELECT * FROM oauth_states WHERE state=?1 AND provider=?2 AND expires_at>strftime('%Y-%m-%dT%H:%M:%f000Z','now')")
        .bind(state).bind(provider).fetch_optional(&mut *tx).await?;
    let Some(stored) = stored else {
        return Ok(None);
    };
    let now =
        sqlx::query_scalar::<_, DateTime<Utc>>("SELECT strftime('%Y-%m-%dT%H:%M:%f000Z','now')")
            .fetch_one(&mut *tx)
            .await?;
    if stored.expires_at <= now {
        return Ok(None);
    }
    let bound = stored.browser_binding.as_deref().is_some_and(|expected| {
        expected.len() == 64
            && binding.len() == 64
            && bool::from(expected.as_bytes().ct_eq(binding.as_bytes()))
    });
    if !bound
        || !bool::from(stored.state.as_bytes().ct_eq(state.as_bytes()))
        || stored.redirect_uri.as_deref() != Some(redirect)
    {
        return Ok(None);
    }
    sqlx::query("DELETE FROM oauth_states WHERE state=?1")
        .bind(state)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(Some(stored))
}

pub async fn get_browser_session(pool: &SqlitePool, id: &str) -> Result<Option<BrowserSession>> {
    // Durable authority, including fresh suspension checks, survives server restarts.
    sqlx::query_as::<_, BrowserSession>("SELECT s.id,s.user_id,s.expires_at,s.created_at FROM sessions s JOIN users u ON u.id=s.user_id WHERE s.id=?1 AND s.expires_at>strftime('%Y-%m-%dT%H:%M:%f000Z','now') AND NOT u.is_banned UNION ALL SELECT id,NULL AS user_id,expires_at,created_at FROM anonymous_sessions WHERE id=?1 AND expires_at>strftime('%Y-%m-%dT%H:%M:%f000Z','now') AND NOT EXISTS(SELECT 1 FROM sessions WHERE id=?1) LIMIT 1")
        .bind(id).fetch_optional(pool).await.map_err(Into::into)
}

pub async fn rotate_authenticated_session(
    pool: &SqlitePool,
    old: Option<&str>,
    fresh: &str,
    user_id: i64,
    credential: crate::backend::db::users::sessions::SessionCredential<'_>,
    user_agent: Option<&str>,
    expires_at: DateTime<Utc>,
) -> Result<Option<BrowserSession>> {
    if !valid_random_token(fresh) || old == Some(fresh) {
        anyhow::bail!("Authenticated session requires a fresh random identifier");
    }
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let future = sqlx::query_scalar::<_, bool>("SELECT ?1>strftime('%Y-%m-%dT%H:%M:%f000Z','now')")
        .bind(crate::backend::db::sqlite::time::encode(expires_at))
        .fetch_one(&mut *tx)
        .await?;
    if !future {
        anyhow::bail!("Session expiry must be in the future");
    }
    if let Some(old) = old {
        let bound = sqlx::query_scalar::<_, bool>(
            "SELECT EXISTS(SELECT 1 FROM sessions WHERE id=?1 AND expires_at>strftime('%Y-%m-%dT%H:%M:%f000Z','now'))",
        )
        .bind(old)
        .fetch_one(&mut *tx)
        .await?;
        if bound {
            return Ok(None);
        }
    }
    let setup =
        sqlx::query_scalar::<_, bool>("SELECT setup_completed FROM node_configuration WHERE id=1")
            .fetch_optional(&mut *tx)
            .await?
            .unwrap_or(false);
    if !setup {
        return Ok(None);
    }
    let user = sqlx::query_as::<_, User>("SELECT * FROM users WHERE id=?1")
        .bind(user_id)
        .fetch_optional(&mut *tx)
        .await?;
    let Some(user) = user.filter(|user| !user.is_banned) else {
        return Ok(None);
    };
    if let Some(expected) = credential.password() {
        if user.password_hash.as_deref() != Some(expected) {
            return Ok(None);
        }
    } else if user.must_change_password {
        return Ok(None);
    }
    use crate::backend::db::users::sessions::SessionCredential;
    match credential {
        SessionCredential::Password(_) if user.two_factor_enabled => return Ok(None),
        SessionCredential::SecondFactor { .. } if !user.two_factor_enabled => return Ok(None),
        SessionCredential::SecondFactor {
            recovery_hash: Some(hash),
            ..
        } => {
            let removed = sqlx::query(
                "DELETE FROM two_factor_recovery_codes WHERE user_id=?1 AND code_hash=?2",
            )
            .bind(user_id)
            .bind(hash)
            .execute(&mut *tx)
            .await?
            .rows_affected();
            if removed != 1 {
                return Ok(None);
            }
        }
        _ => {}
    }
    if let Some(old) = old {
        sqlx::query("DELETE FROM anonymous_sessions WHERE id=?1")
            .bind(old)
            .execute(&mut *tx)
            .await?;
        sqlx::query("DELETE FROM sessions WHERE id=?1 AND expires_at<=strftime('%Y-%m-%dT%H:%M:%f000Z','now')")
            .bind(old)
            .execute(&mut *tx)
            .await?;
    }
    let collision = sqlx::query_scalar::<_, bool>(
        "SELECT EXISTS(SELECT 1 FROM anonymous_sessions WHERE id=?1)",
    )
    .bind(fresh)
    .fetch_one(&mut *tx)
    .await?;
    if collision {
        anyhow::bail!("Fresh session identifier collision");
    }
    let session = sqlx::query_as::<_, BrowserSession>("INSERT INTO sessions (id,user_id,user_agent,expires_at) VALUES (?1,?2,?3,min(?4,strftime('%Y-%m-%dT%H:%M:%f000Z','now','+365 days'))) RETURNING id,user_id,expires_at,created_at")
        .bind(fresh).bind(user_id).bind(user_agent).bind(crate::backend::db::sqlite::time::encode(expires_at)).fetch_one(&mut *tx).await?;
    tx.commit().await?;
    Ok(Some(session))
}

fn valid_random_token(token: &str) -> bool {
    token.len() == 64 && token.bytes().all(|byte| byte.is_ascii_hexdigit())
}

pub async fn create_browser_session(
    pool: &SqlitePool,
    id: &str,
    user_agent: Option<&str>,
    ip_address: Option<&str>,
    expires_at: DateTime<Utc>,
) -> Result<BrowserSession> {
    if !valid_random_token(id) {
        anyhow::bail!("Session requires a random identifier");
    }
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let collision = sqlx::query_scalar::<_, bool>("SELECT EXISTS(SELECT 1 FROM sessions WHERE id=?1 UNION ALL SELECT 1 FROM anonymous_sessions WHERE id=?1)")
        .bind(id).fetch_one(&mut *tx).await?;
    if collision {
        anyhow::bail!("Session identifier collision");
    }
    let future = sqlx::query_scalar::<_, bool>("SELECT ?1>strftime('%Y-%m-%dT%H:%M:%f000Z','now')")
        .bind(crate::backend::db::sqlite::time::encode(expires_at))
        .fetch_one(&mut *tx)
        .await?;
    if !future {
        anyhow::bail!("Session expiry must be in the future");
    }
    let session = sqlx::query_as::<_, BrowserSession>("INSERT INTO anonymous_sessions (id,user_agent,ip_address,expires_at) VALUES (?1,?2,?3,min(?4,strftime('%Y-%m-%dT%H:%M:%f000Z','now','+365 days'))) RETURNING id,NULL AS user_id,created_at,expires_at")
        .bind(id).bind(user_agent).bind(crate::backend::db::sqlite::inet::encode(ip_address)?).bind(crate::backend::db::sqlite::time::encode(expires_at)).fetch_one(&mut *tx).await?;
    tx.commit().await?;
    Ok(session)
}

/// A live session's stored CSRF token, the token's expiry, the session's expiry and the
/// database clock, read in one statement. The query decides liveness, so a revoked,
/// banned or expired session has no row.
#[derive(sqlx::FromRow)]
struct CsrfRow {
    csrf_token: Option<String>,
    csrf_expires_at: Option<DateTime<Utc>>,
    expires_at: DateTime<Utc>,
    now: DateTime<Utc>,
}

async fn csrf_row<'c>(
    conn: impl sqlx::Executor<'c, Database = sqlx::Sqlite>,
    id: &str,
    expected_user: Option<i64>,
) -> Result<Option<CsrfRow>> {
    let row = if let Some(user) = expected_user {
        sqlx::query_as::<_, CsrfRow>("SELECT s.csrf_token,s.csrf_expires_at,s.expires_at,strftime('%Y-%m-%dT%H:%M:%f000Z','now') AS now FROM sessions s JOIN users u ON u.id=s.user_id WHERE s.id=?1 AND s.user_id=?2 AND s.expires_at>strftime('%Y-%m-%dT%H:%M:%f000Z','now') AND NOT u.is_banned")
            .bind(id).bind(user).fetch_optional(conn).await?
    } else {
        sqlx::query_as::<_, CsrfRow>("SELECT csrf_token,csrf_expires_at,expires_at,strftime('%Y-%m-%dT%H:%M:%f000Z','now') AS now FROM anonymous_sessions WHERE id=?1 AND expires_at>strftime('%Y-%m-%dT%H:%M:%f000Z','now') AND NOT EXISTS(SELECT 1 FROM sessions WHERE id=?1)")
            .bind(id).fetch_optional(conn).await?
    };
    Ok(row)
}

/// The stored token, when the session is live and the token is well formed and unexpired.
fn current_token(row: &CsrfRow) -> Option<String> {
    let live = row.expires_at > row.now;
    let unexpired = row.csrf_expires_at.is_some_and(|expiry| expiry > row.now);
    row.csrf_token
        .clone()
        .filter(|token| live && unexpired && valid_random_token(token))
}

pub async fn get_or_create_csrf_token(
    pool: &SqlitePool,
    id: &str,
    expected_user: Option<i64>,
    candidate: &str,
    ttl_seconds: i64,
) -> Result<Option<String>> {
    if !valid_random_token(id)
        || !valid_random_token(candidate)
        || !(1..=7200).contains(&ttl_seconds)
    {
        anyhow::bail!("Invalid CSRF token input");
    }
    // Page loads nearly always find the current token stored already. A plain read
    // answers that without the writer, so browsing never queues behind it.
    let Some(row) = csrf_row(pool, id, expected_user).await? else {
        return Ok(None);
    };
    if let Some(token) = current_token(&row) {
        return Ok(Some(token));
    }
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    // SQLite writer ownership covers concurrent first GETs/expiry replacement,
    // including other pools/processes. No refresh-time rotation/orphan CSRF rows.
    let Some(row) = csrf_row(&mut *tx, id, expected_user).await? else {
        return Ok(None);
    };
    if row.expires_at <= row.now {
        return Ok(None);
    }
    if let Some(token) = current_token(&row) {
        tx.commit().await?;
        return Ok(Some(token));
    }
    let expires_at = (row.now + Duration::seconds(ttl_seconds)).min(row.expires_at);
    let query = if expected_user.is_some() {
        "UPDATE sessions SET csrf_token=?2,csrf_expires_at=?3 WHERE id=?1"
    } else {
        "UPDATE anonymous_sessions SET csrf_token=?2,csrf_expires_at=?3 WHERE id=?1"
    };
    sqlx::query(query)
        .bind(id)
        .bind(candidate)
        .bind(crate::backend::db::sqlite::time::encode(expires_at))
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(Some(candidate.to_owned()))
}

/// The number of anonymous sessions a sweep keeps. Expired ones go first, and the count
/// can run above this until the next sweep.
const MAX_ANONYMOUS_SESSIONS: i64 = 100_000;

/// Deletes expired sessions, anonymous sessions and OAuth states, and clears the
/// password verifier of each processed signup request. An approved request's
/// account already holds the hash it needs, and a rejected one never needs it.
/// Expiry selects only rows with `expires_at <= now`, the exact complement of the
/// validity checks above. Then anonymous sessions are capped at
/// `MAX_ANONYMOUS_SESSIONS`, removing the oldest by expiry even if still live.
/// Authenticated sessions are not capped. Every statement works on at most `BATCH`
/// rows and commits on its own, so no transaction spans the sweep.
pub async fn remove_expired_identity_rows(pool: &SqlitePool) -> Result<u64> {
    const BATCH: i64 = 500;
    const STATEMENTS: [&str; 4] = [
        "DELETE FROM anonymous_sessions WHERE id IN (SELECT id FROM anonymous_sessions WHERE expires_at <= strftime('%Y-%m-%dT%H:%M:%f000Z','now') LIMIT ?1)",
        "DELETE FROM sessions WHERE id IN (SELECT id FROM sessions WHERE expires_at <= strftime('%Y-%m-%dT%H:%M:%f000Z','now') LIMIT ?1)",
        "DELETE FROM oauth_states WHERE state IN (SELECT state FROM oauth_states WHERE expires_at <= strftime('%Y-%m-%dT%H:%M:%f000Z','now') LIMIT ?1)",
        "UPDATE signup_requests SET password_hash = '' WHERE id IN (SELECT id FROM signup_requests WHERE status <> 'pending' AND password_hash <> '' LIMIT ?1)",
    ];
    let mut changed = 0;
    for sql in STATEMENTS {
        loop {
            let batch = sqlx::query(sql)
                .bind(BATCH)
                .execute(pool)
                .await?
                .rows_affected();
            changed += batch;
            if batch < BATCH as u64 {
                break;
            }
        }
    }
    changed += keep_newest_anonymous_sessions(pool, MAX_ANONYMOUS_SESSIONS).await?;
    Ok(changed)
}

/// Removes the oldest anonymous sessions, by expiry, until only the newest `cap` remain.
/// The row just past the newest `cap` fixes a boundary when the sweep starts, and only rows
/// at or below it are deleted, oldest first, so a session that arrives later is never taken
/// in place of an older one. A sweep deletes at most the surplus counted when it starts,
/// which keeps it finite when sessions that expire sooner keep arriving; the next sweep
/// takes whatever is left.
pub async fn keep_newest_anonymous_sessions(pool: &SqlitePool, cap: i64) -> Result<u64> {
    const BATCH: i64 = 500;
    let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM anonymous_sessions")
        .fetch_one(pool)
        .await?;
    let surplus = rows - cap;
    if surplus <= 0 {
        return Ok(0);
    }
    let boundary: Option<(String, String)> = sqlx::query_as("SELECT expires_at, id FROM anonymous_sessions ORDER BY expires_at DESC, id DESC LIMIT 1 OFFSET ?1")
        .bind(cap)
        .fetch_optional(pool)
        .await?;
    let Some((expires_at, id)) = boundary else {
        return Ok(0);
    };
    let mut removed = 0;
    while removed < surplus {
        let limit = BATCH.min(surplus - removed);
        let deleted = sqlx::query("DELETE FROM anonymous_sessions WHERE id IN (SELECT id FROM anonymous_sessions WHERE expires_at < ?1 OR (expires_at = ?1 AND id <= ?2) ORDER BY expires_at, id LIMIT ?3)")
            .bind(expires_at.as_str())
            .bind(id.as_str())
            .bind(limit)
            .execute(pool)
            .await?
            .rows_affected() as i64;
        removed += deleted;
        if deleted < limit {
            break;
        }
    }
    Ok(removed as u64)
}

pub async fn list_user_sessions(pool: &SqlitePool, user_id: i64) -> Result<Vec<Session>> {
    Ok(sqlx::query_as(
        "SELECT id,user_id,user_agent,ip_address,expires_at,created_at FROM sessions WHERE user_id=?1 AND expires_at>strftime('%Y-%m-%dT%H:%M:%f000Z','now') ORDER BY created_at DESC, id",
    )
    .bind(user_id)
    .fetch_all(pool)
    .await?)
}

pub async fn revoke_user_session(
    pool: &SqlitePool,
    user_id: i64,
    session_id: &str,
) -> Result<bool> {
    Ok(
        sqlx::query("DELETE FROM sessions WHERE user_id=?1 AND id=?2")
            .bind(user_id)
            .bind(session_id)
            .execute(pool)
            .await?
            .rows_affected()
            > 0,
    )
}

pub async fn revoke_other_user_sessions(
    pool: &SqlitePool,
    user_id: i64,
    keep: &str,
) -> Result<u64> {
    Ok(
        sqlx::query("DELETE FROM sessions WHERE user_id=?1 AND id<>?2")
            .bind(user_id)
            .bind(keep)
            .execute(pool)
            .await?
            .rows_affected(),
    )
}
