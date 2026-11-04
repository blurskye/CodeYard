use crate::users::transitions::AccountAdmission;
use crate::users::transitions::AdministratorActor;
use crate::users::transitions::BanChange;
use crate::users::transitions::EmailChange;
use crate::users::transitions::RoleChange;
use crate::users::transitions::SetupClaim;
use anyhow::Result;
use codeyard_db_core::models::NodeConfiguration;
use codeyard_db_core::models::User;
use sqlx::Sqlite;
use sqlx::SqlitePool;
use sqlx::Transaction;

pub(crate) async fn organization_name_exists(
    tx: &mut Transaction<'_, Sqlite>,
    username: &str,
) -> Result<bool> {
    // Call only inside BEGIN IMMEDIATE: serialize the reciprocal predicate and
    // insert with organization creation. Preserve spelling, reserve ASCII case variants.
    Ok(sqlx::query_scalar::<_, bool>(
        "SELECT EXISTS(SELECT 1 FROM organizations WHERE lower(name)=lower(?1))",
    )
    .bind(username)
    .fetch_one(&mut **tx)
    .await?)
}

pub(crate) async fn email_owned(
    tx: &mut Transaction<'_, Sqlite>,
    email: Option<&str>,
    except: i64,
) -> Result<bool> {
    Ok(sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM users WHERE lower(email)=lower(?1) AND id<>?2) OR EXISTS(SELECT 1 FROM user_emails WHERE lower(email)=lower(?1) AND user_id<>?2)")
        .bind(email).bind(except).fetch_one(&mut **tx).await?)
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

/// Call within the operation's transaction to retain one authority snapshot.
pub(crate) async fn active_account(
    connection: &mut sqlx::SqliteConnection,
    actor: i64,
) -> Result<bool> {
    Ok(sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM users WHERE id=?1 AND NOT is_banned AND NOT must_change_password)")
        .bind(actor).fetch_one(connection).await?)
}

pub(crate) async fn active_session(
    connection: &mut sqlx::SqliteConnection,
    actor: i64,
    session: &str,
) -> Result<bool> {
    Ok(
        active_account(connection, actor).await?
            && live_session(connection, actor, session).await?,
    )
}

pub(crate) async fn active_session_read(
    connection: &mut sqlx::SqliteConnection,
    actor: i64,
    session: &str,
) -> Result<bool> {
    active_session(connection, actor, session).await
}

pub async fn require_account(
    tx: &mut Transaction<'_, sqlx::Sqlite>,
    actor: i64,
    credential: &codeyard_db_core::queries::users::accounts::AccountCredential<'_>,
) -> Result<()> {
    require_account_with_mode(tx, actor, credential, true).await
}

pub async fn require_read_account(
    tx: &mut Transaction<'_, sqlx::Sqlite>,
    actor: i64,
    credential: &codeyard_db_core::queries::users::accounts::AccountCredential<'_>,
) -> Result<()> {
    require_account_with_mode(tx, actor, credential, false).await
}

async fn require_account_with_mode(
    tx: &mut Transaction<'_, sqlx::Sqlite>,
    actor: i64,
    credential: &codeyard_db_core::queries::users::accounts::AccountCredential<'_>,
    write: bool,
) -> Result<()> {
    use codeyard_support::credentials::AccountRestricted;
    anyhow::ensure!(active_account(tx, actor).await?, AccountRestricted);
    anyhow::ensure!(
        credential_live(tx, actor, credential, write).await?,
        AccountRestricted
    );
    Ok(())
}

pub(crate) async fn current_password(
    tx: &mut Transaction<'_, Sqlite>,
    user: i64,
    expected: &str,
    session: &str,
    allow_must_change: bool,
) -> Result<bool> {
    let valid: bool = sqlx::query_scalar(
        "SELECT password_hash=?2 AND NOT is_banned AND (?3 OR NOT must_change_password) FROM users WHERE id=?1",
    ).bind(user).bind(expected).bind(allow_must_change).fetch_optional(&mut **tx).await?.unwrap_or(false);
    Ok(valid && live_session(tx, user, session).await?)
}

