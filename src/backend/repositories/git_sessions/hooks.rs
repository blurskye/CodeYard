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
    git::{branches_and_tags_in_use, ref_listing_bytes, RefsInUse},
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
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
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

/// The branches and tags a push creates. Each one counts against the repository's ref limit.
fn created_refs(updates: &[RefUpdate]) -> Vec<&str> {
    updates
        .iter()
        .filter(|update| {
            update.old == ZERO_SHA
                && (update.reference.starts_with("refs/heads/")
                    || update.reference.starts_with("refs/tags/"))
        })
        .map(|update| update.reference.as_str())
        .collect()
}

/// The refs that the other admitted pushes to the repository of `operation` will create. Those
/// pushes' proposed updates hold the reservation, and an operation leaves the map when it ends,
/// so the reservation ends with it.
fn reserved_by_others(
    operations: &HashMap<String, Arc<Operation>>,
    operation: &Arc<Operation>,
    repository: &git2::Repository,
) -> Result<RefsInUse> {
    let mut reserved = RefsInUse { count: 0, bytes: 0 };
    let mut names = std::collections::HashSet::new();
    for other in operations.values() {
        if other.repo_id != operation.repo_id || Arc::ptr_eq(other, operation) {
            continue;
        }
        let state = other
            .state
            .lock()
            .map_err(|_| anyhow::anyhow!("operation state unavailable"))?;
        for name in created_refs(state.proposed.as_deref().unwrap_or(&[])) {
            if !names.insert(name.to_owned()) {
                continue;
            }
            match repository.find_reference(name) {
                Ok(_) => continue, // Already included in the on-disk count.
                Err(error) if error.code() == git2::ErrorCode::NotFound => {}
                Err(error) => return Err(error.into()),
            }
            reserved.count += 1;
            reserved.bytes += ref_listing_bytes(name);
        }
    }
    Ok(reserved)
}

