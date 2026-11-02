//! Git-derived proof preparation happens outside database transactions. Only the database
//! decides whether a contribution may commit, using the live policy and the proof's source version.
use crate::backend::{
    app::AppState,
    db::{self, interactions},
    repositories::{
        access,
        store::{repo_store, CommitFilter, ErrorKind},
    },
};
use anyhow::Result;
use std::time::Instant;

static PROOF_WORK: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(4);

pub async fn prepare(
    state: &AppState,
    actor: &db::models::User,
    repository: &db::models::Repository,
) -> Result<()> {
    let Some(restriction) = interactions::effective_restriction(&state.pool, repository.id).await?
    else {
        return Ok(());
    };
    if restriction.limit == "collaborators_only"
        || (restriction.limit == "existing_users"
            && actor.created_at <= chrono::Utc::now() - chrono::Duration::days(1))
    {
        return Ok(());
    }
    if !access::can_read(&state.pool, Some(actor), repository).await?
        || access::user_access(&state.pool, actor, repository)
            .await?
            .can_write()
    {
        return Ok(());
    }
    let source = interactions::source(&state.pool, repository.id)
        .await?
        .ok_or_else(|| anyhow::anyhow!("Repository changed"))?;
    anyhow::ensure!(
        repository.storage_node_id == source.storage_node_id,
        "Repository placement changed"
    );
    let emails = interactions::verified_emails(&state.pool, actor.id).await?;
    if interactions::proof_current(&state.pool, actor.id, &source, &emails)
        .await?
        .is_some()
    {
        return Ok(());
    }
    let _permit = PROOF_WORK.try_acquire()?;
    let store = repo_store(repository, state.cluster.as_ref());
    let deadline = Instant::now() + crate::backend::git::COMMIT_WALK_TIMEOUT;
    // The tip comes from the primary, so a stale replica cannot record a proof for an old head.
    let head = match store
        .commit_from_primary(&format!("refs/heads/{}", source.default_branch))
        .await
    {
        Ok(commit) => Some(commit.sha),
        Err(error) if error.kind == ErrorKind::NotFound => None,
        Err(error) => return Err(error.into()),
    };
    let mut contributed = false;
    if let Some(head) = head {
        'addresses: for email in &emails {
            let filter = CommitFilter {
                author: Some(email.clone()),
                ..CommitFilter::default()
            };
            let mut offset = 0;
            loop {
                let page = store
                    .commits_until(&head, offset, 100, &filter, deadline)
                    .await?;
                if page
                    .iter()
                    .any(|commit| commit.author_email.eq_ignore_ascii_case(email))
                {
                    contributed = true;
                    break 'addresses;
                }
                if page.len() < 100 {
                    break;
                }
                offset += page.len();
            }
        }
    }
    anyhow::ensure!(
        interactions::save_proof(&state.pool, actor.id, &source, &emails, contributed).await?,
        "Contributor proof source changed"
    );
    Ok(())
}
