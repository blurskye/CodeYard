//! Shared organization lifecycle effects for native settings and REST.
use crate::app::AppState;
use codeyard_db_core::queries::organizations::OrganizationOutcome;
use codeyard_db_core::queries::organizations::OrganizationOutcome::Applied;
use codeyard_db_core::DbPool;
use tokio::sync::Semaphore;

static CONVERSION_SLOTS: Semaphore = Semaphore::const_new(16);

/// Acknowledgement follows the authoritative transaction, including for asynchronous REST requests.
pub async fn convert_to_outside(
    pool: &DbPool,
    actor: impl Into<codeyard_db_core::queries::credentials::Actor>,
    org: i64,
    target: i64,
) -> anyhow::Result<OrganizationOutcome<()>> {
    let _permit = CONVERSION_SLOTS
        .try_acquire()
        .map_err(|_| anyhow::anyhow!("Organization conversion admission is full"))?;
    codeyard_db_orgs::organizations::membership::convert_to_outside_collaborator(
        pool, actor, org, target,
    )
    .await
}

/// Deletes repositories first, then the organization. A partial failure leaves the remaining
/// repositories and organization available for a retry; concurrent creation refuses the final delete.
pub async fn delete(
    state: &AppState,
    actor: impl Into<codeyard_db_core::queries::credentials::Actor>,
    org: i64,
) -> anyhow::Result<OrganizationOutcome<()>> {
    let actor = actor.into();
    let repositories =
        match codeyard_db_orgs::organizations::deletion::list_organization_deletion_repositories(
            &state.pool,
            actor.clone(),
            org,
        )
        .await?
        {
            Applied(repositories) => repositories,
            other => return Ok(other.map(|_| ())),
        };
    for repository in repositories {
        let deleted =
            match codeyard_db_orgs::organizations::deletion::delete_organization_repository(
                &state.pool,
                actor.clone(),
                org,
                repository,
            )
            .await?
            {
                Applied(deleted) => deleted,
                other => return Ok(other.map(|_| ())),
            };
        codeyard_repos_core::repositories::lifecycle::remove_deleted_repository(
            state.cluster.as_ref(),
            &deleted,
        )
        .await?;
    }
    codeyard_db_orgs::organizations::deletion::delete_organization(&state.pool, actor, org).await
}
