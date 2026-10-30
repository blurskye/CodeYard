use crate::backend::db::{
    oidc::{Inclusion, Refusal, Scope, Settings, TokenSettings},
    organizations::{self, properties::{DefinitionRow, PropertyValue}, OrganizationOutcome},
};
use anyhow::{ensure, Result};
use sqlx::{PgPool, Postgres, Transaction};
use std::collections::BTreeMap;

async fn authorize(tx: &mut Transaction<'_, Postgres>, actor: impl Into<crate::backend::db::credentials::Actor>, scope: Scope) -> Result<()> {
    let credential = actor.into();
    let actor = credential.user_id;
    let allowed = match scope {
        Scope::Repository(repo) => {
            super::discussions::lock_admin_authority(tx, actor, repo).await?
        }
        Scope::Organization(org) => {
            matches!(super::organizations::authority::authority(tx, actor, org).await?, OrganizationOutcome::Applied(role) if organizations::administers(role))
        }
    };
    ensure!(allowed, Refusal::Denied);
    if let Some(token) = credential.token_id() {
        let (repo, org, permission) = match scope {
            Scope::Repository(repo) => (Some(repo), None, "administration"),
            Scope::Organization(org) => (None, Some(org), "organization_administration"),
        };
        ensure!(super::users::tokens::permits_write(tx, actor, token, repo, org, permission).await?, Refusal::Denied);
    }
    Ok(())
}

async fn read_on(tx: &mut Transaction<'_, Postgres>, scope: Scope) -> Result<Settings> {
    let (table, column, id) = scope.storage();
    let row: Option<(bool, Option<String>)> = sqlx::query_as(&format!(
        "SELECT use_default,claim_keys FROM {table} WHERE {column}=$1"
    ))
    .bind(id)
    .fetch_optional(&mut **tx)
    .await?;
    let (use_default, keys) = row.unwrap_or((matches!(scope, Scope::Repository(_)), None));
    Ok(Settings {
        use_default,
        include_claim_keys: keys.map(|keys| serde_json::from_str(&keys)).transpose()?,
    })
}

pub async fn get(pool: &PgPool, actor: i64, scope: Scope) -> Result<Settings> {
    let mut tx = pool.begin().await?;
    authorize(&mut tx, actor, scope).await?;
    let result = read_on(&mut tx, scope).await?;
    tx.commit().await?;
    Ok(result)
}

pub async fn save(pool: &PgPool, actor: impl Into<crate::backend::db::credentials::Actor>, scope: Scope, settings: &Settings) -> Result<()> {
    settings.validate()?;
    let mut tx = pool.begin().await?;
    authorize(&mut tx, actor, scope).await?;
    let (table, column, id) = scope.storage();
    let keys = if settings.use_default {
        None
    } else {
        settings
            .include_claim_keys
            .as_ref()
            .map(serde_json::to_string)
            .transpose()?
    };
    sqlx::query(&format!("INSERT INTO {table}({column},use_default,claim_keys) VALUES($1,$2,$3) ON CONFLICT({column}) DO UPDATE SET use_default=excluded.use_default,claim_keys=excluded.claim_keys"))
        .bind(id).bind(settings.use_default).bind(keys).execute(&mut *tx).await?;
    tx.commit().await?;
    Ok(())
}

pub async fn inclusions(pool: &PgPool, actor: i64, org: i64) -> Result<Vec<Inclusion>> {
    let mut tx = pool.begin().await?;
    authorize(&mut tx, actor, Scope::Organization(org)).await?;
    let names: Vec<String> = sqlx::query_scalar("SELECT p.property_name FROM actions_oidc_properties o JOIN organization_repository_properties p ON p.id=o.definition_id WHERE p.org_id=$1 ORDER BY p.property_name")
        .bind(org).fetch_all(&mut *tx).await?;
    tx.commit().await?;
    Ok(names
        .into_iter()
        .map(|custom_property_name| Inclusion {
            custom_property_name,
            inclusion_source: "organization",
        })
        .collect())
}

pub async fn change_inclusion(
    pool: &PgPool,
    actor: impl Into<crate::backend::db::credentials::Actor>,
    org: i64,
    name: &str,
    remove: bool,
) -> Result<()> {
    let mut tx = pool.begin().await?;
    authorize(&mut tx, actor, Scope::Organization(org)).await?;
    let id: Option<i64> = sqlx::query_scalar(crate::backend::db::oidc::INCLUSION_PROPERTY_SQL)
        .bind(org).bind(name).bind(remove).fetch_optional(&mut *tx).await?;
    let id = id.ok_or(if remove {
        Refusal::Missing
    } else {
        Refusal::Invalid
    })?;
    let changed = if remove {
        sqlx::query("DELETE FROM actions_oidc_properties WHERE definition_id=$1")
            .bind(id)
            .execute(&mut *tx)
            .await?
            .rows_affected()
    } else {
        sqlx::query(
            "INSERT INTO actions_oidc_properties(definition_id) VALUES($1) ON CONFLICT DO NOTHING",
        )
        .bind(id)
        .execute(&mut *tx)
        .await?
        .rows_affected()
    };
    ensure!(
        changed == 1,
        if remove {
            Refusal::Missing
        } else {
            Refusal::Duplicate
        }
    );
    tx.commit().await?;
    Ok(())
}

pub async fn token_settings(pool: &PgPool, repo: i64) -> Result<TokenSettings> {
    let mut tx = pool.begin().await?;
    sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ READ ONLY")
        .execute(&mut *tx)
        .await?;
    let org: Option<i64> = sqlx::query_scalar("SELECT owner_org_id FROM repositories WHERE id=$1")
        .bind(repo)
        .fetch_one(&mut *tx)
        .await?;
    let own = read_on(&mut tx, Scope::Repository(repo)).await?;
    let include_claim_keys = if own.use_default {
        None
    } else if own.include_claim_keys.is_some() {
        own.include_claim_keys
    } else if let Some(org) = org {
        read_on(&mut tx, Scope::Organization(org))
            .await?
            .include_claim_keys
    } else {
        None
    };
    let mut properties = BTreeMap::new();
    if let Some(org) = org {
        let definitions = sqlx::query_as::<_, DefinitionRow>(crate::backend::db::oidc::INCLUDED_DEFINITIONS_SQL)
            .bind(org).fetch_all(&mut *tx).await?.into_iter().map(DefinitionRow::decode).collect::<Result<Vec<_>>>()?;
        for property in
            super::organizations::properties::values_on(&mut tx, repo, &definitions).await?
        {
            if let Some(value) = property.value.filter(PropertyValue::present) {
                let value = match value {
                    PropertyValue::String(s) => s,
                    PropertyValue::Strings(v) => serde_json::to_string(&v)?,
                };
                properties.insert(format!("repo_property_{}", property.property_name), value);
            }
        }
    }
    tx.commit().await?;
    Ok(TokenSettings {
        include_claim_keys,
        properties,
    })
}
