//! `push` runs for refs the server moves itself: web editor and file API saves,
//! merges, and new branches and tags. Receive hooks share the same filtering policy.
use crate::workflows::should_skip_run_from_message;
use crate::workflows::WorkflowDefinition;
use crate::repositories::store::RepoStore;
use crate::store_types::MAX_COMMIT_PAGE_SIZE;
use anyhow::Context;
use anyhow::Result;
use codeyard_db_core::models::Repository;
use codeyard_git::CommitInfo;
use codeyard_model::workflow_glob::WorkflowMatchBudget;

const ZERO_OID: &str = "0000000000000000000000000000000000000000";

fn is_zero_oid(id: &str) -> bool {
    id == ZERO_OID
}

/// The push document a run's `github.event` reads: the document the push webhook sends, built
/// through the repository store so a repository on another node works too.
pub async fn push_document(
    pool: &codeyard_db_core::DbPool,
    store: &RepoStore,
    repo: &Repository,
    owner: &str,
    actor_id: i64,
    update: &RefUpdate,
) -> Result<serde_json::Value> {
    let sender = codeyard_db_identity::users::accounts::get_user_by_id(pool, actor_id)
        .await?
        .context("push sender is unavailable")?;
    let sender = codeyard_webhooks::payloads::Person::of(&sender);
    if is_zero_oid(&update.new) {
        return Ok(codeyard_webhooks::payloads::push(
            &update.reference,
            update.old.as_deref().unwrap_or(ZERO_OID),
            ZERO_OID,
            &codeyard_webhooks::payloads::PushedCommits {
                commits: Vec::new(),
                head: None,
                forced: false,
            },
            repo,
            owner,
            &sender,
        ));
    }
    let head = store.commit(&update.new).await?;
    let previous = match update.old.as_deref().filter(|old| !is_zero_oid(old)) {
        Some(old) if update.reference.starts_with("refs/tags/") => match store.commit(old).await {
            Ok(commit) => Some(commit.sha),
            Err(error) if error.kind == codeyard_git::error::ErrorKind::NotFound => None,
            Err(error) => return Err(error.into()),
        },
        old => old.map(str::to_owned),
    };
    // Commit data remains bounded independently of the published tag object's identity.
    let (commits, forced) = match previous.as_deref() {
        Some(old) => (
            store.commits_between(old, &head.sha, 20).await?,
            !store.is_ancestor(old, &head.sha).await?,
        ),
        None => (
            store.commits(&head.sha, 0, 20, &Default::default()).await?,
            update.old.as_deref().is_some_and(|old| !is_zero_oid(old)),
        ),
    };
    // Bounded as in the push webhook, so a huge message cannot outgrow the run's claim.
    let pushed = |commit: &CommitInfo| codeyard_webhooks::payloads::PushedCommit {
        id: commit.sha.clone(),
        tree_id: commit.tree_sha.clone(),
        message: codeyard_webhooks::payloads::bounded_text(&commit.message, 4096),
        time: commit.timestamp,
        author_name: codeyard_webhooks::payloads::bounded_text(&commit.author_name, 256),
        author_email: codeyard_webhooks::payloads::bounded_text(&commit.author_email, 256),
        committer_name: codeyard_webhooks::payloads::bounded_text(&commit.committer_name, 256),
        committer_email: codeyard_webhooks::payloads::bounded_text(&commit.committer_email, 256),
        committer_time: commit.committer_timestamp,
        distinct: true,
        files: None,
    };
    let head_commit = codeyard_webhooks::payloads::PushedCommit {
        distinct: commits.iter().any(|commit| commit.sha == head.sha),
        ..pushed(&head)
    };
    let commits = codeyard_webhooks::payloads::PushedCommits {
        commits: commits.iter().map(pushed).collect(),
        head: Some(head_commit),
        forced,
    };
    let old = update.old.clone().unwrap_or_else(|| ZERO_OID.to_owned());
    Ok(codeyard_webhooks::payloads::push(
        &update.reference,
        &old,
        &update.new,
        &commits,
        repo,
        owner,
        &sender,
    ))
}

/// A branch or tag the server has just created or moved.
pub struct RefUpdate {
    /// Full ref name: `refs/heads/main` or `refs/tags/v1`.
    pub reference: String,
    /// Where the ref pointed before; `None` for a new ref.
    pub old: Option<String>,
    pub new: String,
    /// The paths the update changed, when the caller already knows them.
    pub changed_paths: Option<Vec<String>>,
}

/// Like GitHub, a push is skipped when its tip or any commit it introduces asks for it. A new
/// ref checks its tip only, since what it introduces is unbounded.
async fn push_asks_to_skip(
    store: &RepoStore,
    update: &RefUpdate,
    tip_message: Option<&str>,
) -> Result<bool> {
    if tip_message.is_some_and(should_skip_run_from_message) {
        return Ok(true);
    }
    let Some(old) = update.old.as_deref().filter(|old| !is_zero_oid(old)) else {
        return Ok(false);
    };
    match store
        .commits_between(old, &update.new, MAX_COMMIT_PAGE_SIZE)
        .await
    {
        Ok(commits) => Ok(commits
            .iter()
            .any(|commit| should_skip_run_from_message(&commit.message))),
        Err(error)
            if update.reference.starts_with("refs/tags/")
                && error.kind == codeyard_git::error::ErrorKind::NotFound =>
        {
            Ok(false)
        }
        Err(error) => Err(error.into()),
    }
}

/// Ref filters run first. Unavailable path information conservatively admits their candidates.
pub async fn matching_push_workflows(
    store: &RepoStore,
    update: &RefUpdate,
    workflows: Vec<WorkflowDefinition>,
    message: Option<&str>,
    budget: &mut WorkflowMatchBudget,
) -> Result<Vec<WorkflowDefinition>> {
    if update.old.as_deref() == Some(update.new.as_str()) {
        return Ok(Vec::new());
    }
    let mut candidates = Vec::new();
    for workflow in workflows {
        if workflow.matches_event_with_budget("push", &update.reference, budget)? {
            candidates.push(workflow);
        }
    }
    // Deletions have no pushed commit whose message could skip the event.
    if candidates.is_empty()
        || (!is_zero_oid(&update.new) && push_asks_to_skip(store, update, message).await?)
    {
        return Ok(Vec::new());
    }
    let needs_paths =
        !update.reference.starts_with("refs/tags/") && candidates.iter().any(filters_paths);
    let changed = if !needs_paths || is_zero_oid(&update.new) {
        Some(Vec::new())
    } else if update.old.is_some() && update.changed_paths.is_some() {
        update.changed_paths.clone()
    } else {
        let old = update
            .old
            .as_deref()
            .unwrap_or("0000000000000000000000000000000000000000");
        match store.push_changed_paths(old, &update.new).await {
            Ok(paths) => Some(paths),
            Err(error) => {
                tracing::warn!(%error, "push changed paths unavailable; running ref-matched workflows");
                None
            }
        }
    };
    let mut matched = Vec::new();
    for workflow in candidates {
        if changed.is_none() {
            matched.push(workflow);
            continue;
        }
        if workflow.matches_event_with_context_and_budget(
            "push",
            &update.reference,
            changed.as_deref().unwrap_or(&[]),
            message,
            budget,
        )? {
            matched.push(workflow);
        }
    }
    Ok(matched)
}

#[cfg(test)]
#[path = "push_tests.rs"]
mod tests;

fn filters_paths(workflow: &WorkflowDefinition) -> bool {
    workflow
        .triggers
        .push
        .as_ref()
        .is_some_and(|filter| !filter.paths.is_empty() || !filter.paths_ignore.is_empty())
}