pub async fn active_admin_session(
    tx: &mut Transaction<'_, Sqlite>,
    actor: &AdministratorActor,
) -> Result<bool> {
    if !active_admin(tx, actor.user_id).await? {
        return Ok(false);
    }
    match actor.session.as_deref() {
        Some(session) => live_session(tx, actor.user_id, session).await,
        None => Ok(true),
    }
}

pub async fn active_admin(tx: &mut Transaction<'_, Sqlite>, actor: i64) -> Result<bool> {
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
    codeyard_db_core::queries::sqlite::ci::stop_organization_ci_runs(tx, &ownerless).await
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
    if email_owned(&mut tx, email, -1).await? {
        return Ok(AccountAdmission::Conflict);
    }
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
        || sqlx::query_scalar::<_, bool>(
            "SELECT EXISTS(SELECT 1 FROM users WHERE lower(username)=lower(?1))",
        )
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
    actor: impl Into<AdministratorActor>,
    target: i64,
    banned: bool,
) -> Result<BanChange> {
    let actor = actor.into();
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let organizations = owned_organizations_for_account_change(&mut tx, target).await?;
    if !active_admin_session(&mut tx, &actor).await? {
        return Ok(BanChange::Denied);
    }
    if actor.user_id == target {
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
        codeyard_db_core::queries::sqlite::ci::stop_actor_ci_runs(&mut tx, target).await?;
        stop_ownerless_organization_ci(&mut tx, &organizations).await?;
    }
    sqlx::query("INSERT INTO audit_log(actor_id,action,target_type,target_id,metadata) VALUES(?1,'user.ban','user',?2,?3)")
        .bind(actor.user_id).bind(target).bind(serde_json::json!({"banned":banned,"outcome":"applied"}).to_string()).execute(&mut *tx).await?;
    tx.commit().await?;
    // Repository rows, objects and disk contents are preserved on suspension.
    Ok(BanChange::Updated(Box::new(user)))
}

/// Sets the account's address. Another account's address is refused in the same transaction.
pub async fn change_email(
    pool: &SqlitePool,
    user_id: i64,
    email: &str,
    expected: &str,
    session: &str,
) -> Result<EmailChange> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    if !current_password(&mut tx, user_id, expected, session, false).await? {
        return Ok(EmailChange::Missing);
    }
    if email_owned(&mut tx, Some(email), user_id).await? {
        return Ok(EmailChange::Taken);
    }
    let changed: bool = sqlx::query_scalar("SELECT email IS NOT ?2 FROM users WHERE id=?1")
        .bind(user_id)
        .bind(email)
        .fetch_one(&mut *tx)
        .await?;
    let verified: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM user_emails WHERE user_id=?1 AND lower(email)=lower(?2) AND verified)")
        .bind(user_id).bind(email).fetch_one(&mut *tx).await?;
    if changed {
        sqlx::query("DELETE FROM email_tokens WHERE user_id=?1")
            .bind(user_id)
            .execute(&mut *tx)
            .await?;
    }
    sqlx::query("DELETE FROM user_emails WHERE user_id=?1 AND lower(email)=lower(?2)")
        .bind(user_id)
        .bind(email)
        .execute(&mut *tx)
        .await?;
    let user = sqlx::query_as::<_, User>(
        "UPDATE users SET email_verified=CASE WHEN email IS NOT ?2 THEN ?3 ELSE email_verified END,email=?2,updated_at=strftime('%Y-%m-%dT%H:%M:%f000Z','now') WHERE id=?1 RETURNING *",
    )
    .bind(user_id)
    .bind(email)
    .bind(verified)
    .fetch_optional(&mut *tx)
    .await?;
    let Some(user) = user else {
        return Ok(EmailChange::Missing);
    };
    tx.commit().await?;
    Ok(EmailChange::Updated(Box::new(user)))
}

