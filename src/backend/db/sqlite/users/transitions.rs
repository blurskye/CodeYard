use crate::backend::db::users::transitions::{AccountAdmission, BanChange, SetupClaim};
use sqlx::SqlitePool;
// Atomic identity transitions. No provider, filesystem or KDF work under locks.
use crate::backend::db::models::{NodeConfiguration, User};
use anyhow::Result;
use sqlx::{Sqlite, Transaction};

pub(crate) async fn organization_name_exists(
    tx: &mut Transaction<'_, Sqlite>,
    username: &str,
) -> Result<bool> {
    // Call only inside BEGIN IMMEDIATE: serialize the reciprocal predicate and
    // insert with organization creation. Keep exact, case-sensitive names.
    Ok(
        sqlx::query_scalar::<_, bool>("SELECT EXISTS(SELECT 1 FROM organizations WHERE name=?1)")
            .bind(username)
            .fetch_one(&mut **tx)
            .await?,
    )
}

pub(crate) async fn require_user_namespace(
    tx: &mut Transaction<'_, Sqlite>,
    username: &str,
) -> Result<()> {
    if organization_name_exists(tx, username).await? {
        anyhow::bail!("Account namespace conflict");
    }
    Ok(())
}

pub(crate) async fn active_admin(tx: &mut Transaction<'_, Sqlite>, actor: i64) -> Result<bool> {
    Ok(sqlx::query_scalar::<_, bool>(
        "SELECT is_admin AND NOT is_banned AND NOT must_change_password FROM users WHERE id = ?1",
    )
    .bind(actor)
    .fetch_optional(&mut **tx)
    .await?
    .unwrap_or(false))
}

pub(crate) async fn owned_organizations_for_account_change(
    tx: &mut Transaction<'_, Sqlite>,
    user: i64,
) -> Result<Vec<i64>> {
    Ok(sqlx::query_scalar(
        "SELECT org_id FROM org_memberships WHERE user_id=?1 AND role='owner' ORDER BY org_id",
    )
    .bind(user)
    .fetch_all(&mut **tx)
    .await?)
}

pub(crate) async fn stop_ownerless_organization_ci(
    tx: &mut Transaction<'_, Sqlite>,
    organizations: &[i64],
) -> Result<()> {
    if organizations.is_empty() {
        return Ok(());
    }
    let organizations = serde_json::to_string(organizations)?;
    let ownerless: Vec<i64> = sqlx::query_scalar(
        "SELECT o.id FROM organizations o WHERE o.id IN (SELECT value FROM json_each(?1)) AND NOT EXISTS(SELECT 1 FROM org_memberships m JOIN users u ON u.id=m.user_id WHERE m.org_id=o.id AND m.role='owner' AND NOT u.is_banned AND NOT u.must_change_password) ORDER BY o.id",
    )
    .bind(organizations)
    .fetch_all(&mut **tx)
    .await?;
    crate::backend::db::sqlite::ci::stop_organization_ci_runs(tx, &ownerless).await
}

pub async fn complete_setup(
    pool: &SqlitePool,
    username: &str,
    email: Option<&str>,
    hash: &str,
    mode: &str,
    recovery: Option<(i64, &str)>,
) -> Result<SetupClaim> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let completed = sqlx::query_scalar::<_, bool>(
        "SELECT setup_completed FROM node_configuration WHERE id = 1",
    )
    .fetch_optional(&mut *tx)
    .await?
    .unwrap_or(false);
    if completed {
        return Ok(SetupClaim::AlreadyCompleted);
    }
    let count = sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM users")
        .fetch_one(&mut *tx)
        .await?;
    let user = if count == 0 {
        if organization_name_exists(&mut tx, username).await? {
            return Ok(SetupClaim::ExistingAccounts);
        }
        sqlx::query_as::<_, User>("INSERT INTO users (username,email,password_hash,is_admin) VALUES (?1,?2,?3,TRUE) RETURNING *")
            .bind(username).bind(email).bind(hash).fetch_one(&mut *tx).await?
    } else if let Some((id, expected_hash)) = recovery.filter(|_| count == 1) {
        // Complete setup for a sole active admin that was provisioned separately: the
        // verified credentials must match its stored hash exactly. Never adopt an outsider.
        let user = sqlx::query_as::<_, User>("UPDATE users SET password_hash = ?3, updated_at = strftime('%Y-%m-%dT%H:%M:%f000Z','now') WHERE id = ?1 AND username = ?2 AND password_hash = ?4 AND is_admin AND NOT is_banned RETURNING *")
            .bind(id).bind(username).bind(hash).bind(expected_hash).fetch_optional(&mut *tx).await?;
        match user {
            Some(user) => user,
            None => return Ok(SetupClaim::ExistingAccounts),
        }
    } else {
        return Ok(SetupClaim::ExistingAccounts);
    };
    sqlx::query("INSERT INTO node_configuration (id,node_mode,setup_completed) VALUES (1,?1,TRUE) ON CONFLICT (id) DO UPDATE SET node_mode=?1,setup_completed=TRUE,updated_at=strftime('%Y-%m-%dT%H:%M:%f000Z','now')")
        .bind(mode).execute(&mut *tx).await?;
    tx.commit().await?;
    Ok(SetupClaim::Completed(Box::new(user)))
}

