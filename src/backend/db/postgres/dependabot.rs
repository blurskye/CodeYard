use crate::backend::db::credentials::Actor;
use crate::backend::db::dependabot::OrganizationSecret;
use crate::backend::db::dependabot::OrganizationSecretInput;
use crate::backend::db::dependabot::RepositorySelection;
use crate::backend::db::organizations::OrganizationOutcome;
use codeyard_support::secrets::SecretEncryptor;

use crate::backend::db::secrets::SecretsError;
use codeyard_db_core::models::OrgRole;
use codeyard_db_core::models::Repository;
use sqlx::{PgPool, Postgres, Transaction};

pub(super) async fn authorized(
    pool: &PgPool,
    actor: impl Into<Actor>,
    org: i64,
    write: bool,
) -> Result<Transaction<'_, Postgres>, SecretsError> {
    let actor = actor.into();
    let mut tx = pool.begin().await?;
    match crate::backend::db::postgres::organizations::authority::authority(
        &mut tx,
        actor.user_id,
        org,
    )
    .await
    .map_err(SecretsError::authority)?
    {
        OrganizationOutcome::Applied(OrgRole::Owner | OrgRole::Admin) => {
            if !crate::backend::db::postgres::credentials::organization(
                &mut tx,
                &actor,
                org,
                "organization_administration",
                write,
            )
            .await
            .map_err(SecretsError::authority)?
            {
                return Err(SecretsError::Denied);
            }
            Ok(tx)
        }
        OrganizationOutcome::Missing => Err(SecretsError::NotFound),
        _ => Err(SecretsError::Denied),
    }
}

pub async fn organization_key(
    pool: &PgPool,
    encryptor: &SecretEncryptor,
    actor: impl Into<Actor>,
    org: i64,
) -> Result<(String, crypto_box::SecretKey), SecretsError> {
    let mut tx = authorized(pool, actor, org, false).await?;
    let (encrypted, nonce) = encryptor.generate_sealed_box_key()?;
    sqlx::query("INSERT INTO org_dependabot_keys(org_id,key_id,encrypted_key,nonce) VALUES($1,$2,$3,$4) ON CONFLICT(org_id) DO NOTHING")
        .bind(org).bind(uuid::Uuid::new_v4().simple().to_string()).bind(encrypted).bind(nonce.as_slice()).execute(&mut *tx).await?;
    let (id, encrypted, nonce): (String, Vec<u8>, Vec<u8>) = sqlx::query_as(
        "SELECT key_id,encrypted_key,nonce FROM org_dependabot_keys WHERE org_id=$1",
    )
    .bind(org)
    .fetch_one(&mut *tx)
    .await?;
    let key = encryptor.sealed_box_key(&encrypted, &nonce)?;
    tx.commit().await?;
    Ok((id, key))
}

pub async fn list_organization_secrets(
    pool: &PgPool,
    actor: impl Into<Actor>,
    org: i64,
) -> Result<Vec<OrganizationSecret>, SecretsError> {
    let mut tx = authorized(pool, actor, org, false).await?;
    let rows = sqlx::query_as("SELECT name,visibility,created_at,updated_at FROM org_dependabot_secrets WHERE org_id=$1 ORDER BY name")
        .bind(org).fetch_all(&mut *tx).await?;
    tx.commit().await?;
    Ok(rows)
}

pub(super) async fn check_repositories(
    tx: &mut Transaction<'_, Postgres>,
    org: i64,
    repositories: &[i64],
) -> Result<(), SecretsError> {
    if repositories.len() > 1000 {
        return Err(SecretsError::InvalidRepositorySelection);
    }
    // Transfers lock these same repository rows; take them in id order.
    let mut ids = repositories.to_vec();
    ids.sort_unstable();
    ids.dedup();
    let found: Vec<i64> = sqlx::query_scalar("SELECT id FROM repositories WHERE id=ANY($1) AND owner_kind='org' AND owner_org_id=$2 AND owner_user_id IS NULL ORDER BY id FOR SHARE")
        .bind(&ids).bind(org).fetch_all(&mut **tx).await?;
    if found.len() != ids.len() {
        return Err(SecretsError::InvalidRepositorySelection);
    }
    Ok(())
}

