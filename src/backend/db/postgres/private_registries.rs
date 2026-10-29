use crate::backend::{
    crypto::SecretEncryptor,
    db::{
        private_registries::{self, Configuration, Registry, RegistryError},
        secrets::SecretsError,
    },
};
use serde_json::Value;
use sqlx::PgPool;

pub async fn list(pool: &PgPool, actor: i64, org: i64) -> Result<Vec<Registry>, RegistryError> {
    let mut tx = super::dependabot::authorized(pool, actor, org, false).await?;
    let mut registries: Vec<Registry> = sqlx::query_as("SELECT name,configuration,created_at,updated_at FROM org_private_registries WHERE org_id=$1 ORDER BY name")
        .bind(org).fetch_all(&mut *tx).await?;
    let ids: Vec<i64> = sqlx::query_scalar("SELECT id FROM repositories WHERE owner_kind='org' AND owner_org_id=$1 AND owner_user_id IS NULL")
        .bind(org).fetch_all(&mut *tx).await?;
    for registry in &mut registries {
        let mut config: Configuration = serde_json::from_str(&registry.configuration)?;
        if let Some(selected) = config.selected_repository_ids.as_mut() {
            selected.retain(|id| ids.contains(id));
        }
        registry.configuration = serde_json::to_string(&config)?;
    }
    tx.commit().await?;
    Ok(registries)
}

pub async fn save(
    pool: &PgPool,
    encryptor: Option<&SecretEncryptor>,
    actor: i64,
    org: i64,
    name: Option<&str>,
    input: &Value,
    value: Option<&str>,
) -> Result<Registry, RegistryError> {
    let mut tx = super::dependabot::authorized(pool, actor, org, true).await?;
    let stored: Option<String> = match name {
        Some(name) => Some(
            sqlx::query_scalar(
                "SELECT configuration FROM org_private_registries WHERE org_id=$1 AND name=$2",
            )
            .bind(org)
            .bind(name)
            .fetch_optional(&mut *tx)
            .await?
            .ok_or(RegistryError::Missing)?,
        ),
        None => None,
    };
    let config = private_registries::configuration(stored.as_deref(), input)?;
    config.validate(value, stored.is_some())?;
    if let Some(ids) = &config.selected_repository_ids {
        super::dependabot::check_repositories(&mut tx, org, ids).await?;
    }
    let generated = format!(
        "{}_{}",
        config.registry_type.to_ascii_uppercase(),
        uuid::Uuid::new_v4()
            .simple()
            .to_string()
            .to_ascii_uppercase()
    );
    let name = name.unwrap_or(&generated);
    if stored.is_none() {
        let count: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM org_private_registries WHERE org_id=$1")
                .bind(org)
                .fetch_one(&mut *tx)
                .await?;
        if count >= 100 {
            return Err(RegistryError::Invalid("configuration"));
        }
    }
    let configuration = serde_json::to_string(&config)?;
    if let Some(value) = value {
        let encryptor = encryptor.ok_or(RegistryError::Unavailable)?;
        let (encrypted, nonce) = encryptor
            .encrypt(value.as_bytes())
            .map_err(SecretsError::from)?;
        sqlx::query("INSERT INTO org_private_registries(org_id,name,configuration,encrypted_value,nonce) VALUES($1,$2,$3,$4,$5) ON CONFLICT(org_id,name) DO UPDATE SET configuration=EXCLUDED.configuration,encrypted_value=EXCLUDED.encrypted_value,nonce=EXCLUDED.nonce,updated_at=clock_timestamp()")
            .bind(org).bind(name).bind(configuration).bind(encrypted).bind(nonce.as_slice()).execute(&mut *tx).await?;
    } else {
        sqlx::query("INSERT INTO org_private_registries(org_id,name,configuration) VALUES($1,$2,$3) ON CONFLICT(org_id,name) DO UPDATE SET configuration=EXCLUDED.configuration,updated_at=clock_timestamp()")
            .bind(org).bind(name).bind(configuration).execute(&mut *tx).await?;
    }
    let registry = sqlx::query_as("SELECT name,configuration,created_at,updated_at FROM org_private_registries WHERE org_id=$1 AND name=$2")
        .bind(org).bind(name).fetch_one(&mut *tx).await?;
    tx.commit().await?;
    Ok(registry)
}

pub async fn delete(pool: &PgPool, actor: i64, org: i64, name: &str) -> Result<(), RegistryError> {
    let mut tx = super::dependabot::authorized(pool, actor, org, true).await?;
    let deleted = sqlx::query("DELETE FROM org_private_registries WHERE org_id=$1 AND name=$2")
        .bind(org)
        .bind(name)
        .execute(&mut *tx)
        .await?
        .rows_affected();
    if deleted == 0 {
        return Err(RegistryError::Missing);
    }
    tx.commit().await?;
    Ok(())
}
