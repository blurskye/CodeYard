//! Temporary remediation forks reuse repository storage placement and checked fork initialization.
use super::lifecycle;
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
    anyhow::ensure!(advisories::can_administer(&state.pool, actor, repo.id).await?, Refusal::Denied);
    let advisory = advisories::get(&state.pool, Some(actor), repo.id, ghsa)
        .await?
        .ok_or(Refusal::Missing)?;
    anyhow::ensure!(advisory.state == "draft", Refusal::Conflict);
    if let Some(id) = advisory.private_fork_id {
        return db::repositories::records::get_repository_by_id(&state.pool, id)
            .await?
            .ok_or_else(|| Refusal::Missing.into());
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
        if let Err(cleanup) =
            lifecycle::delete(&state.pool, state.cluster.as_ref(), actor, fork.id).await
        {
            tracing::warn!(repo_id=fork.id,%cleanup,"Unattached private advisory fork requires cleanup");
        }
        return Err(error);
    }
    db::repositories::records::get_repository_by_id(&state.pool, fork.id)
        .await?
        .ok_or_else(|| Refusal::Missing.into())
}
