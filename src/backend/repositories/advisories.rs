//! Temporary remediation forks reuse repository storage placement and checked fork initialization.
use super::{access, lifecycle};
use crate::backend::{
    app::AppState,
    cluster::NodeContext,
    db::{
        self,
        advisories::{self, Refusal},
        models::Repository,
        repositories::creation::{CreationAdmission, RepositoryCreationOwner},
        users::accounts::AccountActor,
    },
};
use anyhow::Result;
use std::path::Path;

pub async fn private_fork(
    state: &AppState,
    actor: impl Into<AccountActor>,
    repo: &Repository,
    ghsa: &str,
) -> Result<Repository> {
    let (pool, context, path, repo, ghsa, authority) = (
        state.pool.clone(), state.cluster.clone(), state.repos_path.clone(),
        repo.clone(), ghsa.to_owned(), actor.into(),
    );
    // Attachment and conservative cleanup must finish even if the initiating request goes away.
    tokio::spawn(async move {
        private_fork_owned(&pool, context.as_ref(), &path, authority, &repo, &ghsa).await
    }).await.map_err(|_| anyhow::anyhow!("Advisory fork creation was interrupted; check the advisory before retrying"))?
}

async fn private_fork_owned(
    pool: &db::DbPool,
    context: Option<&NodeContext>,
    path: &Path,
    authority: AccountActor,
    repo: &Repository,
    ghsa: &str,
) -> Result<Repository> {
    let actor = authority.user_id();
    access::credential_user(pool, &authority, true).await?;
    access::check_admin(pool, repo.id, actor).await.map_err(authority_error)?;
    let advisory = advisories::get(pool, Some(actor), repo.id, ghsa)
        .await?.ok_or(Refusal::Missing)?;
    anyhow::ensure!(advisory.state == "draft", Refusal::Conflict);
    if let Some(id) = advisory.private_fork_id {
        let fork = access::check_read(pool, id, Some(actor)).await.map_err(|error| {
            match error {
                access::AccessError::Unavailable => anyhow::anyhow!("Repository authority is unavailable"),
                _ => Refusal::Missing.into(),
            }
        })?;
        access::credential_user(pool, &authority, true).await?;
        access::check_admin(pool, repo.id, actor).await.map_err(authority_error)?;
        return Ok(fork);
    }
    let fork = lifecycle::provision_with_properties(
        (pool, context), path,
        lifecycle::CreationInput {
            owner: RepositoryCreationOwner::personal(actor),
            properties: &[],
            admission: CreationAdmission { authority: Some(&authority), advisory: Some(ghsa) },
        },
        &format!("security-{}", advisory.ghsa_id.to_ascii_lowercase()),
        Some("Private security advisory remediation"), true, Some(repo),
    ).await?;
    if let Err(error) = advisories::attach_fork(pool, authority, repo.id, ghsa, fork.id).await {
        match db::repositories::records::delete_unattached_advisory_fork(pool, fork.id).await {
            Ok(Some(deleted)) => {
                if let Err(cleanup) = lifecycle::remove_deleted_repository(context, &deleted).await {
                    tracing::warn!(repo_id=fork.id,%cleanup,"Unattached private advisory fork requires cleanup");
                }
            }
            Ok(None) => tracing::warn!(repo_id=fork.id,"Preserved advisory fork with accepted work or an attachment"),
            Err(cleanup) => tracing::warn!(repo_id=fork.id,%cleanup,"Unattached private advisory fork requires cleanup"),
        }
        return Err(error);
    }
    access::check_read(pool, fork.id, Some(actor)).await.map_err(authority_error)
}

fn authority_error(error: access::AccessError) -> anyhow::Error {
    match error {
        access::AccessError::Unavailable => anyhow::anyhow!("Repository authority is unavailable"),
        access::AccessError::NotFound => Refusal::Missing.into(),
        _ => Refusal::Denied.into(),
    }
}