async fn replace_repositories(
    tx: &mut Transaction<'_, Postgres>,
    org: i64,
    name: &str,
    repositories: &[i64],
) -> Result<(), SecretsError> {
    check_repositories(tx, org, repositories).await?;
    sqlx::query("DELETE FROM org_dependabot_secret_repositories WHERE org_id=$1 AND name=$2")
        .bind(org)
        .bind(name)
        .execute(&mut **tx)
        .await?;
    sqlx::query("INSERT INTO org_dependabot_secret_repositories(org_id,name,repo_id) SELECT $1,$2,id FROM UNNEST($3::bigint[]) AS selected(id) ON CONFLICT DO NOTHING")
        .bind(org).bind(name).bind(repositories).execute(&mut **tx).await?;
    Ok(())
}

pub async fn save_organization_secret(
    pool: &PgPool,
    encryptor: &SecretEncryptor,
    actor: impl Into<Actor>,
    org: i64,
    input: &OrganizationSecretInput<'_>,
) -> Result<bool, SecretsError> {
    input.validate()?;
    let mut tx = authorized(pool, actor, org, true).await?;
    let exists: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM org_dependabot_secrets WHERE org_id=$1 AND name=$2)",
    )
    .bind(org)
    .bind(input.name)
    .fetch_one(&mut *tx)
    .await?;
    if !exists && input.value.is_none() {
        return Err(SecretsError::MissingValue);
    }
    if let Some(value) = input.value {
        let (others, stored): (i64, i64) = sqlx::query_as("SELECT COUNT(*),COALESCE(SUM(octet_length(name)+octet_length(encrypted_value)),0)::BIGINT FROM org_dependabot_secrets WHERE org_id=$1 AND name<>$2")
            .bind(org).bind(input.name).fetch_one(&mut *tx).await?;
        crate::backend::db::secrets::ensure_room_for_secret(others, stored, input.name, value)?;
        let (encrypted, nonce) = encryptor.encrypt(value.as_bytes())?;
        sqlx::query("INSERT INTO org_dependabot_secrets(org_id,name,visibility,encrypted_value,nonce) VALUES($1,$2,$3,$4,$5) ON CONFLICT(org_id,name) DO UPDATE SET visibility=EXCLUDED.visibility,encrypted_value=EXCLUDED.encrypted_value,nonce=EXCLUDED.nonce,updated_at=clock_timestamp()")
            .bind(org).bind(input.name).bind(input.visibility).bind(encrypted).bind(nonce.as_slice()).execute(&mut *tx).await?;
    } else {
        sqlx::query("UPDATE org_dependabot_secrets SET visibility=$3,updated_at=clock_timestamp() WHERE org_id=$1 AND name=$2")
            .bind(org).bind(input.name).bind(input.visibility).execute(&mut *tx).await?;
    }
    if let Some(ids) = input.repositories {
        replace_repositories(&mut tx, org, input.name, ids).await?;
    } else if input.visibility != "selected" {
        replace_repositories(&mut tx, org, input.name, &[]).await?;
    }
    tx.commit().await?;
    Ok(!exists)
}

pub async fn delete_organization_secret(
    pool: &PgPool,
    actor: impl Into<Actor>,
    org: i64,
    name: &str,
) -> Result<bool, SecretsError> {
    let mut tx = authorized(pool, actor, org, true).await?;
    let deleted = sqlx::query("DELETE FROM org_dependabot_secrets WHERE org_id=$1 AND name=$2")
        .bind(org)
        .bind(name)
        .execute(&mut *tx)
        .await?
        .rows_affected()
        > 0;
    tx.commit().await?;
    Ok(deleted)
}

pub async fn selected_repositories(
    pool: &PgPool,
    actor: impl Into<Actor>,
    org: i64,
    name: &str,
) -> Result<Vec<Repository>, SecretsError> {
    let mut tx = authorized(pool, actor, org, false).await?;
    if !sqlx::query_scalar::<_, bool>(
        "SELECT EXISTS(SELECT 1 FROM org_dependabot_secrets WHERE org_id=$1 AND name=$2)",
    )
    .bind(org)
    .bind(name)
    .fetch_one(&mut *tx)
    .await?
    {
        return Err(SecretsError::NotFound);
    }
    // Ownership is checked on every read as repositories may have been transferred.
    let rows = sqlx::query_as("SELECT r.* FROM repositories r JOIN org_dependabot_secret_repositories s ON s.repo_id=r.id WHERE s.org_id=$1 AND s.name=$2 AND r.owner_kind='org' AND r.owner_org_id=$1 AND r.owner_user_id IS NULL ORDER BY r.id")
        .bind(org).bind(name).fetch_all(&mut *tx).await?;
    tx.commit().await?;
    Ok(rows)
}

