use crate::backend::db::users::transitions::{
    AccountAdmission, AdministratorActor, BanChange, EmailChange, RoleChange, SetupClaim,
};
use sqlx::PgPool;
// Atomic identity transitions. No provider, filesystem or KDF work under locks.
use crate::backend::db::models::{NodeConfiguration, User};
use anyhow::Result;
use sqlx::{PgConnection, Postgres, Transaction};

pub(crate) async fn lock_membership(tx: &mut Transaction<'_, Postgres>) -> Result<()> {
    // Lock order: account membership 37002 -> (for inserts) namespace 37003 ->
    // configuration/actor/request rows -> name predicates. Organization creation
    // takes both transaction locks before the actor row too.
    sqlx::query("SELECT pg_advisory_xact_lock(hashtext(current_schema()), 37002)")
        .execute(&mut **tx)
        .await?;
    Ok(())
}

/// The shared form of the membership lock. A transaction takes it before any row lock.
/// Transitions hold the exclusive form while they lock organizations and then accounts,
/// and an acknowledgement locks an account before organizations. Without the shared form,
/// those two orders could each wait for a row the other holds.
pub(crate) async fn lock_membership_shared(connection: &mut PgConnection) -> Result<()> {
    sqlx::query("SELECT pg_advisory_xact_lock_shared(hashtext(current_schema()), 37002)")
        .execute(connection)
        .await?;
    Ok(())
}

pub(crate) async fn lock_namespace(tx: &mut Transaction<'_, Postgres>) -> Result<()> {
    // Must follow lock_membership, before any actor/name examination. Held to commit.
    sqlx::query("SELECT pg_advisory_xact_lock(hashtext(current_schema()), 37003)")
        .execute(&mut **tx)
        .await?;
    Ok(())
}

pub(crate) async fn organization_name_exists(
    tx: &mut Transaction<'_, Postgres>,
    username: &str,
) -> Result<bool> {
    // Exact namespace policy: no case folding.
    Ok(
        sqlx::query_scalar::<_, bool>("SELECT EXISTS(SELECT 1 FROM organizations WHERE name=$1)")
            .bind(username)
            .fetch_one(&mut **tx)
            .await?,
    )
}

pub(crate) async fn require_user_namespace(
    tx: &mut Transaction<'_, Postgres>,
    username: &str,
) -> Result<()> {
    if organization_name_exists(tx, username).await? {
        anyhow::bail!("Account namespace conflict");
    }
    Ok(())
}

/// The account row is locked by the caller before retaining its session.
pub(crate) async fn live_session(
    tx: &mut Transaction<'_, Postgres>,
    actor: i64,
    session: &str,
) -> Result<bool> {
    let expiry: Option<chrono::DateTime<chrono::Utc>> =
        sqlx::query_scalar("SELECT expires_at FROM sessions WHERE id=$1 AND user_id=$2 FOR SHARE")
            .bind(session)
            .bind(actor)
            .fetch_optional(&mut **tx)
            .await?;
    Ok(
        sqlx::query_scalar("SELECT COALESCE($1::timestamptz>clock_timestamp(),FALSE)")
            .bind(expiry)
            .fetch_one(&mut **tx)
            .await?,
    )
}

pub(crate) async fn current_password(
    tx: &mut Transaction<'_, Postgres>,
    user: i64,
    expected: &str,
    session: &str,
    allow_must_change: bool,
) -> Result<bool> {
    lock_membership_shared(tx).await?;
    let valid: bool = sqlx::query_scalar(
        "SELECT password_hash=$2 AND NOT is_banned AND ($3 OR NOT must_change_password) FROM users WHERE id=$1 FOR NO KEY UPDATE",
    ).bind(user).bind(expected).bind(allow_must_change).fetch_optional(&mut **tx).await?.unwrap_or(false);
    Ok(valid && live_session(tx, user, session).await?)
}