pub async fn create_cli_user(
    pool: &SqlitePool,
    username: &str,
    email: Option<&str>,
) -> Result<AccountAdmission> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let completed =
        sqlx::query_scalar::<_, bool>("SELECT setup_completed FROM node_configuration WHERE id=1")
            .fetch_optional(&mut *tx)
            .await?
            .unwrap_or(false);
    if !completed {
        return Ok(AccountAdmission::SetupRequired);
    }
    require_user_namespace(&mut tx, username).await?;
    let user =
        sqlx::query_as::<_, User>("INSERT INTO users (username,email) VALUES (?1,?2) RETURNING *")
            .bind(username)
            .bind(email)
            .fetch_one(&mut *tx)
            .await?;
    tx.commit().await?;
    Ok(AccountAdmission::User(Box::new(user)))
}

pub async fn bootstrap_admin(
    pool: &SqlitePool,
    username: &str,
    hash: &str,
    mode: &str,
) -> Result<SetupClaim> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let completed =
        sqlx::query_scalar::<_, bool>("SELECT setup_completed FROM node_configuration WHERE id=1")
            .fetch_optional(&mut *tx)
            .await?
            .unwrap_or(false);
    if completed {
        return Ok(SetupClaim::AlreadyCompleted);
    }
    if organization_name_exists(&mut tx, username).await?
        || sqlx::query_scalar::<_, bool>("SELECT EXISTS(SELECT 1 FROM users WHERE username=?1)")
            .bind(username)
            .fetch_one(&mut *tx)
            .await?
    {
        return Ok(SetupClaim::ExistingAccounts);
    }
    let user = sqlx::query_as::<_, User>(
        "INSERT INTO users (username,password_hash,is_admin) VALUES (?1,?2,TRUE) RETURNING *",
    )
    .bind(username)
    .bind(hash)
    .fetch_one(&mut *tx)
    .await?;
    sqlx::query("INSERT INTO node_configuration (id,node_mode,setup_completed) VALUES (1,?1,TRUE) ON CONFLICT (id) DO UPDATE SET node_mode=?1,setup_completed=TRUE,updated_at=strftime('%Y-%m-%dT%H:%M:%f000Z','now')")
        .bind(mode).execute(&mut *tx).await?;
    tx.commit().await?;
    Ok(SetupClaim::Completed(Box::new(user)))
}

pub async fn change_ban(
    pool: &SqlitePool,
    actor: i64,
    target: i64,
    banned: bool,
) -> Result<BanChange> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let organizations = owned_organizations_for_account_change(&mut tx, target).await?;
    if !active_admin(&mut tx, actor).await? {
        return Ok(BanChange::Denied);
    }
    if actor == target {
        return Ok(BanChange::SelfBan);
    }
    let Some(user) = sqlx::query_as::<_, User>("SELECT * FROM users WHERE id=?1")
        .bind(target)
        .fetch_optional(&mut *tx)
        .await?
    else {
        return Ok(BanChange::Missing);
    };
    if banned && user.is_admin && !user.is_banned {
        let count = sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*) FROM users WHERE is_admin AND NOT is_banned",
        )
        .fetch_one(&mut *tx)
        .await?;
        if count <= 1 {
            return Ok(BanChange::LastAdmin);
        }
    }
    let user = sqlx::query_as::<_, User>(
        "UPDATE users SET is_banned=?2,updated_at=strftime('%Y-%m-%dT%H:%M:%f000Z','now') WHERE id=?1 RETURNING *",
    )
    .bind(target)
    .bind(banned)
    .fetch_one(&mut *tx)
    .await?;
    if banned {
        sqlx::query("DELETE FROM sessions WHERE user_id=?1")
            .bind(target)
            .execute(&mut *tx)
            .await?;
        sqlx::query("DELETE FROM personal_access_tokens WHERE user_id=?1")
            .bind(target)
            .execute(&mut *tx)
            .await?;
        // Stop the account's own work everywhere. If it was the last usable owner,
        // stop every actor's work in that organization's repositories as well.
        crate::backend::db::sqlite::ci::stop_actor_ci_runs(&mut tx, target).await?;
        stop_ownerless_organization_ci(&mut tx, &organizations).await?;
    }
    tx.commit().await?;
    // Repository rows, objects and disk contents are preserved on suspension.
    Ok(BanChange::Updated(Box::new(user)))
}