/// Grants or removes the site administrator role. The acting administrator is active and is
/// never the target, so at least one administrator always remains.
pub async fn change_admin_role(
    pool: &SqlitePool,
    actor: impl Into<AdministratorActor>,
    target: i64,
    admin: bool,
) -> Result<RoleChange> {
    let actor = actor.into();
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    if !active_admin_session(&mut tx, &actor).await? {
        return Ok(RoleChange::Denied);
    }
    if actor.user_id == target {
        return Ok(RoleChange::SelfChange);
    }
    let user = sqlx::query_as::<_, User>(
        "UPDATE users SET is_admin=?2,updated_at=strftime('%Y-%m-%dT%H:%M:%f000Z','now') WHERE id=?1 RETURNING *",
    )
    .bind(target)
    .bind(admin)
    .fetch_optional(&mut *tx)
    .await?;
    let Some(user) = user else {
        return Ok(RoleChange::Missing);
    };
    if !admin {
        codeyard_db_core::queries::sqlite::ci::stop_actor_ci_runs(&mut tx, target).await?;
    }
    sqlx::query("INSERT INTO audit_log(actor_id,action,target_type,target_id,metadata) VALUES(?1,'user.admin_role','user',?2,?3)")
        .bind(actor.user_id).bind(target).bind(serde_json::json!({"is_admin":admin,"outcome":"applied"}).to_string()).execute(&mut *tx).await?;
    tx.commit().await?;
    Ok(RoleChange::Updated(Box::new(user)))
}

pub async fn register_password(
    pool: &SqlitePool,
    username: &str,
    email: Option<&str>,
    hash: &str,
    administrator: Option<(i64, &str, bool)>,
) -> Result<AccountAdmission> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let config =
        sqlx::query_as::<_, NodeConfiguration>("SELECT * FROM node_configuration WHERE id=1")
            .fetch_optional(&mut *tx)
            .await?;
    let Some(config) = config.filter(|config| config.setup_completed) else {
        return Ok(AccountAdmission::SetupRequired);
    };
    if let Some((actor, session_id, _)) = administrator {
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
    if email_owned(&mut tx, email, -1).await?
        || sqlx::query_scalar::<_, bool>(
            "SELECT EXISTS(SELECT 1 FROM users WHERE lower(username)=lower(?1))",
        )
        .bind(username)
        .fetch_one(&mut *tx)
        .await?
    {
        return Ok(AccountAdmission::Conflict);
    }
    if config.node_mode == "private" && administrator.is_none() {
        if sqlx::query_scalar::<_, bool>("SELECT EXISTS(SELECT 1 FROM signup_requests WHERE status='pending' AND (username_key=?1 OR (?2 IS NOT NULL AND email_key=?2)))")
            .bind(username.to_lowercase()).bind(email.map(str::to_lowercase)).fetch_one(&mut *tx).await? { return Ok(AccountAdmission::Conflict); }
        sqlx::query("INSERT INTO signup_requests (username,email,password_hash,username_key,email_key) VALUES (?1,?2,?3,?4,?5)")
            .bind(username)
            .bind(email)
            .bind(hash)
            .bind(username.to_lowercase())
            .bind(email.map(str::to_lowercase))
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        return Ok(AccountAdmission::Requested);
    }
    let user = sqlx::query_as::<_, User>("INSERT INTO users (username,email,password_hash,is_admin,must_change_password) VALUES (?1,?2,?3,?5,?4) RETURNING *")
        .bind(username).bind(email).bind(hash).bind(administrator.is_some()).bind(administrator.is_some_and(|(_, _, admin)| admin)).fetch_one(&mut *tx).await?;
    if let Some((actor, _, _)) = administrator {
        sqlx::query("INSERT INTO audit_log(actor_id,action,target_type,target_id,metadata) VALUES(?1,'user.create','user',?2,?3)")
            .bind(actor).bind(user.id).bind(serde_json::json!({"is_admin":user.is_admin,"must_change_password":true}).to_string()).execute(&mut *tx).await?;
    }
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
    // After the numbered names, a few random ones, so taken numbers cannot block the sign-in.
    for suffix in 0..36 {
        if suffix >= 32 {
            name = format!("{base}_{}", &codeyard_support::crypto::random_hex()[..8]);
        } else if suffix > 0 {
            name = format!("{base}_{suffix}");
        }
        if organization_name_exists(&mut tx, &name).await? {
            continue;
        }
        let user = sqlx::query_as::<_, User>("INSERT INTO users (username,display_name) VALUES (?1,?2) ON CONFLICT DO NOTHING RETURNING *")
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
    session: Option<&str>,
) -> Result<Option<User>> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    if let Some(session) = session {
        if !current_password(&mut tx, user_id, expected, session, true).await? {
            return Ok(None);
        }
    }
    let changed = sqlx::query_as::<_, User>("UPDATE users SET password_hash=?3,must_change_password=CASE WHEN ?4 THEN FALSE ELSE must_change_password END,updated_at=strftime('%Y-%m-%dT%H:%M:%f000Z','now') WHERE id=?1 AND password_hash=?2 AND NOT is_banned RETURNING *")
        .bind(user_id).bind(expected).bind(replacement).bind(clear_must_change).fetch_optional(&mut *tx).await?;
    if changed.is_some() && revoke_sessions {
        revoke_credentials(&mut tx, user_id).await?;
    }
    tx.commit().await?;
    Ok(changed)
}