/// Admits a push as proposed. The refs it creates are checked against the repository's ref limit
/// together with the refs that the other admitted pushes will create. The count and the other
/// pushes' reservations are read while the operations lock is held, and this push's proposed
/// updates are stored under the same hold, so two pushes cannot both pass.
fn admit_updates(
    operations: &Mutex<HashMap<String, Arc<Operation>>>,
    operation: &Arc<Operation>,
    path: &Path,
    updates: &[RefUpdate],
) -> Result<()> {
    let operations = operations
        .lock()
        .map_err(|_| anyhow::anyhow!("operation state unavailable"))?;
    let created = created_refs(updates);
    if !created.is_empty() {
        let held = branches_and_tags_in_use(path)?;
        let reserved =
            reserved_by_others(&operations, operation, &git2::Repository::open_bare(path)?)?;
        ensure_ref_room(
            RefsInUse {
                count: held.count + reserved.count,
                bytes: held.bytes + reserved.bytes,
            },
            &created,
        )?;
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
    Ok(())
}

impl Gateway {
    pub(super) async fn hook(
        self: &Arc<Self>,
        operation: &Arc<Operation>,
        post: bool,
        updates: &[RefUpdate],
        quarantine: Option<&str>,
    ) -> Result<()> {
        validation::validate_updates(updates)?;
        #[cfg(test)]
        super::broker::policy_tests::before_authorize(operation, post).await;
        // All transports and server-side ref mutations use this same repository lock.
        let publication = if post {
            None
        } else {
            Some(crate::backend::git::locks::lock(&operation.path).await)
        };
        let repo = db::repositories::records::get_repository_by_id(&self.pool, operation.repo_id)
            .await?
            .context("repository unavailable")?;
        let path = self.bare_path(&repo).await?;
        ensure!(path == operation.path, "repository location changed");
        let authorized = operation
            .credential
            .authorize(&self.pool, &repo, true)
            .await;
        if !post {
            let (identity, _) = authorized?;
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
            let gateway = self.clone();
            let (admitted, admitted_path, admitted_updates) =
                (operation.clone(), path.clone(), updates.to_vec());
            tokio::task::spawn_blocking(move || {
                let tags: Vec<_> = admitted_updates
                    .iter()
                    .filter(|update| update.reference.starts_with("refs/tags/"))
                    .map(|update| (update.reference.clone(), update.new.clone()))
                    .collect();
                if !tags.is_empty() {
                    crate::backend::git::check_tag_budget(
                        &admitted_path,
                        quarantine.as_deref(),
                        &tags,
                    )?;
                }
                admit_updates(
                    &gateway.operations,
                    &admitted,
                    &admitted_path,
                    &admitted_updates,
                )
            })
            .await??;
            operation
                .state
                .lock()
                .map_err(|_| anyhow::anyhow!("operation state unavailable"))?
                .publication = publication;
            return Ok(());
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
        // Metadata describes accepted refs even if the credential has since been revoked.
        let (identity, _) = authorized?;
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
        let (current_identity, _) = operation
            .credential
            .authorize(&self.pool, &current_repo, true)
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
            let message = ci::commit_message_for_ref(
                &repo_store(&repo, self.ci.cluster.as_ref()),
                &commit_sha,
            )
            .await;
            let matched = ci::push::matching_push_workflows(
                &repo_store(&repo, self.ci.cluster.as_ref()),
                &ci::RefUpdate {
                    reference: update.reference.clone(),
                    old: (update.old != ZERO_SHA).then(|| update.old.clone()),
                    new: commit_sha.clone(),
                    changed_paths: None,
                },
                workflows,
                message.as_deref(),
                &mut match_budget,
            )
            .await;
            let matched = match matched {
                Ok(matched) => matched,
                Err(error) => {
                    tracing::warn!(repo_id = repo.id, %error, "push workflow matching failed");
                    failed = true;
                    break 'updates;
                }
            };
            for workflow in matched {
                let pipeline = ci::infer_pipeline_from_workflow_path(&workflow.path);
                // Recheck live identity/access for each dispatch, in particular
                // CD/full. Neither event=push nor the prior pre-hook is a bypass.
                let fresh_repo =
                    db::repositories::records::get_repository_by_id(&self.pool, repo.id)
                        .await?
                        .context("repository unavailable")?;
                let (fresh_identity, access) = operation
                    .credential
                    .authorize(&self.pool, &fresh_repo, true)
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
                    .trigger_run_with_access(
                        CiRunRequest {
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
                            manual: false,
                        },
                        access,
                    )
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

#[cfg(test)]
mod tests {
    use super::super::broker::HookState;
    use super::*;

    fn creates(prefix: &str, count: usize) -> Vec<RefUpdate> {
        (0..count)
            .map(|index| RefUpdate {
                old: ZERO_SHA.to_owned(),
                new: "1".repeat(40),
                reference: format!("refs/heads/{prefix}-{index}"),
            })
            .collect()
    }

    fn operation(repo_id: i64) -> Arc<Operation> {
        Arc::new(Operation {
            credential: ReceiveCredential::SshKey(1),
            repo_id,
            owner: "owner".to_owned(),
            path: PathBuf::new(),
            hooks: tempfile::tempdir().unwrap(),
            state: Mutex::new(HookState::default()),
        })
    }

    #[test]
    fn published_creations_are_not_counted_again_as_reservations() {
        let root = tempfile::tempdir().unwrap();
        let repo = git2::Repository::init_bare(root.path()).unwrap();
        let tree = repo
            .find_tree(repo.treebuilder(None).unwrap().write().unwrap())
            .unwrap();
        let author = git2::Signature::now("Tester", "tester@example.invalid").unwrap();
        let commit = repo
            .commit(None, &author, &author, "root", &tree, &[])
            .unwrap();
        let first = operation(7);
        first.state.lock().unwrap().proposed = Some(creates("first", 1));
        let second = operation(7);
        let operations = [("first".into(), first), ("second".into(), second.clone())].into();
        assert_eq!(
            reserved_by_others(&operations, &second, &repo)
                .unwrap()
                .count,
            1
        );
        repo.reference("refs/heads/first-0", commit, false, "published")
            .unwrap();
        assert_eq!(
            reserved_by_others(&operations, &second, &repo)
                .unwrap()
                .count,
            0
        );
    }

    /// Pushes that are admitted at the same time share the ref limit. The second push is refused
    /// when the first one's creations and its own would pass it, and a push that ends stops
    /// reserving its refs.
    #[test]
    fn admitted_pushes_share_the_ref_limit() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("shared.git");
        let repo = git2::Repository::init_bare(&path).unwrap();
        let tree = repo
            .find_tree(repo.treebuilder(None).unwrap().write().unwrap())
            .unwrap();
        let signature = git2::Signature::now("Tester", "tester@example.invalid").unwrap();
        let commit = repo
            .commit(None, &signature, &signature, "root", &tree, &[])
            .unwrap();
        for index in 0..crate::backend::git::MAX_REPOSITORY_REFS - 10 {
            repo.reference(&format!("refs/heads/held-{index}"), commit, false, "held")
                .unwrap();
        }
        let operations = Mutex::new(HashMap::new());
        let first = operation(7);
        let second = operation(7);
        let third = operation(7);

        operations
            .lock()
            .unwrap()
            .insert("first".to_owned(), first.clone());
        admit_updates(&operations, &first, &path, &creates("first", 5)).unwrap();
        operations
            .lock()
            .unwrap()
            .insert("second".to_owned(), second.clone());
        // 9,990 refs are on disk and 5 are reserved, so six more would pass the limit.
        assert!(admit_updates(&operations, &second, &path, &creates("second", 6)).is_err());
        admit_updates(&operations, &second, &path, &creates("second", 5)).unwrap();
        operations
            .lock()
            .unwrap()
            .insert("third".to_owned(), third.clone());
        assert!(admit_updates(&operations, &third, &path, &creates("third", 1)).is_err());

        // The first push ends, and its reservation ends with it.
        operations.lock().unwrap().remove("first");
        admit_updates(&operations, &third, &path, &creates("third", 1)).unwrap();
    }
}