pub async fn register_password(
    pool: &SqlitePool,
    username: &str,
    email: Option<&str>,
    hash: &str,
    administrator: Option<(i64, &str)>,
) -> Result<AccountAdmission> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let config =
        sqlx::query_as::<_, NodeConfiguration>("SELECT * FROM node_configuration WHERE id=1")
            .fetch_optional(&mut *tx)
            .await?;
    let Some(config) = config.filter(|config| config.setup_completed) else {
        return Ok(AccountAdmission::SetupRequired);
    };
    if let Some((actor, session_id)) = administrator {
        if config.node_mode != "private" || !active_admin(&mut tx, actor).await? {
            return Ok(AccountAdmission::Denied);
        }
        let live: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM sessions WHERE id=?1 AND user_id=?2 AND expires_at>strftime('%Y-%m-%dT%H:%M:%f000Z','now'))")
            .bind(session_id).bind(actor).fetch_one(&mut *tx).await?;
        if !live {
            return Ok(AccountAdmission::Denied);
        }
    }
    if organization_name_exists(&mut tx, username).await? {
        return Ok(AccountAdmission::Conflict);
    }
    if sqlx::query_scalar::<_, bool>(
        "SELECT EXISTS(SELECT 1 FROM users WHERE username=?1 OR (?2 IS NOT NULL AND email=?2))",
    )
    .bind(username)
    .bind(email)
    .fetch_one(&mut *tx)
    .await?
    {
        return Ok(AccountAdmission::Conflict);
    }
    if config.node_mode == "private" && administrator.is_none() {
        if sqlx::query_scalar::<_, bool>("SELECT EXISTS(SELECT 1 FROM signup_requests WHERE status='pending' AND (LOWER(username)=LOWER(?1) OR (?2 IS NOT NULL AND LOWER(email)=LOWER(?2))))")
            .bind(username).bind(email).fetch_one(&mut *tx).await? { return Ok(AccountAdmission::Conflict); }
        sqlx::query("INSERT INTO signup_requests (username,email,password_hash) VALUES (?1,?2,?3)")
            .bind(username)
            .bind(email)
            .bind(hash)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        return Ok(AccountAdmission::Requested);
    }
    let user = sqlx::query_as::<_, User>("INSERT INTO users (username,email,password_hash,is_admin,must_change_password) VALUES (?1,?2,?3,FALSE,?4) RETURNING *")
        .bind(username).bind(email).bind(hash).bind(administrator.is_some()).fetch_one(&mut *tx).await?;
    tx.commit().await?;
    Ok(AccountAdmission::User(Box::new(user)))
}

