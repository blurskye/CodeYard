use crate::backend::db::deploy_keys::NewDeployKey;
use crate::backend::db::models::DeployKey;
use crate::backend::db::users::ssh_keys::SshKeyWriteRefusal;
use anyhow::Result;
use sqlx::PgPool;

const COLUMNS: &str = "id, repo_id, name AS title, fingerprint, public_key, NOT read_only AS can_write, created_by_id, last_used_at, created_at";

pub async fn list_deploy_keys(pool: &PgPool, repo_id: i64) -> Result<Vec<DeployKey>> {
    let sql =
        format!("SELECT {COLUMNS} FROM deploy_keys WHERE repo_id = $1 ORDER BY created_at, id");
    Ok(sqlx::query_as(&sql).bind(repo_id).fetch_all(pool).await?)
}

pub async fn list_deploy_keys_as(
    pool: &PgPool, actor: i64, repo: i64, credential: impl Into<crate::backend::db::deploy_keys::Credential>,
) -> Result<Vec<DeployKey>> {
    let (session, token) = match credential.into() {
        crate::backend::db::deploy_keys::Credential::Session(session) => (Some(session), None),
        crate::backend::db::deploy_keys::Credential::Token(token) => (None, Some(token)),
    };
    let mut tx = pool.begin().await?;
    sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ, READ ONLY").execute(&mut *tx).await?;
    let level = super::repositories::access::USER_REPO_ACCESS_SQL;
    let allowed: bool = sqlx::query_scalar(&format!(r#"
        SELECT EXISTS(
            SELECT 1 FROM repositories r JOIN users u ON u.id=$1
            WHERE r.id=$2 AND NOT r.is_gist AND NOT u.is_banned AND NOT u.must_change_password
              AND ((r.owner_kind='user' AND u.is_admin) OR ({level})>=5)
              AND (
                EXISTS(SELECT 1 FROM sessions s WHERE s.id=$3 AND s.user_id=u.id AND s.expires_at>clock_timestamp())
                OR EXISTS(SELECT 1 FROM personal_access_tokens t WHERE t.id=$4 AND t.user_id=u.id
                    AND t.scope IN ('read','write') AND (t.expires_at IS NULL OR t.expires_at>clock_timestamp()))
              )
        )"#))
        .bind(actor).bind(repo).bind(session).bind(token).fetch_one(&mut *tx).await?;
    anyhow::ensure!(allowed, crate::backend::repositories::deploy_keys::DeployKeyError::Denied);
    let keys = sqlx::query_as(&format!("SELECT {COLUMNS} FROM deploy_keys WHERE repo_id=$1 ORDER BY created_at,id"))
        .bind(repo).fetch_all(&mut *tx).await?;
    tx.commit().await?;
    Ok(keys)
}

pub async fn get_deploy_key(pool: &PgPool, key_id: i64) -> Result<Option<DeployKey>> {
    let sql = format!("SELECT {COLUMNS} FROM deploy_keys WHERE id = $1");
    Ok(sqlx::query_as(&sql)
        .bind(key_id)
        .fetch_optional(pool)
        .await?)
}

pub async fn usable_deploy_key(pool: &PgPool, key_id: i64) -> Result<Option<DeployKey>> {
    let sql=format!("SELECT {COLUMNS} FROM deploy_keys WHERE id=$1 AND NOT EXISTS(SELECT 1 FROM repositories r JOIN organization_settings s ON s.org_id=r.owner_org_id WHERE r.id=deploy_keys.repo_id AND r.owner_kind='org' AND r.owner_user_id IS NULL AND NOT s.deploy_keys_enabled_for_repositories) AND NOT EXISTS(SELECT 1 FROM ssh_keys k WHERE k.fingerprint=deploy_keys.fingerprint)");
    Ok(sqlx::query_as(&sql)
        .bind(key_id)
        .fetch_optional(pool)
        .await?)
}
pub async fn get_deploy_key_by_fingerprint(
    pool: &PgPool,
    fingerprint: &str,
) -> Result<Option<DeployKey>> {
    let sql = format!("SELECT {COLUMNS} FROM deploy_keys WHERE fingerprint = $1");
    Ok(sqlx::query_as(&sql)
        .bind(fingerprint)
        .fetch_optional(pool)
        .await?)
}

async fn require_admin(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    actor: i64,
    repo: i64,
    credential: crate::backend::db::deploy_keys::Credential,
) -> Result<()> {
    use super::repositories::access::{lock_user_repo_access_in_transaction, RepositoryRowLock};
    let access =
        lock_user_repo_access_in_transaction(tx, actor, repo, RepositoryRowLock::Shared, true)
            .await?;
    let locked: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM repository_migration_locks WHERE repo_id=$1)",
    )
    .bind(repo)
    .fetch_one(&mut **tx)
    .await?;
    let allowed = !locked
        && access.is_some_and(|access| {
            access.access.can_admin() || (!access.organization && access.is_admin)
        });
    let live = match credential {
        crate::backend::db::deploy_keys::Credential::Session(session) => super::users::transitions::live_session(tx, actor, &session).await?,
        crate::backend::db::deploy_keys::Credential::Token(id) => sqlx::query_scalar::<_, i64>("SELECT id FROM personal_access_tokens WHERE id=$1 AND user_id=$2 AND scope='write' AND (expires_at IS NULL OR expires_at>NOW()) FOR SHARE").bind(id).bind(actor).fetch_optional(&mut **tx).await?.is_some(),
    };
    anyhow::ensure!(
        allowed && live,
        crate::backend::repositories::deploy_keys::DeployKeyError::Denied
    );
    Ok(())
}

