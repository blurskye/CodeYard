use codeyard_support::secrets::SecretEncryptor;

use crate::backend::db::private_registries::Configuration;
use crate::backend::db::private_registries::Registry;
use crate::backend::db::private_registries::RegistryError;
use crate::backend::db::secrets::SecretsError;
use serde_json::Value;
use sqlx::PgPool;

async fn prune(
    c: &mut sqlx::PgConnection,
    org: i64,
    configurations: &mut [Configuration],
) -> Result<(), RegistryError> {
    let ids: Vec<i64> = configurations
        .iter()
        .flat_map(|config| config.selected_repository_ids.iter().flatten().copied())
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .collect();
    let mut owned = std::collections::HashSet::<i64>::new();
    for chunk in ids.chunks(500) {
        owned.extend(sqlx::query_scalar::<_, i64>("SELECT id FROM repositories WHERE owner_kind='org' AND owner_org_id=$1 AND owner_user_id IS NULL AND id=ANY($2)")
            .bind(org).bind(chunk).fetch_all(&mut *c).await?);
    }
    for config in configurations {
        if let Some(selected) = config.selected_repository_ids.as_mut() {
            selected.retain(|id| owned.contains(id));
        }
    }
    Ok(())
}

pub async fn list(
    pool: &PgPool,
    actor: i64,
    org: i64,
    name: Option<&str>,
    offset: i64,
    limit: i64,
) -> Result<(i64, Vec<Registry>), RegistryError> {
    if offset < 0 || !(1..=101).contains(&limit) {
        return Err(RegistryError::Invalid("page"));
    }
    let mut tx =
        crate::backend::db::postgres::dependabot::authorized(pool, actor, org, false).await?;
    let total: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM org_private_registries WHERE org_id=$1 AND ($2::text IS NULL OR lower(name)=lower($2))")
        .bind(org).bind(name).fetch_one(&mut *tx).await?;
    let mut registries: Vec<Registry> = sqlx::query_as("SELECT name,configuration,created_at,updated_at FROM org_private_registries WHERE org_id=$1 AND ($2::text IS NULL OR lower(name)=lower($2)) ORDER BY name LIMIT $3 OFFSET $4")
        .bind(org).bind(name).bind(limit).bind(offset).fetch_all(&mut *tx).await?;
    let mut configurations = registries
        .iter()
        .map(|registry| serde_json::from_str::<Configuration>(&registry.configuration))
        .collect::<Result<Vec<_>, _>>()?;
    prune(&mut tx, org, &mut configurations).await?;
    for (registry, config) in registries.iter_mut().zip(configurations) {
        registry.configuration = serde_json::to_string(&config)?;
    }
    tx.commit().await?;
    Ok((total, registries))
}

#[expect(
    clippy::too_many_arguments,
    reason = "Saving binds the actor's credential, the registry's fields and its secret."
)]
pub async fn save(
    pool: &PgPool,
    encryptor: Option<&SecretEncryptor>,
    actor: i64,
    credential: &crate::backend::db::users::accounts::AccountCredential<'_>,
    org: i64,
    name: Option<&str>,
    input: &Value,
    value: Option<&str>,
) -> Result<Registry, RegistryError> {
    let mut tx =
        crate::backend::db::postgres::dependabot::authorized(pool, (actor, credential), org, true)
            .await?;
    let mut stored: Option<String> = match name {
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
    if input
        .as_object()
        .is_some_and(|input| !input.contains_key("selected_repository_ids"))
    {
        if let Some(document) = stored.as_mut() {
            let mut config: Configuration = serde_json::from_str(document)?;
            prune(&mut tx, org, std::slice::from_mut(&mut config)).await?;
            *document = serde_json::to_string(&config)?;
        }
    }
    let config = crate::backend::db::private_registries::configuration(stored.as_deref(), input)?;
    config.validate(value, stored.is_some())?;
    if let Some(ids) = &config.selected_repository_ids {
        crate::backend::db::postgres::dependabot::check_repositories(&mut tx, org, ids).await?;
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

pub async fn delete(
    pool: &PgPool,
    actor: i64,
    credential: &crate::backend::db::users::accounts::AccountCredential<'_>,
    org: i64,
    name: &str,
) -> Result<(), RegistryError> {
    let mut tx =
        crate::backend::db::postgres::dependabot::authorized(pool, (actor, credential), org, true)
            .await?;
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