pub async fn admit_oauth_account(
    pool: &SqlitePool,
    provider: &str,
    provider_id: &str,
    username_base: &str,
    display_name: Option<&str>,
) -> Result<AccountAdmission> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let config =
        sqlx::query_as::<_, NodeConfiguration>("SELECT * FROM node_configuration WHERE id=1")
            .fetch_optional(&mut *tx)
            .await?;
    let Some(config) = config.filter(|config| config.setup_completed) else {
        return Ok(AccountAdmission::SetupRequired);
    };
    let linked = sqlx::query_as::<_, User>("SELECT u.* FROM users u JOIN oauth_accounts a ON a.user_id=u.id WHERE a.provider=?1 AND a.provider_user_id=?2")
        .bind(provider).bind(provider_id).fetch_optional(&mut *tx).await?;
    if let Some(user) = linked {
        if user.is_banned || user.must_change_password {
            return Ok(AccountAdmission::Denied);
        }
        tx.commit().await?;
        return Ok(AccountAdmission::User(Box::new(user)));
    }
    // Never link by an untrusted/unverified provider email. Private-node enrollment
    // needs an existing administrator-approved provider link, not just an email match.
    if config.node_mode == "private" {
        return Ok(AccountAdmission::ApprovalRequired);
    }
    let base: String = username_base.chars().take(200).collect();
    let mut name = base.clone();
    for suffix in 0..32 {
        if suffix > 0 {
            name = format!("{base}_{suffix}");
        }
        if organization_name_exists(&mut tx, &name).await? {
            continue;
        }
        let user = sqlx::query_as::<_, User>("INSERT INTO users (username,display_name) VALUES (?1,?2) ON CONFLICT (username) DO NOTHING RETURNING *")
            .bind(&name).bind(display_name).fetch_optional(&mut *tx).await?;
        if let Some(user) = user {
            // Login requires neither offline access nor token persistence. Do not retain
            // provider bearer credentials as plaintext just to establish a local identity.
            sqlx::query(
                "INSERT INTO oauth_accounts (user_id,provider,provider_user_id) VALUES (?1,?2,?3)",
            )
            .bind(user.id)
            .bind(provider)
            .bind(provider_id)
            .execute(&mut *tx)
            .await?;
            tx.commit().await?;
            return Ok(AccountAdmission::User(Box::new(user)));
        }
    }
    Ok(AccountAdmission::Conflict)
}

/// Explicit administrator-approved enrollment for an existing private-node account.
/// Ingress must first obtain the provider subject from a verified provider flow, not
/// an email/profile match. The parent owns any new route/UI wiring for this operation.
pub async fn enroll_oauth_identity(
    pool: &SqlitePool,
    actor: i64,
    user: i64,
    provider: &str,
    subject: &str,
) -> Result<bool> {
    if !["github", "google", "gitlab", "discord", "bitbucket"].contains(&provider)
        || subject.is_empty()
        || subject.len() > 255
    {
        return Ok(false);
    }
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let private = sqlx::query_scalar::<_, bool>(
        "SELECT setup_completed AND node_mode='private' FROM node_configuration WHERE id=1",
    )
    .fetch_optional(&mut *tx)
    .await?
    .unwrap_or(false);
    if !private || !active_admin(&mut tx, actor).await? {
        return Ok(false);
    }
    let active = sqlx::query_scalar::<_, bool>("SELECT NOT is_banned FROM users WHERE id=?1")
        .bind(user)
        .fetch_optional(&mut *tx)
        .await?
        .unwrap_or(false);
    if !active {
        return Ok(false);
    }
    let inserted = sqlx::query("INSERT INTO oauth_accounts (user_id,provider,provider_user_id) VALUES (?1,?2,?3) ON CONFLICT (provider,provider_user_id) DO NOTHING")
        .bind(user).bind(provider).bind(subject).execute(&mut *tx).await?.rows_affected() == 1;
    tx.commit().await?;
    Ok(inserted)
}

pub async fn compare_exchange_password(
    pool: &SqlitePool,
    user_id: i64,
    expected: &str,
    replacement: &str,
    clear_must_change: bool,
    revoke_sessions: bool,
) -> Result<bool> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    if clear_must_change {
        owned_organizations_for_account_change(&mut tx, user_id).await?;
    }
    let changed = sqlx::query("UPDATE users SET password_hash=?3,must_change_password=CASE WHEN ?4 THEN FALSE ELSE must_change_password END,updated_at=strftime('%Y-%m-%dT%H:%M:%f000Z','now') WHERE id=?1 AND password_hash=?2 AND NOT is_banned")
        .bind(user_id).bind(expected).bind(replacement).bind(clear_must_change).execute(&mut *tx).await?.rows_affected() == 1;
    if changed && revoke_sessions {
        sqlx::query("DELETE FROM personal_access_tokens WHERE user_id=?1")
            .bind(user_id)
            .execute(&mut *tx)
            .await?;
        sqlx::query("DELETE FROM sessions WHERE user_id=?1")
            .bind(user_id)
            .execute(&mut *tx)
            .await?;
    }
    tx.commit().await?;
    Ok(changed)
}
