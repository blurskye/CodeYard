//! Receive authorization and follow-up after Git accepts immutable updates.
use super::broker::{Gateway, Operation};
use super::validation::{self, RefUpdate, ZERO_SHA};
use super::ReceiveCredential;
use crate::backend::ci::workflows::{parse_push_workflow, WorkflowMatchBudget};
use crate::backend::webhooks::ref_changes::{self, RefMove};
use crate::backend::{
    ci::{self, CiRunRequest},
    db::{
        self,
        models::{AccessLevel, Repository},
    },
    repositories::{
        languages,
        protection::{
            check_branch_write_with, load_rules, BranchWrite, History, Refusal, WriteVia,
        },
        pull_requests, reads,
        store::{ensure_ref_room, repo_store, ErrorKind, RepoError},
    },
};
use anyhow::{ensure, Context, Result};
use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

/// The directory where Git holds the objects of a push it has not accepted yet. The helper
/// sends it, so it counts only when it really is one of this repository's `incoming-*`
/// (older Git) or `tmp_objdir-incoming-*` directories.
async fn quarantine_directory(repository: &Path, claimed: &str) -> Option<PathBuf> {
    let claimed = Path::new(claimed);
    if !claimed.is_absolute() {
        return None;
    }
    let directory = tokio::fs::canonicalize(claimed).await.ok()?;
    let is_incoming = directory
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| {
            name.starts_with("incoming-") || name.starts_with("tmp_objdir-incoming-")
        });
    (is_incoming && directory.parent() == Some(repository.join("objects").as_path()))
        .then_some(directory)
}

