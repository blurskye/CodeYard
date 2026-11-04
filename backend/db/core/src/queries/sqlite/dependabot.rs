use crate::models::OrgRole;
use crate::queries::credentials::Actor;
use crate::queries::organizations::OrganizationOutcome;
use crate::queries::secrets::SecretsError;
use sqlx::Sqlite;
use sqlx::SqlitePool;
use sqlx::Transaction;
pub async fn authorized(
    pool: &SqlitePool,
    actor: impl Into<Actor>,
    org: i64,
    write: bool,
) -> Result<Transaction<'_, Sqlite>, SecretsError> {
    let actor = actor.into();
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    match crate::queries::sqlite::organizations::authority::authority(&mut tx, actor.user_id, org)
        .await
        .map_err(SecretsError::authority)?
    {
        OrganizationOutcome::Applied(OrgRole::Owner | OrgRole::Admin) => {
            if !crate::queries::sqlite::credentials::organization(
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

pub async fn check_repositories(
    tx: &mut Transaction<'_, Sqlite>,
    org: i64,
    repositories: &[i64],
) -> Result<(), SecretsError> {
    if repositories.len() > 1000 {
        return Err(SecretsError::InvalidRepositorySelection);
    }
    let ids: std::collections::BTreeSet<_> = repositories.iter().copied().collect();
    let found: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM repositories WHERE id IN (SELECT value FROM json_each(?1)) AND owner_kind='org' AND owner_org_id=?2 AND owner_user_id IS NULL")
        .bind(serde_json::to_string(&ids).map_err(|error| SecretsError::authority(error.into()))?).bind(org).fetch_one(&mut **tx).await?;
    if found != ids.len() as i64 {
        return Err(SecretsError::InvalidRepositorySelection);
    }
    Ok(())
}