pub async fn insert_deploy_key(
    pool: &PgPool,
    key: &NewDeployKey,
    credential: impl Into<crate::backend::db::deploy_keys::Credential>,
) -> Result<DeployKey> {
    let mut tx = pool.begin().await?;
    require_admin(&mut tx, key.created_by_id, key.repo_id, credential.into()).await?;
    let enabled:bool=sqlx::query_scalar("SELECT NOT EXISTS(SELECT 1 FROM repositories r JOIN organization_settings s ON s.org_id=r.owner_org_id WHERE r.id=$1 AND r.owner_kind='org' AND r.owner_user_id IS NULL AND NOT s.deploy_keys_enabled_for_repositories)").bind(key.repo_id).fetch_one(&mut *tx).await?;
    anyhow::ensure!(
        enabled,
        crate::backend::repositories::deploy_keys::DeployKeyError::Denied
    );

    // The same advisory lock as account-key enrollment, so one fingerprint is checked once.
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 37000003))")
        .bind(&key.fingerprint)
        .execute(&mut *tx)
        .await?;
    let used: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM ssh_keys WHERE fingerprint = $1) OR EXISTS(SELECT 1 FROM deploy_keys WHERE fingerprint = $1)",
    )
    .bind(&key.fingerprint)
    .fetch_one(&mut *tx)
    .await?;
    anyhow::ensure!(!used, SshKeyWriteRefusal::AlreadyUsed);
    let sql = format!(
        "INSERT INTO deploy_keys (repo_id, name, fingerprint, public_key, read_only, created_by_id) VALUES ($1, $2, $3, $4, $5, $6) RETURNING {COLUMNS}"
    );
    let row = sqlx::query_as::<_, DeployKey>(&sql)
        .bind(key.repo_id)
        .bind(&key.title)
        .bind(&key.fingerprint)
        .bind(&key.public_key)
        .bind(!key.can_write)
        .bind(key.created_by_id)
        .fetch_one(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(row)
}

pub async fn delete_deploy_key(
    pool: &PgPool,
    repo_id: i64,
    key_id: i64,
    actor: i64,
    credential: impl Into<crate::backend::db::deploy_keys::Credential>,
) -> Result<bool> {
    let mut tx = pool.begin().await?;
    require_admin(&mut tx, actor, repo_id, credential.into()).await?;
    let result = sqlx::query("DELETE FROM deploy_keys WHERE id = $1 AND repo_id = $2")
        .bind(key_id)
        .bind(repo_id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(result.rows_affected() > 0)
}

/// Records a use. It runs outside any transaction, and its caller ignores a failure.
pub async fn touch_deploy_key(pool: &PgPool, key_id: i64) -> Result<()> {
    sqlx::query("UPDATE deploy_keys SET last_used_at = NOW() WHERE id = $1")
        .bind(key_id)
        .execute(pool)
        .await?;
    Ok(())
}
