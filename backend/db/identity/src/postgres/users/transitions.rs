use crate::users::transitions::AccountAdmission;
use crate::users::transitions::AdministratorActor;
use crate::users::transitions::BanChange;
use crate::users::transitions::EmailChange;
use crate::users::transitions::RoleChange;
use crate::users::transitions::SetupClaim;
use anyhow::Result;
use codeyard_database::models::NodeConfiguration;
use codeyard_database::models::User;
use codeyard_database::pool::PgPool;
use codeyard_database::queries::postgres::users::transitions::account_locks;
use codeyard_database::queries::postgres::users::transitions::credential_live;
use codeyard_database::queries::postgres::users::transitions::live_session;
use codeyard_database::queries::postgres::users::transitions::lock_accounts_shared;
use sqlx::PgConnection;
use sqlx::Postgres;
use sqlx::Transaction;

/// Take the complete account set before namespace or row locks. Account restrictions
/// are exclusive; authority readers retain shared locks through their effect.
/// Never add another account after taking rows, or upgrade a shared advisory lock.
pub async fn lock_accounts(connection: &mut PgConnection, accounts: &[i64]) -> Result<()> {
    account_locks(connection, accounts, false).await
}

pub async fn lock_namespace(tx: &mut Transaction<'_, Postgres>, name: &str) -> Result<()> {
    // Setup excludes all initial accounts, including bot creation. Once setup completes,
    // normal namespace reservations never take the setup lock.
    let completed: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM node_configuration WHERE id=1 AND setup_completed)",
    )
    .fetch_one(&mut **tx)
    .await?;
    if !completed {
        sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended(COALESCE(current_schema(),'') || ':setup',0))")
            .execute(&mut **tx).await?;
    }
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended(COALESCE(current_schema(),'') || ':namespace:' || lower($1 COLLATE \"C\"),0))")
        .bind(name).execute(&mut **tx).await?;
    Ok(())
}

pub(crate) async fn organization_name_exists(
    tx: &mut Transaction<'_, Postgres>,
    username: &str,
) -> Result<bool> {
    // Preserve canonical spelling while reserving ASCII case variants together.
    Ok(
        sqlx::query_scalar::<_, bool>("SELECT EXISTS(SELECT 1 FROM organizations WHERE lower(name COLLATE \"C\")=lower($1 COLLATE \"C\"))")
            .bind(username)
            .fetch_one(&mut **tx)
            .await?,
    )
}

pub(crate) use codeyard_database::queries::postgres::users::transitions::lock_emails;

pub(crate) async fn email_owned(
    tx: &mut Transaction<'_, Postgres>,
    email: Option<&str>,
    except: i64,
) -> Result<bool> {
    if let Some(email) = email {
        lock_emails(tx, &[email]).await?;
    }
    Ok(sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM users WHERE lower(email)=lower($1) AND id<>$2) OR EXISTS(SELECT 1 FROM user_emails WHERE lower(email)=lower($1) AND user_id<>$2)")
        .bind(email).bind(except).fetch_one(&mut **tx).await?)
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

/// Call within a transaction; authority and session locks are retained until commit.
async fn active_account_locked(
    connection: &mut PgConnection,
    actor: i64,
    exclusive: bool,
) -> Result<bool> {
    lock_accounts_shared(connection, &[actor]).await?;
    let lock = if exclusive {
        "FOR NO KEY UPDATE"
    } else {
        "FOR SHARE"
    };
    Ok(sqlx::query_scalar(&format!(
        "SELECT NOT is_banned AND NOT must_change_password FROM users WHERE id=$1 {lock}"
    ))
    .bind(actor)
    .fetch_optional(connection)
    .await?
    .unwrap_or(false))
}

pub async fn native_account(connection: &mut sqlx::PgConnection, actor: i64) -> Result<bool> {
    Ok(
        sqlx::query_scalar(&crate::users::transitions::native_account_sql())
            .bind(actor)
            .fetch_one(connection)
            .await?,
    )
}

