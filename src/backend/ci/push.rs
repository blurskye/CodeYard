//! `push` runs for refs the server moves itself: web editor and file API saves,
//! merges, and new branches and tags. Receive hooks share the same filtering policy.
use super::{
    infer_pipeline_from_workflow_path, list_workflows_for_ref,
    workflows::{should_skip_run_from_message, WorkflowMatchBudget},
    CiRunRequest, CiRuns, WorkflowDefinition,
};
use crate::backend::db::models::Repository;
use crate::backend::git::CommitInfo;
use crate::backend::repositories::store::{repo_store, RepoStore, MAX_COMMIT_PAGE_SIZE};
use crate::backend::webhooks::payloads;
use anyhow::{Context, Result};

const ZERO_OID: &str = "0000000000000000000000000000000000000000";

fn is_zero_oid(id: &str) -> bool {
    id == ZERO_OID
}

/// The push document a run's `github.event` reads: the document the push webhook sends, built
/// through the repository store so a repository on another node works too.
pub(crate) async fn push_document(
    pool: &crate::backend::db::DbPool,
    store: &RepoStore,
    repo: &Repository,
    owner: &str,
    actor_id: i64,
    update: &RefUpdate,
) -> Result<serde_json::Value> {
    let sender = crate::backend::db::users::accounts::get_user_by_id(pool, actor_id)
        .await?
        .context("push sender is unavailable")?;
    let sender = payloads::Person::of(&sender);
    if is_zero_oid(&update.new) {
        return Ok(payloads::push(
            &update.reference,
            update.old.as_deref().unwrap_or(ZERO_OID),
            ZERO_OID,
            &payloads::PushedCommits {
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
            Err(error)
                if error.kind == crate::backend::repositories::store::ErrorKind::NotFound =>
            {
                None
            }
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
    let pushed = |commit: &CommitInfo| payloads::PushedCommit {
        id: commit.sha.clone(),
        tree_id: commit.tree_sha.clone(),
        message: payloads::bounded_text(&commit.message, 4096),
        time: commit.timestamp,
        author_name: payloads::bounded_text(&commit.author_name, 256),
        author_email: payloads::bounded_text(&commit.author_email, 256),
        committer_name: payloads::bounded_text(&commit.committer_name, 256),
        committer_email: payloads::bounded_text(&commit.committer_email, 256),
        committer_time: commit.committer_timestamp,
        distinct: true,
        files: None,
    };
    let head_commit = payloads::PushedCommit {
        distinct: commits.iter().any(|commit| commit.sha == head.sha),
        ..pushed(&head)
    };
    let commits = payloads::PushedCommits {
        commits: commits.iter().map(pushed).collect(),
        head: Some(head_commit),
        forced,
    };
    let old = update.old.clone().unwrap_or_else(|| ZERO_OID.to_owned());
    Ok(payloads::push(
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

impl CiRuns {
    /// Queues the workflows at `update.new` that listen to `push` for this ref, as
    /// `actor_id`. The caller has checked write access and admission checks it again.
    /// The ref has already moved, so a run that cannot be queued is logged and skipped.
    pub async fn queue_push_runs(
        &self,
        repo: &Repository,
        owner: &str,
        actor_id: i64,
        update: RefUpdate,
    ) {
        if update.old.as_deref() == Some(update.new.as_str()) {
            return;
        }
        match self.actor_may_discover_workflows(repo.id, actor_id).await {
            Ok(true) => {}
            Ok(false) => return,
            Err(error) => {
                tracing::warn!(repo_id = repo.id, %error, "push runs skipped: CI authority unavailable");
                return;
            }
        }
        let store = repo_store(repo, self.cluster.as_ref());
        let commit = match store.commit(&update.new).await {
            Ok(commit) => commit,
            Err(error) => {
                tracing::warn!(repo_id = repo.id, %error, "push runs skipped: commit unreadable");
                return;
            }
        };
        if let Err(error) = crate::backend::db::ci::record_ci_accepted_push(
            &self.pool,
            repo.id,
            actor_id,
            &update.reference,
            &commit.sha,
        )
        .await
        {
            tracing::warn!(repo_id = repo.id, %error, "push runs skipped: accepted update could not be recorded");
            return;
        }
        let workflows = match list_workflows_for_ref(&store, &commit.sha).await {
            Ok(workflows) => workflows,
            Err(error) => {
                tracing::warn!(repo_id = repo.id, %error, "push runs skipped: workflows unreadable");
                return;
            }
        };
        let mut budget = WorkflowMatchBudget::new();
        let matched = match matching_push_workflows(
            &store,
            &update,
            workflows,
            Some(&commit.message),
            &mut budget,
        )
        .await
        {
            Ok(matched) => matched,
            Err(error) => {
                tracing::warn!(repo_id = repo.id, %error, "push workflow matching failed");
                return;
            }
        };
        if matched.is_empty() {
            return;
        }
        // A run's github.event retains the published object ID, including annotated tags.
        let event = match push_document(&self.pool, &store, repo, owner, actor_id, &update).await {
            Ok(event) => event,
            Err(error) => {
                tracing::warn!(repo_id = repo.id, %error, "push runs skipped: push document unavailable");
                return;
            }
        };
        for workflow in matched {
            let pipeline = infer_pipeline_from_workflow_path(&workflow.path);
            let result = self
                .trigger_event_run(
                    CiRunRequest {
                        repo_id: repo.id,
                        repo_owner: owner.to_owned(),
                        repo_name: repo.name.clone(),
                        branch: update.reference.clone(),
                        head_sha: Some(commit.sha.clone()),
                        event: "push".to_owned(),
                        pull_request_id: None,
                        pipeline,
                        workflow_paths: vec![workflow.path.clone()],
                        environment: None,
                        inputs: None,
                        triggered_by_id: actor_id,
                        manual: false,
                    },
                    crate::backend::db::models::AccessLevel::Owner,
                    event.clone(),
                    None,
                )
                .await;
            if let Err(error) = result {
                tracing::debug!(repo_id = repo.id, workflow = %workflow.path, %error, "push run not queued");
            }
        }
    }
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
                && error.kind == crate::backend::repositories::store::ErrorKind::NotFound =>
        {
            Ok(false)
        }
        Err(error) => Err(error.into()),
    }
}

/// Ref filters run first. Unavailable path information conservatively admits their candidates.
pub(crate) async fn matching_push_workflows(
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

/// The branch name a run is stored and shown with. A push arrives as the full ref
/// (`refs/heads/main`) and is stored as `main`, like a manual run. A tag keeps its
/// full ref so it cannot be mistaken for a branch of the same name. Branches whose
/// short names start with `refs/` also keep their full ref to preserve their kind.
pub fn branch_name(reference: &str) -> &str {
    match reference.strip_prefix("refs/heads/") {
        // Keep the kind of branches whose short name looks like a canonical ref.
        Some(name) if !name.starts_with("refs/") => name,
        _ => reference,
    }
}
