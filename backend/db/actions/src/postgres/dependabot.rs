use codeyard_database::models::OrgRole;
use codeyard_database::pool::PgPool;
use codeyard_database::queries::credentials::Actor;
use codeyard_database::queries::organizations::OrganizationOutcome;
use codeyard_database::queries::secrets::SecretsError;
use sqlx::Postgres;
use sqlx::Transaction;
pub async fn authorized(
    pool: &PgPool,
    actor: impl Into<Actor>,
    org: i64,
    write: bool,
) -> Result<Transaction<'_, Postgres>, SecretsError> {
    let actor = actor.into();
    let mut tx = pool.begin().await?;
    match codeyard_database::queries::postgres::organizations::authority::authority(
        &mut tx,
        actor.user_id,
        org,
    )
    .await
    .map_err(SecretsError::authority)?
    {
        OrganizationOutcome::Applied(OrgRole::Owner | OrgRole::Admin) => {
            if !codeyard_database::queries::postgres::credentials::organization(
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
