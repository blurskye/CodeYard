//! Temporary remediation forks reuse repository storage placement and checked fork initialization.
use super::{access, lifecycle};
use crate::backend::{
    app::AppState,
    db::{
        self,
        advisories::{self, Refusal},
        models::Repository,
    },
};
use anyhow::Result;
pub async fn private_fork(
    state: &AppState,
    actor: i64,
    repo: &Repository,
    ghsa: &str,
) -> Result<Repository> {
    access::check_admin(&state.pool, repo.id, actor)
        .await
        .map_err(|error| -> anyhow::Error {
            match error {
                access::AccessError::Unavailable => {
                    anyhow::anyhow!("Repository authority is unavailable")
                }
                access::AccessError::NotFound => Refusal::Missing.into(),
                _ => Refusal::Denied.into(),
            }
        })?;
    let advisory = advisories::get(&state.pool, Some(actor), repo.id, ghsa)
        .await?
        .ok_or(Refusal::Missing)?;
    anyhow::ensure!(advisory.state == "draft", Refusal::Conflict);
    if let Some(id) = advisory.private_fork_id {
        return access::check_read(&state.pool, id, Some(actor))
            .await
            .map_err(|error| match error {
                access::AccessError::Unavailable => anyhow::anyhow!("Repository authority is unavailable"),
                _ => Refusal::Missing.into(),
            });
    }
    let fork = lifecycle::provision(
        (&state.pool, state.cluster.as_ref()),
        &state.repos_path,
        db::repositories::creation::RepositoryCreationOwner::personal(actor),
        &format!("security-{}", advisory.ghsa_id.to_ascii_lowercase()),
        Some("Private security advisory remediation"),
        true,
        Some(repo),
    )
    .await?;
    if let Err(error) = advisories::attach_fork(&state.pool, actor, repo.id, ghsa, fork.id).await {
        match db::repositories::records::delete_unattached_advisory_fork(&state.pool, fork.id).await {
            Ok(Some(deleted)) => {
                if let Err(cleanup) = lifecycle::remove_deleted_repository(state.cluster.as_ref(), &deleted).await {
                    tracing::warn!(repo_id=fork.id,%cleanup,"Unattached private advisory fork requires cleanup");
                }
            }
            Ok(None) => tracing::warn!(repo_id=fork.id,"Preserved advisory fork with accepted work or an attachment"),
            Err(cleanup) => tracing::warn!(repo_id=fork.id,%cleanup,"Unattached private advisory fork requires cleanup"),
        }
        return Err(error);
    }
    db::repositories::records::get_repository_by_id(&state.pool, fork.id)
        .await?
        .ok_or_else(|| Refusal::Missing.into())
}