pub(crate) async fn active_admin_session(
    tx: &mut Transaction<'_, Postgres>,
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

pub(crate) async fn active_admin(tx: &mut Transaction<'_, Postgres>, actor: i64) -> Result<bool> {
    // Serialize authority changes without blocking CI notification foreign keys.
    Ok(sqlx::query_scalar::<_, bool>("SELECT is_admin AND NOT is_banned AND NOT must_change_password FROM users WHERE id = $1 FOR NO KEY UPDATE")
        .bind(actor).fetch_optional(&mut **tx).await?.unwrap_or(false))
}

/// Locks every organization currently owned by `user` before the account row.
/// Membership changes take the same transaction lock before changing ownership,
/// so this first query is a current ownership snapshot rather than a stale candidate list.
pub(crate) async fn owned_organizations_for_account_change(
    tx: &mut Transaction<'_, Postgres>,
    user: i64,
) -> Result<Vec<i64>> {
    lock_membership(tx).await?;
    let candidates: Vec<i64> = sqlx::query_scalar(
        "SELECT org_id FROM org_memberships WHERE user_id=$1 AND role='owner' ORDER BY org_id",
    )
    .bind(user)
    .fetch_all(&mut **tx)
    .await?;
    if candidates.is_empty() {
        return Ok(candidates);
    }
    let _: Vec<i64> =
        sqlx::query_scalar("SELECT id FROM organizations WHERE id=ANY($1) ORDER BY id FOR UPDATE")
            .bind(&candidates)
            .fetch_all(&mut **tx)
            .await?;
    Ok(sqlx::query_scalar(
        "SELECT org_id FROM org_memberships WHERE user_id=$1 AND role='owner' AND org_id=ANY($2) ORDER BY org_id",
    )
    .bind(user)
    .bind(&candidates)
    .fetch_all(&mut **tx)
    .await?)
}

/// Stops organization work only where this restriction removed the last usable
/// owner. Repository locks close admission while the account change commits.
pub(crate) async fn stop_ownerless_organization_ci(
    tx: &mut Transaction<'_, Postgres>,
    organizations: &[i64],
) -> Result<()> {
    if organizations.is_empty() {
        return Ok(());
    }
    let ownerless: Vec<i64> = sqlx::query_scalar(
        "SELECT o.id FROM organizations o WHERE o.id=ANY($1) AND NOT EXISTS(SELECT 1 FROM org_memberships m JOIN users u ON u.id=m.user_id WHERE m.org_id=o.id AND m.role='owner' AND NOT u.is_banned AND NOT u.must_change_password) ORDER BY o.id",
    )
    .bind(organizations)
    .fetch_all(&mut **tx)
    .await?;
    if ownerless.is_empty() {
        return Ok(());
    }
    let _: Vec<i64> = sqlx::query_scalar(
        "SELECT id FROM repositories WHERE owner_kind='org' AND owner_user_id IS NULL AND owner_org_id=ANY($1) ORDER BY id FOR UPDATE",
    )
    .bind(&ownerless)
    .fetch_all(&mut **tx)
    .await?;
    crate::backend::db::postgres::ci::stop_organization_ci_runs(tx, &ownerless).await
}

pub async fn complete_setup(
    pool: &PgPool,
    username: &str,
    email: Option<&str>,
    hash: &str,
    mode: &str,
    recovery: Option<(i64, &str)>,
) -> Result<SetupClaim> {
    let mut tx = pool.begin().await?;
    lock_membership(&mut tx).await?;
    lock_namespace(&mut tx).await?;
    let completed = sqlx::query_scalar::<_, bool>(
        "SELECT setup_completed FROM node_configuration WHERE id = 1 FOR UPDATE",
    )
    .fetch_optional(&mut *tx)
    .await?
    .unwrap_or(false);
    // Every exit that changes nothing rolls back explicitly. A dropped transaction only queues
    // its ROLLBACK for the pooled connection's next query, which would leave the setup locks held.
    if completed {
        tx.rollback().await?;
        return Ok(SetupClaim::AlreadyCompleted);
    }
    let count = sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM users")
        .fetch_one(&mut *tx)
        .await?;
    let user = if count == 0 {
        if organization_name_exists(&mut tx, username).await? {
            tx.rollback().await?;
            return Ok(SetupClaim::ExistingAccounts);
        }
        sqlx::query_as::<_, User>("INSERT INTO users (username,email,password_hash,is_admin) VALUES ($1,$2,$3,TRUE) RETURNING *")
            .bind(username).bind(email).bind(hash).fetch_one(&mut *tx).await?
    } else if let Some((id, expected_hash)) = recovery.filter(|_| count == 1) {
        // Complete setup for a sole active admin that was provisioned separately: the
        // verified credentials must match its stored hash exactly. Never adopt an outsider.
        let user = sqlx::query_as::<_, User>("UPDATE users SET password_hash = $3, updated_at = NOW() WHERE id = $1 AND username = $2 AND password_hash = $4 AND is_admin AND NOT is_banned RETURNING *")
            .bind(id).bind(username).bind(hash).bind(expected_hash).fetch_optional(&mut *tx).await?;
        match user {
            Some(user) => user,
            None => {
                tx.rollback().await?;
                return Ok(SetupClaim::ExistingAccounts);
            }
        }
    } else {
        tx.rollback().await?;
        return Ok(SetupClaim::ExistingAccounts);
    };
    sqlx::query("INSERT INTO node_configuration (id,node_mode,setup_completed) VALUES (1,$1,TRUE) ON CONFLICT (id) DO UPDATE SET node_mode=$1,setup_completed=TRUE,updated_at=NOW()")
        .bind(mode).execute(&mut *tx).await?;
    tx.commit().await?;
    Ok(SetupClaim::Completed(Box::new(user)))
}

pub async fn create_cli_user(
    pool: &PgPool,
    username: &str,
    email: Option<&str>,
) -> Result<AccountAdmission> {
    let mut tx = pool.begin().await?;
    lock_membership(&mut tx).await?;
    lock_namespace(&mut tx).await?;
    let completed = sqlx::query_scalar::<_, bool>(
        "SELECT setup_completed FROM node_configuration WHERE id=1 FOR UPDATE",
    )
    .fetch_optional(&mut *tx)
    .await?
    .unwrap_or(false);
    if !completed {
        tx.rollback().await?;
        return Ok(AccountAdmission::SetupRequired);
    }
    require_user_namespace(&mut tx, username).await?;
    let user =
        sqlx::query_as::<_, User>("INSERT INTO users (username,email) VALUES ($1,$2) RETURNING *")
            .bind(username)
            .bind(email)
            .fetch_one(&mut *tx)
            .await?;
    tx.commit().await?;
    Ok(AccountAdmission::User(Box::new(user)))
}

pub async fn bootstrap_admin(
    pool: &PgPool,
    username: &str,
    hash: &str,
    mode: &str,
) -> Result<SetupClaim> {
    let mut tx = pool.begin().await?;
    lock_membership(&mut tx).await?;
    lock_namespace(&mut tx).await?;
    let completed = sqlx::query_scalar::<_, bool>(
        "SELECT setup_completed FROM node_configuration WHERE id=1 FOR UPDATE",
    )
    .fetch_optional(&mut *tx)
    .await?
    .unwrap_or(false);
    if completed {
        tx.rollback().await?;
        return Ok(SetupClaim::AlreadyCompleted);
    }
    if organization_name_exists(&mut tx, username).await?
        || sqlx::query_scalar::<_, bool>("SELECT EXISTS(SELECT 1 FROM users WHERE username=$1)")
            .bind(username)
            .fetch_one(&mut *tx)
            .await?
    {
        tx.rollback().await?;
        return Ok(SetupClaim::ExistingAccounts);
    }
    let user = sqlx::query_as::<_, User>(
        "INSERT INTO users (username,password_hash,is_admin) VALUES ($1,$2,TRUE) RETURNING *",
    )
    .bind(username)
    .bind(hash)
    .fetch_one(&mut *tx)
    .await?;
    sqlx::query("INSERT INTO node_configuration (id,node_mode,setup_completed) VALUES (1,$1,TRUE) ON CONFLICT (id) DO UPDATE SET node_mode=$1,setup_completed=TRUE,updated_at=NOW()")
        .bind(mode).execute(&mut *tx).await?;
    tx.commit().await?;
    Ok(SetupClaim::Completed(Box::new(user)))
}

pub async fn change_ban(
    pool: &PgPool,
    actor: impl Into<AdministratorActor>,
    target: i64,
    banned: bool,
) -> Result<BanChange> {
    let actor = actor.into();
    let mut tx = pool.begin().await?;
    let organizations = owned_organizations_for_account_change(&mut tx, target).await?;
    if !active_admin_session(&mut tx, &actor).await? {
        return Ok(BanChange::Denied);
    }
    if actor.user_id == target {
        return Ok(BanChange::SelfBan);
    }
    let Some(user) = sqlx::query_as::<_, User>("SELECT * FROM users WHERE id=$1 FOR NO KEY UPDATE")
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
        "UPDATE users SET is_banned=$2,updated_at=NOW() WHERE id=$1 RETURNING *",
    )
    .bind(target)
    .bind(banned)
    .fetch_one(&mut *tx)
    .await?;
    if banned {
        sqlx::query("DELETE FROM sessions WHERE user_id=$1")
            .bind(target)
            .execute(&mut *tx)
            .await?;
        sqlx::query("DELETE FROM personal_access_tokens WHERE user_id=$1")
            .bind(target)
            .execute(&mut *tx)
            .await?;
        // Stop the account's own work everywhere. If it was the last usable owner,
        // stop every actor's work in that organization's repositories as well.
        crate::backend::db::postgres::ci::stop_actor_ci_runs(&mut tx, target).await?;
        stop_ownerless_organization_ci(&mut tx, &organizations).await?;
    }
    tx.commit().await?;
    // Repository rows, objects and disk contents are preserved on suspension.
    Ok(BanChange::Updated(Box::new(user)))
}