pub(crate) async fn active_account(connection: &mut PgConnection, actor: i64) -> Result<bool> {
    active_account_locked(connection, actor, true).await
}

async fn active_session_locked(
    connection: &mut PgConnection,
    actor: i64,
    session: &str,
    exclusive: bool,
) -> Result<bool> {
    Ok(active_account_locked(connection, actor, exclusive).await?
        && native_account(connection, actor).await?
        && live_session(connection, actor, session).await?)
}

pub(crate) async fn active_session(
    connection: &mut PgConnection,
    actor: i64,
    session: &str,
) -> Result<bool> {
    active_session_locked(connection, actor, session, true).await
}

/// For operations which do not modify the account or upgrade its lock later.
pub(crate) async fn active_session_read(
    connection: &mut PgConnection,
    actor: i64,
    session: &str,
) -> Result<bool> {
    active_session_locked(connection, actor, session, false).await
}

pub async fn require_account(
    tx: &mut Transaction<'_, Postgres>,
    actor: i64,
    credential: &codeyard_database::queries::users::accounts::AccountCredential<'_>,
) -> Result<()> {
    require_account_with_mode(tx, actor, credential, true, false).await
}

/// Account-local mutations retain their original serialization and quota lock.
pub(crate) async fn require_account_exclusive(
    tx: &mut Transaction<'_, Postgres>,
    actor: i64,
    credential: &codeyard_database::queries::users::accounts::AccountCredential<'_>,
) -> Result<()> {
    require_account_with_mode(tx, actor, credential, true, true).await?;
    anyhow::ensure!(
        native_account(tx, actor).await?,
        codeyard_types::credentials::AccountRestricted
    );
    Ok(())
}

pub async fn require_read_account(
    tx: &mut Transaction<'_, Postgres>,
    actor: i64,
    credential: &codeyard_database::queries::users::accounts::AccountCredential<'_>,
) -> Result<()> {
    require_account_with_mode(tx, actor, credential, false, false).await
}

async fn require_account_with_mode(
    tx: &mut Transaction<'_, Postgres>,
    actor: i64,
    credential: &codeyard_database::queries::users::accounts::AccountCredential<'_>,
    write: bool,
    exclusive_actor: bool,
) -> Result<()> {
    use codeyard_types::credentials::AccountRestricted;
    // Repository/org authority already holds a shared actor lock. Upgrading it
    // after acquiring resource locks deadlocks concurrent requests by one actor.
    // Shared is sufficient to prevent account restrictions through commit.
    anyhow::ensure!(
        active_account_locked(tx, actor, exclusive_actor).await?,
        AccountRestricted
    );
    anyhow::ensure!(
        credential_live(tx, actor, credential, write).await?,
        AccountRestricted
    );
    Ok(())
}

pub(crate) async fn current_password(
    tx: &mut Transaction<'_, Postgres>,
    user: i64,
    expected: &str,
    session: &str,
    allow_must_change: bool,
) -> Result<bool> {
    lock_accounts_shared(tx, &[user]).await?;
    let valid: bool = sqlx::query_scalar(
        "SELECT password_hash=$2 AND NOT is_banned AND ($3 OR NOT must_change_password) FROM users WHERE id=$1 FOR NO KEY UPDATE",
    ).bind(user).bind(expected).bind(allow_must_change).fetch_optional(&mut **tx).await?.unwrap_or(false);
    Ok(valid && live_session(tx, user, session).await?)
}

