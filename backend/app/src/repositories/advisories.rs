//! Temporary remediation forks reuse repository storage placement and checked fork initialization.

use crate::app::AppState;
use codeyard_repos_core::context::NodeContext;
use anyhow::Result;
use codeyard_db_core::models::Repository;
use codeyard_db_core::queries::credentials::Actor;
use codeyard_db_repos::repositories::creation::CreationAdmission;
use codeyard_db_repos::repositories::creation::RepositoryCreationOwner;
use codeyard_model::advisories::Refusal;
use std::path::Path;

pub async fn private_fork(
    state: &AppState,
    actor: impl Into<Actor>,
    repo: &Repository,
    ghsa: &str,
) -> Result<Repository> {
    let (pool, context, path, repo, ghsa, authority) = (
        state.pool.clone(),
        state.cluster.clone(),
        state.repos_path.clone(),
        repo.clone(),
        ghsa.to_owned(),
        actor.into(),
    );
    // Attachment and conservative cleanup must finish even if the initiating request goes away.
    tokio::spawn(async move {
        private_fork_owned(&pool, context.as_ref(), &path, authority, &repo, &ghsa).await
    })
    .await
    .map_err(|_| {
        anyhow::anyhow!(
            "Advisory fork creation was interrupted; check the advisory before retrying"
        )
    })?
}

async fn private_fork_owned(
    pool: &codeyard_db_core::DbPool,
    context: Option<&NodeContext>,
    path: &Path,
    authority: Actor,
    repo: &Repository,
    ghsa: &str,
) -> Result<Repository> {
    let actor = authority.user_id;
    codeyard_identity::repository_access::credential_user(pool, &authority, true).await?;
    anyhow::ensure!(
        codeyard_db_security::advisories::can_administer(pool, actor, repo.id).await?,
        Refusal::Denied
    );
    let advisory = codeyard_db_security::advisories::get(pool, Some(actor), repo.id, ghsa)
        .await?
        .ok_or(Refusal::Missing)?;
    anyhow::ensure!(
        matches!(advisory.state.as_str(), "draft" | "triage"),
        Refusal::Conflict
    );
    if let Some(id) = advisory.private_fork_id {
        let fork = codeyard_identity::repository_access::check_read(pool, id, Some(actor))
            .await
            .map_err(|error| match error {
                codeyard_identity::repository_access::AccessError::Unavailable => {
                    anyhow::anyhow!("Repository authority is unavailable")
                }
                _ => Refusal::Missing.into(),
            })?;
        codeyard_identity::repository_access::credential_user(pool, &authority, true).await?;
        anyhow::ensure!(
            codeyard_db_security::advisories::can_administer(pool, actor, repo.id).await?,
            Refusal::Denied
        );
        return Ok(fork);
    }
    let fork = codeyard_repos_core::repositories::lifecycle::provision_with_properties(
        (pool, context),
        path,
        codeyard_repos_core::repositories::lifecycle::CreationInput {
            owner: RepositoryCreationOwner::personal(actor),
            properties: &[],
            admission: CreationAdmission {
                authority: Some(&authority),
                advisory: Some(ghsa),
            },
        },
        &format!("security-{}", advisory.ghsa_id.to_ascii_lowercase()),
        Some("Private security advisory remediation"),
        true,
        Some(repo),
    )
    .await?;
    if let Err(error) =
        codeyard_db_security::advisories::attach_fork(pool, authority, repo.id, ghsa, fork.id).await
    {
        match codeyard_db_repos::repositories::records::delete_unattached_advisory_fork(
            pool, fork.id,
        )
        .await
        {
            Ok(Some(deleted)) => {
                if let Err(cleanup) =
                    codeyard_repos_core::repositories::lifecycle::remove_deleted_repository(context, &deleted)
                        .await
                {
                    tracing::warn!(repo_id=fork.id,%cleanup,"Unattached private advisory fork requires cleanup");
                }
            }
            Ok(None) => tracing::warn!(
                repo_id = fork.id,
                "Preserved advisory fork with accepted work or an attachment"
            ),
            Err(cleanup) => {
                tracing::warn!(repo_id=fork.id,%cleanup,"Unattached private advisory fork requires cleanup")
            }
        }
        return Err(error);
    }
    codeyard_identity::repository_access::check_read(pool, fork.id, Some(actor))
        .await
        .map_err(authority_error)
}

fn authority_error(error: codeyard_identity::repository_access::AccessError) -> anyhow::Error {
    match error {
        codeyard_identity::repository_access::AccessError::Unavailable => {
            anyhow::anyhow!("Repository authority is unavailable")
        }
        codeyard_identity::repository_access::AccessError::NotFound => Refusal::Missing.into(),
        _ => Refusal::Denied.into(),
    }
}