/// Ends the account's sessions and access tokens, and its reset links: a reset link still in a
/// mailbox would undo the new password.
async fn revoke_credentials(tx: &mut Transaction<'_, Sqlite>, user_id: i64) -> Result<()> {
    sqlx::query("DELETE FROM personal_access_tokens WHERE user_id=?1")
        .bind(user_id)
        .execute(&mut **tx)
        .await?;
    sqlx::query("DELETE FROM sessions WHERE user_id=?1")
        .bind(user_id)
        .execute(&mut **tx)
        .await?;
    sqlx::query("DELETE FROM email_tokens WHERE user_id=?1 AND purpose='reset'")
        .bind(user_id)
        .execute(&mut **tx)
        .await?;
    Ok(())
}

/// Account facts to pin password verification outside the consuming transaction.
pub async fn password_reset_account(pool: &SqlitePool, token_hash: &str) -> Result<Option<User>> {
    Ok(sqlx::query_as("SELECT u.* FROM users u JOIN email_tokens t ON t.user_id=u.id WHERE t.purpose='reset' AND t.token_hash=?1 AND t.expires_at>strftime('%Y-%m-%dT%H:%M:%f000Z','now') AND u.email=t.email AND u.password_hash IS NOT NULL AND NOT u.is_banned")
        .bind(token_hash).fetch_optional(pool).await?)
}

/// Consumes the link, compares the verified password state and revokes credentials atomically.
pub async fn reset_password_with_link(
    pool: &SqlitePool,
    token_hash: &str,
    replacement: &str,
    expected_hash: &str,
    expected_temporary: bool,
) -> Result<Option<i64>> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let taken: Option<i64> = sqlx::query_scalar(
        "DELETE FROM email_tokens
         WHERE purpose = 'reset' AND token_hash = ?1
           AND expires_at > strftime('%Y-%m-%dT%H:%M:%f000Z','now')
           AND EXISTS (SELECT 1 FROM users u
                       WHERE u.id = email_tokens.user_id AND u.email = email_tokens.email
                         AND NOT u.is_banned)
         RETURNING user_id",
    )
    .bind(token_hash)
    .fetch_optional(&mut *tx)
    .await?;
    let Some(user_id) = taken else {
        return Ok(None);
    };
    let changed = sqlx::query(
        "UPDATE users SET password_hash=?2,must_change_password=FALSE,updated_at=strftime('%Y-%m-%dT%H:%M:%f000Z','now')
         WHERE id=?1 AND password_hash=?3 AND must_change_password=?4 AND NOT is_banned",
    )
    .bind(user_id)
    .bind(replacement)
    .bind(expected_hash)
    .bind(expected_temporary)
    .execute(&mut *tx)
    .await?
    .rows_affected()
        == 1;
    if !changed {
        return Ok(None);
    }
    revoke_credentials(&mut tx, user_id).await?;
    tx.commit().await?;
    Ok(Some(user_id))
}

use codeyard_db_core::queries::sqlite::users::transitions::credential_live;

use codeyard_db_core::queries::sqlite::users::transitions::live_session;

use codeyard_db_core::queries::sqlite::users::transitions::credential_live;

use codeyard_db_core::queries::sqlite::users::transitions::live_session;