pub async fn select_repositories(
    pool: &PgPool,
    actor: impl Into<Actor>,
    org: i64,
    name: &str,
    selection: RepositorySelection<'_>,
) -> Result<(), SecretsError> {
    let mut tx = authorized(pool, actor, org, true).await?;
    let visibility: String = sqlx::query_scalar(
        "SELECT visibility FROM org_dependabot_secrets WHERE org_id=$1 AND name=$2",
    )
    .bind(org)
    .bind(name)
    .fetch_optional(&mut *tx)
    .await?
    .ok_or(SecretsError::NotFound)?;
    if visibility != "selected" {
        return Err(SecretsError::NotSelected);
    }
    match selection {
        RepositorySelection::Set(ids) => replace_repositories(&mut tx, org, name, ids).await?,
        RepositorySelection::Add(id) => {
            check_repositories(&mut tx, org, &[id]).await?;
            sqlx::query("INSERT INTO org_dependabot_secret_repositories(org_id,name,repo_id) VALUES($1,$2,$3) ON CONFLICT DO NOTHING")
                .bind(org).bind(name).bind(id).execute(&mut *tx).await?;
        }
        RepositorySelection::Remove(id) => {
            check_repositories(&mut tx, org, &[id]).await?;
            sqlx::query("DELETE FROM org_dependabot_secret_repositories WHERE org_id=$1 AND name=$2 AND repo_id=$3")
                .bind(org).bind(name).bind(id).execute(&mut *tx).await?;
        }
    }
    tx.commit().await?;
    Ok(())
}

pub async fn repository_access(
    pool: &PgPool,
    actor: impl Into<Actor>,
    org: i64,
    window: (i64, i64),
) -> Result<(String, Vec<Repository>), SecretsError> {
    let mut tx = authorized(pool, actor, org, false).await?;
    let level =
        sqlx::query_scalar("SELECT default_level FROM org_dependabot_access WHERE org_id=$1")
            .bind(org)
            .fetch_optional(&mut *tx)
            .await?
            .unwrap_or_else(|| "public".to_owned());
    let repositories = sqlx::query_as("SELECT r.* FROM repositories r WHERE r.owner_kind='org' AND r.owner_org_id=$1 AND r.owner_user_id IS NULL AND (NOT r.is_private OR EXISTS(SELECT 1 FROM org_dependabot_access_repositories a WHERE a.org_id=$1 AND a.repo_id=r.id)) ORDER BY r.id LIMIT $2 OFFSET $3")
        .bind(org).bind(window.1).bind(window.0).fetch_all(&mut *tx).await?;
    tx.commit().await?;
    Ok((level, repositories))
}

pub async fn update_repository_access(
    pool: &PgPool,
    actor: impl Into<Actor>,
    org: i64,
    add: &[i64],
    remove: &[i64],
) -> Result<(), SecretsError> {
    crate::backend::db::dependabot::validate_repository_access(add, remove)?;
    let mut tx = authorized(pool, actor, org, true).await?;
    let mut ids: Vec<_> = add.iter().chain(remove).copied().collect();
    ids.sort_unstable();
    ids.dedup();
    check_repositories(&mut tx, org, &ids).await?;
    sqlx::query("INSERT INTO org_dependabot_access_repositories(org_id,repo_id) SELECT $1,id FROM UNNEST($2::bigint[]) AS selected(id) ON CONFLICT DO NOTHING")
        .bind(org).bind(add).execute(&mut *tx).await?;
    sqlx::query(
        "DELETE FROM org_dependabot_access_repositories WHERE org_id=$1 AND repo_id=ANY($2)",
    )
    .bind(org)
    .bind(remove)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(())
}

pub async fn set_repository_access_default(
    pool: &PgPool,
    actor: impl Into<Actor>,
    org: i64,
    level: &str,
) -> Result<(), SecretsError> {
    if !matches!(level, "public" | "internal") {
        return Err(SecretsError::InvalidRepositorySelection);
    }
    let mut tx = authorized(pool, actor, org, true).await?;
    sqlx::query("INSERT INTO org_dependabot_access(org_id,default_level) VALUES($1,$2) ON CONFLICT(org_id) DO UPDATE SET default_level=EXCLUDED.default_level")
        .bind(org).bind(level).execute(&mut *tx).await?;
    tx.commit().await?;
    Ok(())
}