/// Sets the account's address. Another account's address is refused in the same transaction.
pub async fn change_email(
    pool: &PgPool,
    user_id: i64,
    email: &str,
    expected: &str,
    session: &str,
) -> Result<EmailChange> {
    let mut tx = pool.begin().await?;
    if !current_password(&mut tx, user_id, expected, session, false).await? {
        return Ok(EmailChange::Missing);
    }
    if sqlx::query_scalar::<_, bool>("SELECT EXISTS(SELECT 1 FROM users WHERE email=$1 AND id<>$2)")
        .bind(email)
        .bind(user_id)
        .fetch_one(&mut *tx)
        .await?
    {
        return Ok(EmailChange::Taken);
    }
    let user = sqlx::query_as::<_, User>(
        "UPDATE users SET email=$2,updated_at=NOW() WHERE id=$1 RETURNING *",
    )
    .bind(user_id)
    .bind(email)
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
    pool: &PgPool,
    actor: impl Into<AdministratorActor>,
    target: i64,
    admin: bool,
) -> Result<RoleChange> {
    let actor = actor.into();
    let mut tx = pool.begin().await?;
    lock_membership(&mut tx).await?;
    if !active_admin_session(&mut tx, &actor).await? {
        return Ok(RoleChange::Denied);
    }
    if actor.user_id == target {
        return Ok(RoleChange::SelfChange);
    }
    let user = sqlx::query_as::<_, User>(
        "UPDATE users SET is_admin=$2,updated_at=NOW() WHERE id=$1 RETURNING *",
    )
    .bind(target)
    .bind(admin)
    .fetch_optional(&mut *tx)
    .await?;
    let Some(user) = user else {
        return Ok(RoleChange::Missing);
    };
    tx.commit().await?;
    Ok(RoleChange::Updated(Box::new(user)))
}

