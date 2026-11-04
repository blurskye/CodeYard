//! Git-derived proof preparation happens outside database transactions. Only the database
//! decides whether a contribution may commit, using the live policy and the proof's source version.
use crate::backend::app::AppState;

use crate::backend::repositories::store::repo_store;
use crate::backend::repositories::store::CommitFilter;
use anyhow::Result;
use std::time::Instant;

static PROOF_WORK: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(4);

pub async fn prepare(
    state: &AppState,
    actor: &codeyard_db_core::models::User,
    repository: &codeyard_db_core::models::Repository,
) -> Result<()> {
    if !codeyard_db_identity::interactions::proof_required(&state.pool, actor.id, repository.id)
        .await?
    {
        return Ok(());
    }
    if !codeyard_identity::repository_access::can_read(&state.pool, Some(actor), repository).await?
        || codeyard_identity::repository_access::user_access(&state.pool, actor, repository)
            .await?
            .can_write()
    {
        return Ok(());
    }
    let source = codeyard_db_identity::interactions::source(&state.pool, repository.id)
        .await?
        .ok_or_else(|| anyhow::anyhow!("Repository changed"))?;
    anyhow::ensure!(
        repository.storage_node_id == source.storage_node_id,
        "Repository placement changed"
    );
    let emails = codeyard_db_identity::interactions::verified_emails(&state.pool, actor.id).await?;
    if codeyard_db_identity::interactions::proof_current(&state.pool, actor.id, &source, &emails)
        .await?
        .is_some()
    {
        return Ok(());
    }
    let _permit = PROOF_WORK.try_acquire()?;
    let store = repo_store(repository, state.cluster.as_ref());
    let deadline = Instant::now() + codeyard_git::COMMIT_WALK_TIMEOUT;
    // The tip comes from the primary, so a stale replica cannot record a proof for an old head.
    let head = store
        .refs_from_primary()
        .await?
        .remove(&format!("refs/heads/{}", source.default_branch));
    let contributed = if let Some(head) = head.filter(|_| !emails.is_empty()) {
        let filter = CommitFilter {
            author_emails: emails.clone(),
            ..CommitFilter::default()
        };
        !store
            .commits_until(&head, 0, 1, &filter, deadline)
            .await?
            .is_empty()
    } else {
        false
    };
    anyhow::ensure!(
        codeyard_db_identity::interactions::save_proof(
            &state.pool,
            actor.id,
            &source,
            &emails,
            contributed
        )
        .await?,
        "Contributor proof source changed"
    );
    Ok(())
}