impl Gateway {
    pub(super) async fn hook(
        &self,
        operation: &Arc<Operation>,
        post: bool,
        updates: &[RefUpdate],
        quarantine: Option<&str>,
    ) -> Result<()> {
        validation::validate_updates(updates)?;
        #[cfg(test)]
        super::broker::policy_tests::before_authorize(operation, post).await;
        let repo = db::repositories::records::get_repository_by_id(&self.pool, operation.repo_id)
            .await?
            .context("repository unavailable")?;
        let path = self.bare_path(&repo).await?;
        ensure!(path == operation.path, "repository location changed");
        let (identity, _) = self.authorize(operation.credential, &repo, true).await?;
        if !post {
            let quarantine = match quarantine {
                Some(claimed) => quarantine_directory(&path, claimed).await,
                None => None,
            };
            let history = History::Push {
                repository: &path,
                quarantine: quarantine.as_deref(),
            };
            // Every branch of the push is judged against one read of the rules and one budget.
            let branch_updates: Vec<(&RefUpdate, &str)> = updates
                .iter()
                .filter_map(|update| {
                    let branch = update.reference.strip_prefix("refs/heads/")?;
                    Some((update, branch))
                })
                .collect();
            if !branch_updates.is_empty() {
                let rules = load_rules(&self.pool, &repo)
                    .await
                    .map_err(|_| anyhow::anyhow!("branch protection could not be checked"))?;
                let mut protection_budget = WorkflowMatchBudget::new();
                for (update, branch) in branch_updates {
                    let write = BranchWrite {
                        branch,
                        old: (update.old != ZERO_SHA).then_some(update.old.as_str()),
                        new: (update.new != ZERO_SHA).then_some(update.new.as_str()),
                        via: if matches!(operation.credential, ReceiveCredential::DeployKey(_)) {
                            WriteVia::DeployKey
                        } else {
                            WriteVia::Direct
                        },
                    };
                    check_branch_write_with(
                        &self.pool,
                        &rules,
                        &repo,
                        &identity,
                        &write,
                        &history,
                        &mut protection_budget,
                    )
                    .await
                    .map_err(|refusal| match refusal {
                        Refusal::Blocked(reason) => {
                            anyhow::Error::from(RepoError::new(ErrorKind::Invalid, reason))
                        }
                        Refusal::Unavailable => {
                            anyhow::anyhow!("branch protection could not be checked")
                        }
                    })?;
                }
            }
            // The stored default can lag behind Git, so the default that Git advertises is
            // protected as well.
            let advertised = crate::backend::git::repository::default_branch(&path).await?;
            ensure!(
                !updates.iter().any(|update| update.new == ZERO_SHA
                    && update
                        .reference
                        .strip_prefix("refs/heads/")
                        .is_some_and(|branch| {
                            branch == repo.default_branch || advertised.as_deref() == Some(branch)
                        })),
                "the default branch cannot be deleted"
            );
            let created: Vec<&str> = updates
                .iter()
                .filter(|update| {
                    update.old == ZERO_SHA
                        && (update.reference.starts_with("refs/heads/")
                            || update.reference.starts_with("refs/tags/"))
                })
                .map(|update| update.reference.as_str())
                .collect();
            if !created.is_empty() {
                let repo_path = path.clone();
                let held = tokio::task::spawn_blocking(move || {
                    crate::backend::git::branches_and_tags_in_use(&repo_path)
                })
                .await??;
                ensure_ref_room(held, &created)?;
            }
            let mut state = operation
                .state
                .lock()
                .map_err(|_| anyhow::anyhow!("operation state unavailable"))?;
            ensure!(
                state.proposed.is_none() && !state.post_started,
                "duplicate pre-receive hook"
            );
            state.proposed = Some(updates.to_vec());
            return Ok(());
        }
        {
            let mut state = operation
                .state
                .lock()
                .map_err(|_| anyhow::anyhow!("operation state unavailable"))?;
            let proposed = state
                .proposed
                .as_ref()
                .context("missing successful pre-receive hook")?;
            ensure!(
                !state.post_started && updates.iter().all(|update| proposed.contains(update)),
                "post-receive updates do not match admission"
            );
            state.post_started = true; // No replay or duplicate CI admission.
        }
        // This control request runs in the persistent backend. Disconnecting a
        // helper does not move scheduled CI into a short-lived CLI runtime.
        let mut failed = false;
        if self
            .update_metadata(&repo, &path, updates, operation.clone())
            .await
            .is_err()
        {
            failed = true;
        }
        crate::backend::git::maintenance::after_push(repo.id, path.clone());
        // Webhooks follow the refs Git has just accepted. They never fail the push.
        let moves: Vec<RefMove> = updates
            .iter()
            .map(|update| {
                (
                    update.reference.clone(),
                    update.old.clone(),
                    update.new.clone(),
                )
            })
            .collect();
        ref_changes::emit_ref_moves(&self.pool, &repo, identity.id, &path, &moves);
        // Open pull requests follow the branches this push created, moved or deleted.
        let branches: Vec<&str> = updates
            .iter()
            .filter_map(|update| update.reference.strip_prefix("refs/heads/"))
            .collect();
        if !branches.is_empty()
            && pull_requests::refresh_open_pull_requests(
                &self.pool,
                &repo,
                Some(branches.as_slice()),
                &self.ci,
                identity.id,
            )
            .await
            .is_err()
        {
            tracing::warn!(
                repo_id = repo.id,
                "could not refresh open pull requests after a push"
            );
            failed = true;
        }
        // Do not read or parse repository-controlled workflows until current identity,
        // write access and host-execution trust have all been established.
        let current_repo = db::repositories::records::get_repository_by_id(&self.pool, repo.id)
            .await?
            .context("repository unavailable")?;
        let (current_identity, _) = self
            .authorize(operation.credential, &current_repo, true)
            .await?;
        ensure!(current_identity.id == identity.id, "identity changed");
        ensure!(
            self.bare_path(&current_repo).await? == operation.path,
            "repository location changed"
        );
        // Branch pushes appear in the pusher's profile activity, with or without CI. A push to
        // the default branch also refreshes its stored languages.
        for update in updates.iter().filter(|update| update.new != ZERO_SHA) {
            let Some(branch) = update.reference.strip_prefix("refs/heads/") else {
                continue;
            };
            languages::refresh_default_branch(
                self.pool.clone(),
                repo.id,
                path.clone(),
                branch.to_owned(),
            );
            let old = (update.old != ZERO_SHA).then_some(update.old.as_str());
            let recorded = match crate::backend::git::count_pushed_commits(
                &path,
                old,
                &update.new,
                &update.reference,
            )
            .await
            {
                Ok(count) => {
                    let count = i32::try_from(count).unwrap_or(i32::MAX);
                    db::activity::record_repository_push(
                        &self.pool,
                        repo.id,
                        identity.id,
                        branch,
                        count,
                    )
                    .await
                }
                Err(error) => Err(error),
            };
            // Profile activity is not worth failing a push that Git has already accepted.
            if let Err(error) = recorded {
                tracing::warn!(repo_id = repo.id, %error, "could not record a branch push");
            }
        }
        if !current_identity.is_admin
            && !self
                .ci
                .config()
                .trusted_actor_ids
                .contains(&current_identity.id)
        {
            ensure!(
                !failed,
                "accepted Git writes have post-push bookkeeping failures"
            );
            return Ok(());
        }

        let mut match_budget = WorkflowMatchBudget::new();
        'updates: for update in updates.iter().filter(|update| update.new != ZERO_SHA) {
            let immutable_sha = update.new.clone();
            let workflow_path = path.clone();
            let loading = operation.clone();
            let loaded = tokio::task::spawn_blocking(move || {
                let _operation = loading;
                let (commit_sha, blobs) =
                    crate::backend::git::ci::load_push_workflows(&workflow_path, &immutable_sha)?;
                let mut workflows = Vec::new();
                for (path, text) in blobs {
                    match parse_push_workflow(&path, &text) {
                        Ok(workflow) => workflows.push(workflow),
                        Err(error) => tracing::warn!(
                            workflow = %path,
                            commit = %commit_sha,
                            %error,
                            "malformed push workflow skipped"
                        ),
                    }
                }
                workflows.sort_by(|a, b| a.name.cmp(&b.name).then(a.path.cmp(&b.path)));
                Ok::<_, anyhow::Error>((commit_sha, workflows))
            })
            .await?;
            let (commit_sha, workflows) = match loaded {
                Ok(loaded) => loaded,
                // A tag may name a blob or a tree. There is nothing to run for it.
                Err(error)
                    if update.reference.starts_with("refs/tags/")
                        && crate::backend::git::is_not_a_commit(&error) =>
                {
                    continue;
                }
                Err(_) => {
                    failed = true;
                    continue;
                }
            };
            let needs_paths = !update.reference.starts_with("refs/tags/")
                && workflows.iter().any(|workflow| {
                    workflow.triggers.push.as_ref().is_some_and(|filter| {
                        !filter.paths.is_empty() || !filter.paths_ignore.is_empty()
                    })
                });
            let changed = if needs_paths {
                match crate::backend::git::changed_paths(&path, &update.old, &commit_sha, true)
                    .await
                {
                    Ok(paths) => paths,
                    Err(error) => {
                        tracing::warn!(
                            repo_id = repo.id,
                            %error,
                            "push changed paths unavailable; workflows skipped"
                        );
                        failed = true;
                        continue;
                    }
                }
            } else {
                Vec::new()
            };
            let message = ci::commit_message_for_ref(
                &repo_store(&repo, self.ci.cluster.as_ref()),
                &commit_sha,
            )
            .await;
            for workflow in workflows {
                let matches = workflow.matches_event_with_context_and_budget(
                    "push",
                    &update.reference,
                    &changed,
                    message.as_deref(),
                    &mut match_budget,
                );
                let matches = match matches {
                    Ok(matches) => matches,
                    Err(error) => {
                        tracing::warn!(
                            repo_id = repo.id,
                            %error,
                            "push workflow matching stopped at its work limit"
                        );
                        failed = true;
                        break 'updates;
                    }
                };
                if !matches {
                    continue;
                }
                let pipeline = ci::infer_pipeline_from_workflow_path(&workflow.path);
                // Recheck live identity/access for each dispatch, in particular
                // CD/full. Neither event=push nor the prior pre-hook is a bypass.
                let fresh_repo =
                    db::repositories::records::get_repository_by_id(&self.pool, repo.id)
                        .await?
                        .context("repository unavailable")?;
                let (fresh_identity, access) = self
                    .authorize(operation.credential, &fresh_repo, true)
                    .await?;
                if pipeline.includes_cd() && access < AccessLevel::Admin {
                    failed = true;
                    continue;
                }
                ensure!(fresh_identity.id == identity.id, "identity changed");
                ensure!(
                    self.bare_path(&fresh_repo).await? == operation.path,
                    "repository location changed"
                );
                // Git accepted this immutable commit even if another push has
                // since moved the ref. Annotated tags use their peeled commit.
                db::ci::record_ci_accepted_push(
                    &self.pool,
                    repo.id,
                    fresh_identity.id,
                    &update.reference,
                    &commit_sha,
                )
                .await?;
                let result = self
                    .ci
                    .trigger_run(CiRunRequest {
                        repo_id: repo.id,
                        repo_owner: operation.owner.clone(),
                        repo_name: fresh_repo.name.clone(),
                        branch: update.reference.clone(),
                        head_sha: Some(commit_sha.clone()),
                        event: "push".to_owned(),
                        pull_request_id: None,
                        pipeline,
                        workflow_paths: vec![workflow.path],
                        environment: None,
                        inputs: None,
                        triggered_by_id: fresh_identity.id,
                    })
                    .await;
                if result.is_err() {
                    failed = true;
                }
            }
        }
        ensure!(
            !failed,
            "accepted Git writes have post-push dispatch failures"
        );
        Ok(())
    }

    async fn update_metadata(
        &self,
        repo: &Repository,
        path: &Path,
        updates: &[RefUpdate],
        operation: Arc<Operation>,
    ) -> Result<()> {
        // HEAD selection and its database default must belong to the same push.
        let _lock = crate::backend::git::locks::lock(path).await;
        let path = path.to_owned();
        let fallback = repo.default_branch.clone();
        let created: Vec<String> = updates
            .iter()
            .filter(|update| update.old == ZERO_SHA)
            .filter_map(|update| update.reference.strip_prefix("refs/heads/"))
            .map(str::to_owned)
            .collect();
        let (branch, size) = tokio::task::spawn_blocking(move || {
            let _operation = operation;
            let repository = git2::Repository::open_bare(&path)?;
            let head = repository.find_reference("HEAD")?;
            let mut branch = head
                .symbolic_target()
                .and_then(|name| name.strip_prefix("refs/heads/"))
                .map(str::to_owned)
                .unwrap_or(fallback);
            // A first push can create only a branch other than the initial HEAD. Git
            // leaves HEAD unborn, so the first pushed branch becomes the default.
            if repository.head().is_err() {
                if let Some(first) = reads::preferred_branch(created.iter().map(String::as_str)) {
                    repository.set_head(&format!("refs/heads/{first}"))?;
                    branch = first.to_owned();
                }
            }
            let size = crate::backend::git::stats::directory_size(&path)?;
            Ok::<_, anyhow::Error>((branch, size.min(i64::MAX as u64) as i64))
        })
        .await??;
        db::repositories::records::update_repo_default_branch_and_size(
            &self.pool, repo.id, &branch, size,
        )
        .await?;
        if repo.size_bytes != size {
            db::repositories::records::insert_repo_storage_snapshot(&self.pool, repo.id, size)
                .await?;
        }
        Ok(())
    }
}