pub async fn register_password(
    pool: &PgPool,
    username: &str,
    email: Option<&str>,
    hash: &str,
    administrator: Option<(i64, &str, bool)>,
) -> Result<AccountAdmission> {
    let mut tx = pool.begin().await?;
    lock_membership(&mut tx).await?;
    lock_namespace(&mut tx).await?;
    let config = sqlx::query_as::<_, NodeConfiguration>(
        "SELECT * FROM node_configuration WHERE id=1 FOR UPDATE",
    )
    .fetch_optional(&mut *tx)
    .await?;
    let Some(config) = config.filter(|config| config.setup_completed) else {
        return Ok(AccountAdmission::SetupRequired);
    };
    if let Some((actor, session_id, _)) = administrator {
        if config.node_mode != "private" || !active_admin(&mut tx, actor).await? {
            return Ok(AccountAdmission::Denied);
        }
        // Retain session authority through insertion, including against logout.
        let expiry: Option<chrono::DateTime<chrono::Utc>> = sqlx::query_scalar(
            "SELECT expires_at FROM sessions WHERE id=$1 AND user_id=$2 FOR SHARE",
        )
        .bind(session_id)
        .bind(actor)
        .fetch_optional(&mut *tx)
        .await?;
        // A separate statement checks wall-clock expiry after any lock wait.
        let live: bool =
            sqlx::query_scalar("SELECT COALESCE($1::timestamptz>clock_timestamp(),FALSE)")
                .bind(expiry)
                .fetch_one(&mut *tx)
                .await?;
        if !live {
            return Ok(AccountAdmission::Denied);
        }
    }
    if organization_name_exists(&mut tx, username).await? {
        return Ok(AccountAdmission::Conflict);
    }
    if sqlx::query_scalar::<_, bool>("SELECT EXISTS(SELECT 1 FROM users WHERE username=$1 OR ($2::text IS NOT NULL AND email=$2))")
        .bind(username).bind(email).fetch_one(&mut *tx).await? { return Ok(AccountAdmission::Conflict); }
    if config.node_mode == "private" && administrator.is_none() {
        if sqlx::query_scalar::<_, bool>("SELECT EXISTS(SELECT 1 FROM signup_requests WHERE status='pending' AND (LOWER(username)=LOWER($1) OR ($2::text IS NOT NULL AND LOWER(email)=LOWER($2))))")
            .bind(username).bind(email).fetch_one(&mut *tx).await? { return Ok(AccountAdmission::Conflict); }
        sqlx::query("INSERT INTO signup_requests (username,email,password_hash) VALUES ($1,$2,$3)")
            .bind(username)
            .bind(email)
            .bind(hash)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        return Ok(AccountAdmission::Requested);
    }
    let user = sqlx::query_as::<_, User>("INSERT INTO users (username,email,password_hash,is_admin,must_change_password) VALUES ($1,$2,$3,$5,$4) RETURNING *")
        .bind(username).bind(email).bind(hash).bind(administrator.is_some()).bind(administrator.is_some_and(|(_, _, admin)| admin)).fetch_one(&mut *tx).await?;
    tx.commit().await?;
    Ok(AccountAdmission::User(Box::new(user)))
}