pub async fn active_admin_session(
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

/// Live site administration through an original session or personal-token credential.
pub async fn active_administrator(
    tx: &mut Transaction<'_, Postgres>,
    actor: &codeyard_database::queries::credentials::Actor,
) -> Result<bool> {
    let Some(credential) = actor.credential() else {
        return Ok(false);
    };
    Ok(active_admin(tx, actor.user_id).await?
        && codeyard_database::queries::postgres::users::transitions::credential_live(
            tx,
            actor.user_id,
            &credential,
            true,
        )
        .await?)
}

pub async fn active_admin(tx: &mut Transaction<'_, Postgres>, actor: i64) -> Result<bool> {
    lock_accounts_shared(tx, &[actor]).await?;
    // Serialize authority changes without blocking CI notification foreign keys.
    Ok(sqlx::query_scalar::<_, bool>("SELECT is_admin AND NOT is_banned AND NOT must_change_password FROM users WHERE id = $1 FOR NO KEY UPDATE")
        .bind(actor).fetch_optional(&mut **tx).await?.unwrap_or(false))
}

/// Locks every organization currently owned by `user` before the account row.
/// Membership changes retain the target account lock before changing ownership,
/// so this first query is a current ownership snapshot rather than a stale candidate list.
/// Administrative callers lock actor and target together before entering here.
pub(crate) async fn owned_organizations_for_account_change(
    tx: &mut Transaction<'_, Postgres>,
    user: i64,
) -> Result<Vec<i64>> {
    lock_accounts(tx, &[user]).await?;
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
    codeyard_database::queries::postgres::ci::stop_organization_ci_runs(tx, &ownerless).await
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
    if let Some((user, _)) = recovery {
        lock_accounts(&mut tx, &[user]).await?;
    }
    lock_namespace(&mut tx, username).await?;
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
        sqlx::query_as::<_, User>(&format!("INSERT INTO users (home_cell,username,email,password_hash,is_admin) VALUES ({cell_home},$1,$2,$3,TRUE) RETURNING *", cell_home = codeyard_database::cells::LOCAL_HOME_SQL))
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
    anyhow::ensure!(
        codeyard_database::cells::directory::postgres::user(pool, &mut tx, &user).await?,
        codeyard_database::cells::directory::Refusal::Conflict
    );
    tx.commit().await?;
    Ok(SetupClaim::Completed(Box::new(user)))
}

pub async fn create_cli_user(
    pool: &PgPool,
    username: &str,
    email: Option<&str>,
) -> Result<AccountAdmission> {
    let mut tx = pool.begin().await?;
    lock_namespace(&mut tx, username).await?;
    let completed = sqlx::query_scalar::<_, bool>(
        "SELECT setup_completed FROM node_configuration WHERE id=1 FOR SHARE",
    )
    .fetch_optional(&mut *tx)
    .await?
    .unwrap_or(false);
    if !completed {
        tx.rollback().await?;
        return Ok(AccountAdmission::SetupRequired);
    }
    require_user_namespace(&mut tx, username).await?;
    if email_owned(&mut tx, email, -1).await? {
        return Ok(AccountAdmission::Conflict);
    }
    let user = sqlx::query_as::<_, User>(&format!(
        "INSERT INTO users (home_cell,username,email) VALUES ({cell_home},$1,$2) RETURNING *",
        cell_home = codeyard_database::cells::LOCAL_HOME_SQL
    ))
    .bind(username)
    .bind(email)
    .fetch_one(&mut *tx)
    .await?;
    if !codeyard_database::cells::directory::postgres::user(pool, &mut tx, &user).await? {
        return Ok(AccountAdmission::Conflict);
    }
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
    lock_namespace(&mut tx, username).await?;
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
        || sqlx::query_scalar::<_, bool>("SELECT EXISTS(SELECT 1 FROM users WHERE lower(username COLLATE \"C\")=lower($1 COLLATE \"C\"))")
            .bind(username)
            .fetch_one(&mut *tx)
            .await?
    {
        tx.rollback().await?;
        return Ok(SetupClaim::ExistingAccounts);
    }
    let user = sqlx::query_as::<_, User>(
        &format!("INSERT INTO users (home_cell,username,password_hash,is_admin) VALUES ({cell_home},$1,$2,TRUE) RETURNING *", cell_home = codeyard_database::cells::LOCAL_HOME_SQL),
    )
    .bind(username)
    .bind(hash)
    .fetch_one(&mut *tx)
    .await?;
    sqlx::query("INSERT INTO node_configuration (id,node_mode,setup_completed) VALUES (1,$1,TRUE) ON CONFLICT (id) DO UPDATE SET node_mode=$1,setup_completed=TRUE,updated_at=NOW()")
        .bind(mode).execute(&mut *tx).await?;
    anyhow::ensure!(
        codeyard_database::cells::directory::postgres::user(pool, &mut tx, &user).await?,
        codeyard_database::cells::directory::Refusal::Conflict
    );
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
    lock_accounts(&mut tx, &[actor.user_id, target]).await?;
    let organizations = owned_organizations_for_account_change(&mut tx, target).await?;
    if !active_admin_session(&mut tx, &actor).await? {
        return Ok(BanChange::Denied);
    }
    if actor.user_id == target {
        return Ok(BanChange::SelfBan);
    }
    let Some(user) = sqlx::query_as::<_, User>(
        "SELECT * FROM users WHERE id=$1 AND NOT is_stub FOR NO KEY UPDATE",
    )
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
        codeyard_database::queries::postgres::ci::stop_actor_ci_runs(&mut tx, target).await?;
        stop_ownerless_organization_ci(&mut tx, &organizations).await?;
    }
    sqlx::query("INSERT INTO audit_log(actor_id,action,target_type,target_id,metadata) VALUES($1,'user.ban','user',$2,$3)")
        .bind(actor.user_id).bind(target).bind(serde_json::json!({"banned":banned,"outcome":"applied"})).execute(&mut *tx).await?;
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
    if email_owned(&mut tx, Some(email), user_id).await? {
        return Ok(EmailChange::Taken);
    }
    let previous: Option<String> = if pool.directory().is_some() {
        sqlx::query_scalar("SELECT substr(email,1,256) FROM users WHERE id=$1")
            .bind(user_id)
            .fetch_one(&mut *tx)
            .await?
    } else {
        None
    };
    let changed: bool =
        sqlx::query_scalar("SELECT email IS DISTINCT FROM $2 FROM users WHERE id=$1")
            .bind(user_id)
            .bind(email)
            .fetch_one(&mut *tx)
            .await?;
    let verified: bool = sqlx::query_scalar("SELECT COALESCE(bool_or(verified),FALSE) FROM user_emails WHERE user_id=$1 AND lower(email)=lower($2)")
        .bind(user_id).bind(email).fetch_one(&mut *tx).await?;
    if changed {
        sqlx::query("DELETE FROM email_tokens WHERE user_id=$1")
            .bind(user_id)
            .execute(&mut *tx)
            .await?;
    }
    sqlx::query("DELETE FROM user_emails WHERE user_id=$1 AND lower(email)=lower($2)")
        .bind(user_id)
        .bind(email)
        .execute(&mut *tx)
        .await?;
    let user = sqlx::query_as::<_, User>(
        "UPDATE users SET email_verified=CASE WHEN email IS DISTINCT FROM $2 THEN $3 ELSE email_verified END,email=$2,updated_at=NOW() WHERE id=$1 RETURNING *",
    )
    .bind(user_id)
    .bind(email)
    .bind(verified)
    .fetch_optional(&mut *tx)
    .await;
    // A concurrent claim of the same address loses on the unique constraint.
    let user = match user {
        Err(sqlx::Error::Database(error)) if error.is_unique_violation() => {
            return Ok(EmailChange::Taken);
        }
        other => other?,
    };
    let Some(user) = user else {
        return Ok(EmailChange::Missing);
    };
    if !codeyard_database::cells::directory::postgres::user(pool, &mut tx, &user).await? {
        return Ok(EmailChange::Taken);
    }
    if !current_password(&mut tx, user_id, expected, session, false).await? {
        return Ok(EmailChange::Missing);
    }
    let cleanup = previous
        .as_deref()
        .map(|email| codeyard_types::cells::Key::new(codeyard_types::cells::KeyKind::Email, email))
        .transpose()?;
    let object = codeyard_types::cells::Object {
        kind: codeyard_types::cells::ObjectKind::User,
        id: user_id,
    };
    if let Some(key) = &cleanup {
        codeyard_database::cells::outbox::postgres::release(
            pool,
            &mut tx,
            object,
            std::slice::from_ref(key),
        )
        .await?;
    }
    tx.commit().await?;
    if let Some(key) = &cleanup {
        codeyard_database::cells::directory::postgres::finish_release(
            pool,
            object,
            std::slice::from_ref(key),
        )
        .await;
    }
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
    lock_accounts(&mut tx, &[actor.user_id, target]).await?;
    if !active_admin_session(&mut tx, &actor).await? {
        return Ok(RoleChange::Denied);
    }
    if actor.user_id == target {
        return Ok(RoleChange::SelfChange);
    }
    let user = sqlx::query_as::<_, User>(
        "UPDATE users SET is_admin=$2,updated_at=NOW() WHERE id=$1 AND NOT is_stub RETURNING *",
    )
    .bind(target)
    .bind(admin)
    .fetch_optional(&mut *tx)
    .await?;
    let Some(user) = user else {
        return Ok(RoleChange::Missing);
    };
    if !admin {
        codeyard_database::queries::postgres::ci::stop_actor_ci_runs(&mut tx, target).await?;
    }
    sqlx::query("INSERT INTO audit_log(actor_id,action,target_type,target_id,metadata) VALUES($1,'user.admin_role','user',$2,$3)")
        .bind(actor.user_id).bind(target).bind(serde_json::json!({"is_admin":admin,"outcome":"applied"})).execute(&mut *tx).await?;
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
    if let Some((actor, _, _)) = administrator {
        lock_accounts(&mut tx, &[actor]).await?;
    }
    lock_namespace(&mut tx, username).await?;
    let config = sqlx::query_as::<_, NodeConfiguration>(
        "SELECT * FROM node_configuration WHERE id=1 FOR SHARE",
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
    if email_owned(&mut tx, email, -1).await? || sqlx::query_scalar::<_, bool>("SELECT EXISTS(SELECT 1 FROM users WHERE lower(username COLLATE \"C\")=lower($1 COLLATE \"C\"))")
        .bind(username).fetch_one(&mut *tx).await? { return Ok(AccountAdmission::Conflict); }
    if config.node_mode == "private" && administrator.is_none() {
        if pool.directory().is_some() {
            let mut keys = vec![codeyard_types::cells::Key::new(
                codeyard_types::cells::KeyKind::Name,
                username,
            )?];
            if let Some(email) = email {
                keys.push(codeyard_types::cells::Key::new(
                    codeyard_types::cells::KeyKind::Email,
                    email,
                )?);
            }
            if !codeyard_database::cells::directory::postgres::available(
                pool,
                &mut tx,
                codeyard_types::cells::ObjectKind::User,
                &keys,
            )
            .await?
            {
                return Ok(AccountAdmission::Conflict);
            }
        }
        if sqlx::query_scalar::<_, bool>("SELECT EXISTS(SELECT 1 FROM signup_requests WHERE status='pending' AND (username_key=$1 OR ($2::text IS NOT NULL AND email_key=$2)))")
            .bind(username.to_lowercase()).bind(email.map(str::to_lowercase)).fetch_one(&mut *tx).await? { return Ok(AccountAdmission::Conflict); }
        sqlx::query("INSERT INTO signup_requests (username,email,password_hash,username_key,email_key) VALUES ($1,$2,$3,$4,$5)")
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
    let user = sqlx::query_as::<_, User>(&format!("INSERT INTO users (home_cell,username,email,password_hash,is_admin,must_change_password) VALUES ({cell_home},$1,$2,$3,$5,$4) RETURNING *", cell_home = codeyard_database::cells::LOCAL_HOME_SQL))
        .bind(username).bind(email).bind(hash).bind(administrator.is_some()).bind(administrator.is_some_and(|(_, _, admin)| admin)).fetch_one(&mut *tx).await?;
    if !codeyard_database::cells::directory::postgres::user(pool, &mut tx, &user).await? {
        return Ok(AccountAdmission::Conflict);
    }
    if let Some((actor, session, _)) = administrator {
        if !active_admin(&mut tx, actor).await? || !live_session(&mut tx, actor, session).await? {
            return Ok(AccountAdmission::Denied);
        }
    }
    if let Some((actor, _, _)) = administrator {
        sqlx::query("INSERT INTO audit_log(actor_id,action,target_type,target_id,metadata) VALUES($1,'user.create','user',$2,$3)")
            .bind(actor).bind(user.id).bind(serde_json::json!({"is_admin":user.is_admin,"must_change_password":true})).execute(&mut *tx).await?;
    }
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
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended(COALESCE(current_schema(),'') || ':oauth-account:' || $1 || ':' || $2,0))")
        .bind(provider).bind(provider_id).execute(&mut *tx).await?;
    let linked_account: Option<i64> = sqlx::query_scalar(
        "SELECT user_id FROM oauth_accounts WHERE provider=$1 AND provider_user_id=$2",
    )
    .bind(provider)
    .bind(provider_id)
    .fetch_optional(&mut *tx)
    .await?;
    if let Some(user) = linked_account {
        lock_accounts(&mut tx, &[user]).await?;
    }
    lock_namespace(&mut tx, username_base).await?;
    let config = sqlx::query_as::<_, NodeConfiguration>(
        "SELECT * FROM node_configuration WHERE id=1 FOR SHARE",
    )
    .fetch_optional(&mut *tx)
    .await?;
    let Some(config) = config.filter(|config| config.setup_completed) else {
        return Ok(AccountAdmission::SetupRequired);
    };
    let linked = sqlx::query_as::<_, User>("SELECT u.* FROM users u JOIN oauth_accounts a ON a.user_id=u.id WHERE a.provider=$1 AND a.provider_user_id=$2 FOR UPDATE OF u")
        .bind(provider).bind(provider_id).fetch_optional(&mut *tx).await?;
    if let Some(user) = linked {
        if linked_account != Some(user.id) || user.is_banned || user.must_change_password {
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
            name = format!("{base}_{}", &codeyard_security::crypto::random_hex()[..8]);
        } else if suffix > 0 {
            name = format!("{base}_{suffix}");
        }
        lock_namespace(&mut tx, &name).await?;
        if organization_name_exists(&mut tx, &name).await? {
            continue;
        }
        let user = sqlx::query_as::<_, User>(&format!("INSERT INTO users (home_cell,username,display_name) VALUES ({cell_home},$1,$2) ON CONFLICT DO NOTHING RETURNING *", cell_home = codeyard_database::cells::LOCAL_HOME_SQL))
            .bind(&name).bind(display_name).fetch_optional(&mut *tx).await?;
        if let Some(user) = user {
            if !codeyard_database::cells::directory::postgres::user(pool, &mut tx, &user).await? {
                sqlx::query("DELETE FROM users WHERE id=$1")
                    .bind(user.id)
                    .execute(&mut *tx)
                    .await?;
                continue;
            }
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
/// The subject comes from a verified provider flow or the local operator's identity
/// provider console. An email/profile match is never authority to link an account.
pub async fn enroll_oauth_identity(
    pool: &PgPool,
    actor: i64,
    user: i64,
    provider: &str,
    subject: &str,
) -> Result<bool> {
    if !crate::users::transitions::valid_oauth_enrollment(provider, subject) {
        return Ok(false);
    }
    let mut tx = pool.begin().await?;
    lock_accounts(&mut tx, &[actor, user]).await?;
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
    let organization =
        if let Some(name) = codeyard_types::oauth_apps::organization_from_oidc_provider(provider) {
            let id: Option<i64> = sqlx::query_scalar("SELECT id FROM organizations WHERE name=$1")
                .bind(name)
                .fetch_optional(&mut *tx)
                .await?;
            let Some(id) = id else {
                return Ok(false);
            };
            if !matches!(
                codeyard_database::queries::postgres::organizations::authority::authority(
                    &mut tx, user, id
                )
                .await?,
                codeyard_database::queries::organizations::OrganizationOutcome::Applied(_)
            ) {
                return Ok(false);
            }
            Some(id)
        } else {
            None
        };
    let inserted = sqlx::query("INSERT INTO oauth_accounts (user_id,provider,provider_user_id) VALUES ($1,$2,$3) ON CONFLICT (provider,provider_user_id) DO NOTHING")
        .bind(user).bind(provider).bind(subject).execute(&mut *tx).await?.rows_affected() == 1;
    if let Some(org) = organization.filter(|_| inserted) {
        sqlx::query("INSERT INTO audit_log(org_id,actor_id,action,target_type,target_id,metadata) VALUES($1,$2,'org.sso_enroll','user',$3,$4)")
            .bind(org).bind(actor).bind(user).bind(serde_json::json!({"provider":provider})).execute(&mut *tx).await?;
    }
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
        revoke_credentials(&mut tx, user_id).await?;
    }
    tx.commit().await?;
    Ok(changed)
}

/// Ends the account's sessions and access tokens, and its reset links: a reset link still in a
/// mailbox would undo the new password.
async fn revoke_credentials(tx: &mut Transaction<'_, Postgres>, user_id: i64) -> Result<()> {
    sqlx::query("DELETE FROM personal_access_tokens WHERE user_id=$1")
        .bind(user_id)
        .execute(&mut **tx)
        .await?;
    sqlx::query("DELETE FROM sessions WHERE user_id=$1")
        .bind(user_id)
        .execute(&mut **tx)
        .await?;
    sqlx::query("DELETE FROM email_tokens WHERE user_id=$1 AND purpose='reset'")
        .bind(user_id)
        .execute(&mut **tx)
        .await?;
    Ok(())
}

/// Account facts to pin password verification outside the consuming transaction.
pub async fn password_reset_account(pool: &PgPool, token_hash: &str) -> Result<Option<User>> {
    Ok(sqlx::query_as("SELECT u.* FROM users u JOIN email_tokens t ON t.user_id=u.id WHERE t.purpose='reset' AND t.token_hash=$1 AND t.expires_at>clock_timestamp() AND u.email=t.email AND u.password_hash IS NOT NULL AND NOT u.is_banned")
        .bind(token_hash).fetch_optional(pool).await?)
}

/// Consumes the link, compares the verified password state and revokes credentials atomically.
pub async fn reset_password_with_link(
    pool: &PgPool,
    token_hash: &str,
    replacement: &str,
    expected_hash: &str,
    expected_temporary: bool,
) -> Result<Option<i64>> {
    let mut tx = pool.begin().await?;
    // Retain the owning organizations and account before consuming the issuance.
    let user: Option<i64> = sqlx::query_scalar(
        "SELECT user_id FROM email_tokens WHERE purpose='reset' AND token_hash=$1",
    )
    .bind(token_hash)
    .fetch_optional(&mut *tx)
    .await?;
    let Some(user) = user else { return Ok(None) };
    owned_organizations_for_account_change(&mut tx, user).await?;
    if sqlx::query_scalar::<_, i64>("SELECT id FROM users WHERE id=$1 FOR NO KEY UPDATE")
        .bind(user)
        .fetch_optional(&mut *tx)
        .await?
        .is_none()
    {
        return Ok(None);
    }
    let taken: Option<i64> = sqlx::query_scalar(
        "DELETE FROM email_tokens
         WHERE purpose = 'reset' AND token_hash = $1 AND user_id=$2 AND expires_at > clock_timestamp()
           AND EXISTS (SELECT 1 FROM users u
                       WHERE u.id = email_tokens.user_id AND u.email = email_tokens.email
                         AND NOT u.is_banned)
         RETURNING user_id",
    )
    .bind(token_hash)
    .bind(user)
    .fetch_optional(&mut *tx)
    .await?;
    let Some(user_id) = taken else {
        return Ok(None);
    };
    let changed = sqlx::query(
        "UPDATE users SET password_hash=$2,must_change_password=FALSE,updated_at=NOW()
         WHERE id=$1 AND password_hash=$3 AND must_change_password=$4 AND NOT is_banned",
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