pub async fn admit_oauth_account(
    pool: &PgPool,
    provider: &str,
    provider_id: &str,
    username_base: &str,
    display_name: Option<&str>,
) -> Result<AccountAdmission> {
    let mut tx = pool.begin().await?;
    lock_membership(&mut tx).await?;
    lock_namespace(&mut tx).await?;
    let config = sqlx::query_as::<_, NodeConfiguration>(
        "SELECT * FROM node_configuration WHERE id=1 FOR UPDATE",
    )
    .fetch_optional(&mut *tx)
    .await?;
    let Some(config) = config.filter(|config| config.setup_completed) else {
        return Ok(AccountAdmission::SetupRequired);
    };
    let linked = sqlx::query_as::<_, User>("SELECT u.* FROM users u JOIN oauth_accounts a ON a.user_id=u.id WHERE a.provider=$1 AND a.provider_user_id=$2 FOR UPDATE OF u")
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
        let user = sqlx::query_as::<_, User>("INSERT INTO users (username,display_name) VALUES ($1,$2) ON CONFLICT (username) DO NOTHING RETURNING *")
            .bind(&name).bind(display_name).fetch_optional(&mut *tx).await?;
        if let Some(user) = user {
            // Login requires neither offline access nor token persistence. Do not retain
            // provider bearer credentials as plaintext just to establish a local identity.
            sqlx::query(
                "INSERT INTO oauth_accounts (user_id,provider,provider_user_id) VALUES ($1,$2,$3)",
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
    pool: &PgPool,
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
    let mut tx = pool.begin().await?;
    lock_membership(&mut tx).await?;
    let private = sqlx::query_scalar::<_, bool>("SELECT setup_completed AND node_mode='private' FROM node_configuration WHERE id=1 FOR UPDATE")
        .fetch_optional(&mut *tx).await?.unwrap_or(false);
    if !private || !active_admin(&mut tx, actor).await? {
        return Ok(false);
    }
    let active =
        sqlx::query_scalar::<_, bool>("SELECT NOT is_banned FROM users WHERE id=$1 FOR UPDATE")
            .bind(user)
            .fetch_optional(&mut *tx)
            .await?
            .unwrap_or(false);
    if !active {
        return Ok(false);
    }
    let inserted = sqlx::query("INSERT INTO oauth_accounts (user_id,provider,provider_user_id) VALUES ($1,$2,$3) ON CONFLICT (provider,provider_user_id) DO NOTHING")
        .bind(user).bind(provider).bind(subject).execute(&mut *tx).await?.rows_affected() == 1;
    tx.commit().await?;
    Ok(inserted)
}

pub async fn compare_exchange_password(
    pool: &PgPool,
    user_id: i64,
    expected: &str,
    replacement: &str,
    clear_must_change: bool,
    revoke_sessions: bool,
    session: Option<&str>,
) -> Result<Option<User>> {
    let mut tx = pool.begin().await?;
    if clear_must_change {
        owned_organizations_for_account_change(&mut tx, user_id).await?;
    }
    if let Some(session) = session {
        if !current_password(&mut tx, user_id, expected, session, true).await? {
            return Ok(None);
        }
    }
    let changed = sqlx::query_as::<_, User>("UPDATE users SET password_hash=$3,must_change_password=CASE WHEN $4 THEN FALSE ELSE must_change_password END,updated_at=NOW() WHERE id=$1 AND password_hash=$2 AND NOT is_banned RETURNING *")
        .bind(user_id).bind(expected).bind(replacement).bind(clear_must_change).fetch_optional(&mut *tx).await?;
    if changed.is_some() && revoke_sessions {
        sqlx::query("DELETE FROM personal_access_tokens WHERE user_id=$1")
            .bind(user_id)
            .execute(&mut *tx)
            .await?;
        sqlx::query("DELETE FROM sessions WHERE user_id=$1")
            .bind(user_id)
            .execute(&mut *tx)
            .await?;
    }
    tx.commit().await?;
    Ok(changed)
}
