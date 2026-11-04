//! Receive authorization and follow-up after Git accepts immutable updates.
use crate::ci::runs::CiRunRequest;
use crate::repositories::git_sessions::broker::Gateway;
use crate::repositories::git_sessions::broker::Operation;
use crate::repositories::git_sessions::validation::RefUpdate;
use crate::repositories::git_sessions::validation::ZERO_SHA;
use crate::repositories::git_sessions::ReceiveCredential;
use crate::repositories::protection::check_branch_write_with;
use crate::repositories::protection::load_rules;
use crate::repositories::protection::BranchWrite;
use crate::repositories::protection::History;
use crate::repositories::protection::Refusal;
use crate::repositories::protection::WriteVia;
use crate::repositories::store::local::ensure_ref_room;
use crate::repositories::store::repo_store;
use crate::webhooks::ref_changes::RefMove;
use anyhow::ensure;
use anyhow::Result;
use codeyard_db_core::models::AccessLevel;
use codeyard_db_core::models::Repository;
use codeyard_db_core::models::User;
use codeyard_git::error::ErrorKind;
use codeyard_git::error::RepoError;
use codeyard_git::published_refs_in_use;
use codeyard_git::ref_listing_bytes;
use codeyard_git::RefsInUse;
use codeyard_model::workflow_glob::WorkflowMatchBudget;
use std::collections::HashMap;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;

fn quorum_refusal() -> anyhow::Error {
    RepoError::new(ErrorKind::Invalid, "write quorum unavailable").into()
}

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

/// Each created public ref counts against the same repository quota and reservations.
fn created_refs(updates: &[RefUpdate]) -> Vec<&str> {
    updates
        .iter()
        .filter(|update| update.old == ZERO_SHA && codeyard_git::is_public_ref(&update.reference))
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
        let held = published_refs_in_use(path)?;
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
    if updates
        .iter()
        .any(|update| update.old == ZERO_SHA && update.reference.starts_with("refs/heads/"))
    {
        state.previous_refs = codeyard_git::list_mirror_refs(path)?
            .into_iter()
            .filter(|(name, _)| name.starts_with("refs/heads/") || name.starts_with("refs/tags/"))
            .map(|(_, oid)| oid)
            .collect();
    }
    state.proposed = Some(updates.to_vec());
    Ok(())
}

impl Gateway {
    pub(super) async fn reference_transaction(
        self: &Arc<Self>,
        operation: &Arc<Operation>,
        phase: crate::repositories::git_sessions::protocol::TransactionPhase,
        updates: &[RefUpdate],
    ) -> Result<()> {
        use crate::repositories::git_sessions::protocol::TransactionPhase;

        use codeyard_db_cluster::cluster::writes as journal;
        crate::repositories::git_sessions::validation::validate_transaction_updates(updates)?;
        if !operation
            .state
            .lock()
            .map_err(|_| anyhow::anyhow!("operation state unavailable"))?
            .quorum
        {
            return Ok(());
        }
        // HEAD may be included as an alias for the branch Git is updating. Receive-pack
        // cannot change symbolic HEAD itself; only the branch's oid enters this batch.
        let mut changes: Vec<_> = updates
            .iter()
            .filter(|update| update.reference != "HEAD" && update.old != update.new)
            .map(|update| codeyard_db_core::models::RefChange {
                name: update.reference.clone(),
                kind: "oid".into(),
                old_target: (update.old != ZERO_SHA).then(|| update.old.clone()),
                new_target: (update.new != ZERO_SHA).then(|| update.new.clone()),
            })
            .collect();
        changes.sort_by(|a, b| a.name.cmp(&b.name));
        if changes.is_empty() {
            return Ok(());
        }
        match phase {
            TransactionPhase::Prepared => {
                {
                    let mut state = operation
                        .state
                        .lock()
                        .map_err(|_| anyhow::anyhow!("operation state unavailable"))?;
                    ensure!(
                        state.transaction.is_none() && state.publication.is_some(),
                        "unadmitted reference transaction"
                    );
                    let proposed = state
                        .proposed
                        .as_ref()
                        .context("missing pre-receive admission")?;
                    ensure!(
                        updates.iter().all(|update| proposed.contains(update)
                            || (update.reference == "HEAD"
                                && proposed.iter().any(|proposal| proposal.old == update.old
                                    && proposal.new == update.new))),
                        "reference transaction differs from admission"
                    );
                    ensure!(
                        state.writes.len()
                            < crate::repositories::git_sessions::validation::MAX_UPDATES,
                        "too many reference transactions"
                    );
                    state.quorum_started = true;
                }
                let (repo, _) = self.operation_repository(operation).await?;
                let (actor, _) = operation
                    .credential
                    .authorize(&self.pool, &repo, true)
                    .await?;
                ensure!(actor.id == operation.actor_id, "identity changed");
                let cluster = self.ci.cluster.as_ref().context("cluster unavailable")?;
                let primary = cluster.node_id;
                let repository = crate::cluster::rpc::repository::repository_uuid(&repo.disk_path)?;
                let view = journal::recovery_view(&self.pool, repo.id).await?;
                let id = uuid::Uuid::new_v4().to_string();
                let write = match crate::cluster::writes::begin(
                    &self.pool, repo.id, primary, view.term, &id, &changes,
                )
                .await
                {
                    Ok(write) => write,
                    Err(error) => {
                        if error.is::<crate::cluster::writes::OutcomeUnknown>() {
                            operation
                                .state
                                .lock()
                                .unwrap_or_else(|error| error.into_inner())
                                .quorum_unknown = true;
                        }
                        return Err(quorum_refusal());
                    }
                };
                let index = {
                    let mut state = operation
                        .state
                        .lock()
                        .map_err(|_| anyhow::anyhow!("operation state unavailable"))?;
                    let index = state.writes.len();
                    state.writes.push(write.record().clone());
                    state.transaction = Some(index);
                    index
                };
                let lease = operation
                    .publication_lease
                    .as_ref()
                    .context("publication lease missing")?;
                let prepared = lease
                    .scope(cluster.prepare_push_write(repository, &operation.path, write.record()))
                    .await;
                let votes = prepared.is_ok()
                    && crate::cluster::writes::collect_votes(&self.pool, &write)
                        .await
                        .unwrap_or(false);
                // Recheck after network and disk waits, immediately before the decision.
                let authorized = async {
                    let (repo, path) = self.operation_repository(operation).await?;
                    let (actor, _) = operation
                        .credential
                        .authorize(&self.pool, &repo, true)
                        .await?;
                    ensure!(actor.id == operation.actor_id, "identity changed");
                    let via = if matches!(operation.credential, ReceiveCredential::DeployKey(_)) {
                        WriteVia::DeployKey
                    } else {
                        WriteVia::Direct
                    };
                    self.check_publication_branches(
                        &repo,
                        &actor,
                        updates,
                        &History::Stored { repository: &path },
                        via,
                    )
                    .await?;
                    crate::repositories::releases::immutable::check_current_tag_updates(
                        &self.pool, &repo, updates,
                    )
                    .await?;
                    Ok::<_, anyhow::Error>(())
                };
                let decided = crate::cluster::writes::decide(
                    &self.pool,
                    &write,
                    votes && authorized.await.is_ok(),
                )
                .await?;
                operation
                    .state
                    .lock()
                    .map_err(|_| anyhow::anyhow!("operation state unavailable"))?
                    .writes[index] = decided.clone();
                if decided.decision != "commit" {
                    if lease
                        .scope(cluster.abort_push_write(repository, &operation.path, &decided))
                        .await
                        .is_ok()
                    {
                        journal::finish_write(&self.pool, &decided.id, decided.term).await?;
                    }
                    return Err(quorum_refusal());
                }
                Ok(())
            }
            TransactionPhase::Committed | TransactionPhase::Aborted => {
                let write = {
                    let mut state = operation
                        .state
                        .lock()
                        .map_err(|_| anyhow::anyhow!("operation state unavailable"))?;
                    let index = state
                        .transaction
                        .take()
                        .context("missing prepared transaction")?;
                    state.writes[index].clone()
                };
                ensure!(
                    write.changes == changes,
                    "reference transaction changed after preparation"
                );
                let write = journal::load_write(&self.pool, &write.id)
                    .await?
                    .context("write disappeared")?;
                if write.decision == "commit" {
                    ensure!(
                        phase == TransactionPhase::Committed,
                        crate::cluster::writes::OutcomeUnknown
                    );
                    let (repo, _) = self.operation_repository(operation).await?;
                    let repository =
                        crate::cluster::rpc::repository::repository_uuid(&repo.disk_path)?;
                    let cluster = self.ci.cluster.as_ref().context("cluster unavailable")?;
                    let lease = operation
                        .publication_lease
                        .as_ref()
                        .context("publication lease missing")?;
                    lease
                        .scope(cluster.publish_push_write(repository, &operation.path, &write))
                        .await
                } else {
                    let aborted = journal::decide_write(
                        &self.pool,
                        &write.id,
                        write.primary_id,
                        write.term,
                        false,
                    )
                    .await?;
                    ensure!(
                        aborted.decision == "abort",
                        crate::cluster::writes::OutcomeUnknown
                    );
                    let (repo, _) = self.operation_repository(operation).await?;
                    let repository =
                        crate::cluster::rpc::repository::repository_uuid(&repo.disk_path)?;
                    let cluster = self.ci.cluster.as_ref().context("cluster unavailable")?;
                    let lease = operation
                        .publication_lease
                        .as_ref()
                        .context("publication lease missing")?;
                    if lease
                        .scope(cluster.abort_push_write(repository, &operation.path, &aborted))
                        .await
                        .is_ok()
                    {
                        journal::finish_write(&self.pool, &write.id, write.term).await?;
                    }
                    Ok(())
                }
            }
        }
    }

    pub(super) async fn complete_quorum(&self, operation: &Operation) -> Result<bool> {
        use crate::cluster::writes::OutcomeUnknown;
        let writes = {
            let state = operation
                .state
                .lock()
                .map_err(|_| anyhow::anyhow!("operation state unavailable"))?;
            ensure!(!state.quorum_unknown, OutcomeUnknown);
            state.writes.clone()
        };
        let mut committed = false;
        for write in writes {
            let current = tokio::time::timeout(
                std::time::Duration::from_secs(5),
                codeyard_db_cluster::cluster::writes::load_write(&self.pool, &write.id),
            )
            .await??
            .context("write disappeared")?;
            ensure!(
                current.decision == "abort" || (current.decision == "commit" && current.finished),
                OutcomeUnknown
            );
            committed |= current.decision == "commit";
        }
        Ok(committed)
    }

    pub(super) async fn hook(
        self: &Arc<Self>,
        operation: &Arc<Operation>,
        post: bool,
        updates: &[RefUpdate],
        quarantine: Option<&str>,
    ) -> Result<()> {
        crate::repositories::git_sessions::validation::validate_updates(updates)?;
        #[cfg(test)]
        crate::repositories::git_sessions::broker::policy_tests::before_authorize(operation, post)
            .await;
        if post {
            self.post_receive(operation, updates).await
        } else {
            self.pre_receive(operation, updates, quarantine).await
        }
    }

    /// The operation's repository, still at the location it was admitted at.
    async fn operation_repository(&self, operation: &Operation) -> Result<(Repository, PathBuf)> {
        let repo = codeyard_db_repos::repositories::records::get_repository_by_id(
            &self.pool,
            operation.repo_id,
        )
        .await?
        .context("repository unavailable")?;
        let committed_term = operation
            .state
            .lock()
            .map_err(|_| anyhow::anyhow!("operation state unavailable"))?
            .writes
            .iter()
            .find(|write| write.decision == "commit")
            .map(|write| write.term);
        let path = if let Some(term) = committed_term {
            // This admitted child must publish before its voter can be marked current.
            // It may finish only in its original serving term, never in a successor's path.
            let cluster = self.ci.cluster.as_ref().context("cluster unavailable")?;
            let view =
                codeyard_db_cluster::cluster::writes::recovery_view(&self.pool, repo.id).await?;
            ensure!(
                view.serving && view.term == term && view.primary_id == cluster.node_id,
                "repository term changed"
            );
            let node = codeyard_db_cluster::cluster::get_node_by_id(&self.pool, cluster.node_id)
                .await?
                .context("storage node unavailable")?;
            ensure!(
                node.status == "active"
                    && node.public_key == cluster.key.public_hex()
                    && node.roles.iter().any(|role| role == "storage"),
                "storage node unavailable"
            );
            let uuid = crate::cluster::rpc::repository::repository_uuid(&repo.disk_path)?;
            let path = crate::cluster::context::term_path(&cluster.repos_path, uuid, term);
            crate::cluster::context::literal_directory(&cluster.repos_path, &path).await?;
            path
        } else {
            self.bare_path(&repo).await?
        };
        ensure!(path == operation.path, "repository location changed");
        Ok((repo, path))
    }

    async fn check_publication_branches(
        &self,
        repo: &Repository,
        identity: &User,
        updates: &[RefUpdate],
        history: &History<'_>,
        via: WriteVia,
    ) -> Result<()> {
        let branches = updates
            .iter()
            .filter_map(|update| {
                update
                    .reference
                    .strip_prefix("refs/heads/")
                    .map(|branch| (update, branch))
            })
            .collect::<Vec<_>>();
        if branches.is_empty() {
            return Ok(());
        }
        let rules = load_rules(&self.pool, repo)
            .await
            .map_err(|_| anyhow::anyhow!("branch protection could not be checked"))?;
        let mut budget = WorkflowMatchBudget::new();
        for (update, branch) in branches {
            let write = BranchWrite {
                branch,
                old: (update.old != ZERO_SHA).then_some(update.old.as_str()),
                new: (update.new != ZERO_SHA).then_some(update.new.as_str()),
                via,
            };
            check_branch_write_with(
                &self.pool,
                &rules,
                repo,
                identity,
                &write,
                history,
                &mut budget,
            )
            .await
            .map_err(|refusal| match refusal {
                Refusal::Blocked(reason) => {
                    anyhow::Error::from(RepoError::new(ErrorKind::Invalid, reason))
                }
                Refusal::Unavailable => anyhow::anyhow!("branch protection could not be checked"),
            })?;
        }
        Ok(())
    }

    /// Judges a push before Git accepts it and keeps the repository lock until it is published.
    async fn pre_receive(
        self: &Arc<Self>,
        operation: &Arc<Operation>,
        updates: &[RefUpdate],
        quarantine: Option<&str>,
    ) -> Result<()> {
        // Lock the same open description the receive child inherited before negotiation.
        // If this backend dies after admission, the child still owns the publication lock.
        let publication = operation
            .publication_lease
            .as_ref()
            .context("receive publication lease missing")?
            .acquire(&operation.path)
            .await?;
        let (repo, path) = self.operation_repository(operation).await?;
        let (identity, _) = operation
            .credential
            .authorize(&self.pool, &repo, true)
            .await?;
        ensure!(identity.id == operation.actor_id, "identity changed");
        crate::repositories::releases::immutable::check_current_tag_updates(
            &self.pool, &repo, updates,
        )
        .await?;
        let quarantine = match quarantine {
            Some(claimed) => quarantine_directory(&path, claimed).await,
            None => None,
        };
        let heads: Vec<_> = updates
            .iter()
            .filter(|update| update.new != ZERO_SHA)
            .map(|update| update.new.clone())
            .collect();
        let bypasses =
            crate::repositories::secret_scanning::push_protection::refuse_blocked_secrets(
                &self.pool,
                &repo,
                identity.id,
                &path,
                quarantine.as_deref(),
                &heads,
            )
            .await?;
        operation
            .state
            .lock()
            .map_err(|_| anyhow::anyhow!("operation state unavailable"))?
            .secret_bypasses = bypasses;
        if repo.is_gist {
            ensure!(
                updates.len() == 1
                    && updates[0].reference == "refs/heads/main"
                    && updates[0].new != ZERO_SHA,
                "A gist has one nonempty main branch"
            );
            let path = path.clone();
            let quarantine = quarantine.clone();
            let revision = updates[0].new.clone();
            let old = updates[0].old.clone();
            tokio::task::spawn_blocking(move || {
                codeyard_git::gists::validate_push(&path, quarantine.as_deref(), &old, &revision)
            })
            .await??;
        }
        let history = History::Push {
            repository: &path,
            quarantine: quarantine.as_deref(),
        };
        let via = if matches!(operation.credential, ReceiveCredential::DeployKey(_)) {
            WriteVia::DeployKey
        } else {
            WriteVia::Direct
        };
        self.check_publication_branches(&repo, &identity, updates, &history, via)
            .await?;
        // The stored default can lag behind Git, so the default that Git advertises is
        // protected as well.
        let advertised = codeyard_git::repository::default_branch(&path).await?;
        if updates.iter().any(|update| {
            update.new == ZERO_SHA
                && update
                    .reference
                    .strip_prefix("refs/heads/")
                    .is_some_and(|branch| {
                        branch == repo.default_branch || advertised.as_deref() == Some(branch)
                    })
        }) {
            return Err(RepoError::new(
                ErrorKind::Invalid,
                "The default branch cannot be deleted.",
            )
            .into());
        }
        let check_workflows = if let ReceiveCredential::Token(id) = operation.credential {
            !codeyard_identity::oauth_apps::allows_workflows(&self.pool, id).await?
        } else {
            false
        };
        let ref_change = crate::repositories::store::local::begin_coordinated_ref_change(
            &self.pool,
            repo.id,
            &operation.path,
            Some(repo.storage_node_id),
        )
        .await?;
        let gateway = self.clone();
        let (admitted, admitted_path, admitted_updates) =
            (operation.clone(), path.clone(), updates.to_vec());
        let admitted_quarantine = quarantine.clone();
        let held = publication.clone();
        let quorum = operation
            .state
            .lock()
            .map_err(|_| anyhow::anyhow!("operation state unavailable"))?
            .quorum;
        let publications = tokio::task::spawn_blocking(move || {
            let _held = held;
            if check_workflows {
                codeyard_git::oauth::check_workflows(
                    &admitted_path,
                    admitted_quarantine.as_deref(),
                    &admitted_updates
                        .iter()
                        .map(|update| (update.old.clone(), update.new.clone()))
                        .collect::<Vec<_>>(),
                )?;
            }
            let tags: Vec<_> = admitted_updates
                .iter()
                .filter(|update| update.reference.starts_with("refs/tags/"))
                .map(|update| (update.reference.clone(), update.new.clone()))
                .collect();
            if !tags.is_empty() {
                codeyard_git::check_tag_budget(
                    &admitted_path,
                    admitted_quarantine.as_deref(),
                    &tags,
                )?;
            }
            admit_updates(
                &gateway.operations,
                &admitted,
                &admitted_path,
                &admitted_updates,
            )?;
            let publications = admitted_updates
                .iter()
                .filter(|update| update.reference.starts_with("refs/heads/"))
                .map(|update| codeyard_git::PublicationUpdate {
                    reference: update.reference.clone(),
                    old: (update.old != ZERO_SHA).then(|| update.old.clone()),
                    new: (update.new != ZERO_SHA).then(|| update.new.clone()),
                })
                .collect::<Vec<_>>();
            let actor = match admitted.credential {
                ReceiveCredential::DeployKey(id) => codeyard_git::PublicationActor::DeployKey(id),
                ReceiveCredential::SshKey(_) | ReceiveCredential::Token(_) => {
                    codeyard_git::PublicationActor::User(admitted.actor_id)
                }
            };
            if quorum {
                codeyard_git::plan_branch_publications(
                    &admitted_path,
                    repo.id,
                    actor,
                    &identity.username,
                    &publications,
                )
            } else {
                codeyard_git::prepare_branch_publications(
                    &admitted_path,
                    repo.id,
                    actor,
                    &identity.username,
                    &publications,
                )?;
                Ok(Vec::new())
            }
        })
        .await??;
        if !publications.is_empty() {
            let cluster = self.ci.cluster.as_ref().context("cluster unavailable")?;
            let uuid = crate::cluster::rpc::repository::repository_uuid(&repo.disk_path)?;
            let term = codeyard_db_cluster::cluster::writes::recovery_view(&self.pool, repo.id)
                .await?
                .term;
            let changes = publications
                .into_iter()
                .map(|change| codeyard_db_core::models::RefChange {
                    name: change.reference,
                    kind: "oid".into(),
                    old_target: change.old,
                    new_target: change.new,
                })
                .collect::<Vec<_>>();
            let result = publication
                .scope(
                    cluster.publish_ref_changes(uuid, &path, repo.id, term, &changes, async {
                        let (repo, _) = self.operation_repository(operation).await?;
                        let (actor, _) = operation
                            .credential
                            .authorize(&self.pool, &repo, true)
                            .await?;
                        ensure!(actor.id == operation.actor_id, "identity changed");
                        self.check_publication_branches(&repo, &actor, updates, &history, via)
                            .await?;
                        Ok(())
                    }),
                )
                .await;
            if let Err(error) = result {
                if error.is::<crate::cluster::writes::OutcomeUnknown>() {
                    operation
                        .state
                        .lock()
                        .unwrap_or_else(|error| error.into_inner())
                        .quorum_unknown = true;
                }
                return Err(error);
            }
        }
        // An intent written before a slow batch is not authority to publish after revocation.
        let (current, _) = self.operation_repository(operation).await?;
        let (identity, _) = operation
            .credential
            .authorize(&self.pool, &current, true)
            .await?;
        ensure!(identity.id == operation.actor_id, "identity changed");
        self.check_publication_branches(&current, &identity, updates, &history, via)
            .await?;
        // A failed creation deletes its repository only while no write has reached it, so the
        // version moves before Git takes the push.
        codeyard_db_repos::repositories::records::advance_contribution_version(
            &self.pool, current.id,
        )
        .await?;
        let mut state = operation
            .state
            .lock()
            .map_err(|_| anyhow::anyhow!("operation state unavailable"))?;
        state.ref_change = Some(ref_change);
        state.publication = Some(publication);
        Ok(())
    }

    /// Follow-up after Git accepted the push: metadata, events, pull requests and CI.
    async fn post_receive(&self, operation: &Arc<Operation>, updates: &[RefUpdate]) -> Result<()> {
        let (repo, path) = self.operation_repository(operation).await?;
        // Proofs made while this push was in flight saw the old history under the new version.
        // This comes first, so no later bookkeeping failure can leave them current.
        codeyard_db_repos::repositories::records::advance_contribution_version(&self.pool, repo.id)
            .await?;
        if let Some(actor) =
            codeyard_db_identity::users::accounts::get_user_by_id(&self.pool, operation.actor_id)
                .await?
        {
            // Git has moved the pushed objects out of quarantine by now.
            let history = History::Stored { repository: &path };
            if let Err(error) = crate::repositories::rule_suites::record_push(
                &self.pool,
                &repo,
                &actor,
                updates,
                &history,
                Some(operation.credential),
            )
            .await
            {
                tracing::warn!(%error, "could not record accepted rule suites");
            }
        }
        let bypasses = operation
            .state
            .lock()
            .map_err(|_| anyhow::anyhow!("operation state unavailable"))?
            .secret_bypasses
            .clone();
        let store = repo_store(&repo, self.ci.cluster.as_ref());
        let mut consumed = std::collections::HashSet::new();
        for bypass in bypasses {
            let mut accepted = false;
            for update in updates.iter().filter(|update| update.new != ZERO_SHA) {
                if store
                    .is_ancestor(&bypass.commit, &update.new)
                    .await
                    .unwrap_or(false)
                {
                    accepted = true;
                    break;
                }
            }
            if !accepted || !consumed.insert(bypass.hash.clone()) {
                continue;
            }
            codeyard_db_security::secret_scanning::use_bypass(
                &self.pool,
                repo.id,
                operation.actor_id,
                &bypass.hash,
                chrono::Utc::now(),
            )
            .await?;
        }
        // This control request runs in the persistent backend. Disconnecting a
        // helper does not move scheduled CI into a short-lived CLI runtime.
        let mut failed = false;
        if let Err(error) = self
            .update_metadata(&repo, &path, updates, operation.clone())
            .await
        {
            tracing::warn!(
                repo_id = repo.id,
                error = %format!("{error:#}"),
                "could not update repository metadata after a push"
            );
            failed = true;
        }
        operation
            .state
            .lock()
            .map_err(|_| anyhow::anyhow!("operation state unavailable"))?
            .ref_change
            .take();
        codeyard_git::maintenance::after_push(repo.id, path.clone());
        let mut scan_heads = std::collections::HashSet::new();
        for update in updates.iter().filter(|update| update.new != ZERO_SHA) {
            let commit = match store.commit(&update.new).await {
                Ok(commit) => commit,
                Err(error)
                    if update.reference.starts_with("refs/tags/")
                        && error.kind == ErrorKind::NotFound =>
                {
                    continue
                }
                Err(_) => {
                    failed = true;
                    continue;
                }
            };
            if !scan_heads.insert(commit.sha.clone()) {
                continue;
            }
            if codeyard_db_security::secret_scanning::enqueue(
                &self.pool,
                repo.id,
                &commit.sha,
                operation.actor_id,
            )
            .await
            .is_err()
            {
                tracing::warn!(
                    repo_id = repo.id,
                    "could not queue secret scanning after a push"
                );
                failed = true;
            }
        }
        if repo.is_gist {
            if codeyard_db_repos::gists::pushed(&self.pool, repo.id, Some(repo.storage_node_id))
                .await
                .is_err()
            {
                failed = true;
                tracing::error!(repo_id = repo.id, "could not update gist after push");
            }
            return if failed {
                Err(crate::repositories::git_sessions::PostReceiveFailed.into())
            } else {
                Ok(())
            };
        }
        // Accepted-write bookkeeping uses the admitted actor, not credential liveness. CI keeps
        // the actor's live access, and a credential revoked since the push gets no access.
        let actor_id = operation.actor_id;
        let ci_access = match operation
            .credential
            .authorize(&self.pool, &repo, true)
            .await
        {
            Ok((identity, access)) if identity.id == actor_id => access,
            _ => AccessLevel::None,
        };
        let repo =
            codeyard_db_repos::repositories::records::get_repository_by_id(&self.pool, repo.id)
                .await?
                .context("repository unavailable")?;
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
        if let Err(error) = crate::repositories::activity::record_moves(
            &self.pool, repo.id, actor_id, &path, &moves,
        )
        .await
        {
            tracing::warn!(repo_id=repo.id,%error,"could not record accepted Git ref activity");
            failed = true;
        }
        crate::repositories::dependencies::observe_refs(&self.pool, &repo, &moves).await;
        crate::webhooks::ref_changes::emit_ref_moves(&self.pool, &repo, actor_id, &path, &moves)
            .await;
        // Open pull requests follow the branches this push created, moved or deleted.
        let branches: Vec<&str> = updates
            .iter()
            .filter_map(|update| update.reference.strip_prefix("refs/heads/"))
            .collect();
        if !branches.is_empty()
            && crate::repositories::pull_requests::branches::refresh_open_pull_requests(
                &self.pool,
                &repo,
                Some(branches.as_slice()),
                &self.ci,
                actor_id,
                ci_access,
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
        let previous = operation
            .state
            .lock()
            .map_err(|_| anyhow::anyhow!("operation state unavailable"))?
            .previous_refs
            .clone();
        for update in updates.iter().filter(|update| update.new != ZERO_SHA) {
            if let Some(branch) = update.reference.strip_prefix("refs/heads/") {
                crate::repositories::languages::refresh_default_branch(
                    self.pool.clone(),
                    repo.id,
                    path.clone(),
                    branch.to_owned(),
                    Some(repo.storage_node_id),
                );
            }
        }
        // Optional profile counts share one short budget and identical ranges are walked once.
        let activity = async {
            let mut counts = HashMap::new();
            for update in updates.iter().filter(|update| update.new != ZERO_SHA) {
                let Some(branch) = update.reference.strip_prefix("refs/heads/") else {
                    continue;
                };
                let range = (update.old.clone(), update.new.clone());
                let count = if let Some(count) = counts.get(&range) {
                    *count
                } else {
                    let count = codeyard_git::repository::count_pushed_commits_snapshot(
                        &path,
                        (update.old != ZERO_SHA).then_some(update.old.as_str()),
                        &update.new,
                        &previous,
                    )
                    .await?;
                    let count = i32::try_from(count).unwrap_or(i32::MAX);
                    counts.insert(range, count);
                    count
                };
                codeyard_db_repos::activity::record_repository_push(
                    &self.pool, repo.id, actor_id, branch, count,
                )
                .await?;
            }
            Ok::<_, anyhow::Error>(())
        };
        if !matches!(
            tokio::time::timeout(codeyard_git::repository::PUSH_COUNT_TIMEOUT, activity).await,
            Ok(Ok(()))
        ) {
            tracing::warn!(
                repo_id = repo.id,
                "could not finish optional push activity within its budget"
            );
        }
        // Remember every accepted commit, even when trust, skip messages or filters prevent CI.
        let store = repo_store(&repo, self.ci.cluster.as_ref());
        for update in updates.iter().filter(|update| update.new != ZERO_SHA) {
            match store.commit(&update.new).await {
                Ok(commit) => {
                    codeyard_db_actions::ci::record_ci_accepted_push(
                        &self.pool,
                        repo.id,
                        actor_id,
                        &update.reference,
                        &commit.sha,
                    )
                    .await?;
                }
                Err(_) if update.reference.starts_with("refs/tags/") => {}
                Err(_) => failed = true,
            }
        }
        // Accepted-push facts above are independent of credential liveness.
        let current_repo =
            codeyard_db_repos::repositories::records::get_repository_by_id(&self.pool, repo.id)
                .await?
                .context("repository unavailable")?;
        let current_identity = match operation
            .credential
            .authorize(&self.pool, &current_repo, true)
            .await
        {
            Ok((identity, _)) if identity.id == actor_id => identity,
            _ => {
                return if failed {
                    Err(crate::repositories::git_sessions::PostReceiveFailed.into())
                } else {
                    Ok(())
                }
            }
        };
        ensure!(
            self.bare_path(&current_repo).await? == operation.path,
            "repository location changed"
        );
        if !current_identity.is_admin
            && !self
                .ci
                .config()
                .trusted_actor_ids
                .contains(&current_identity.id)
        {
            ensure!(
                !failed,
                crate::repositories::git_sessions::PostReceiveFailed
            );
            return Ok(());
        }

        let mut match_budget = WorkflowMatchBudget::new();
        'updates: for update in updates {
            if update.reference.starts_with("refs/notes/") {
                continue;
            }
            let revision = if update.new == ZERO_SHA {
                format!("refs/heads/{}", current_repo.default_branch)
            } else {
                update.new.clone()
            };
            let commit_sha = match store.commit(&revision).await {
                Ok(commit) => commit.sha,
                Err(error)
                    if update.reference.starts_with("refs/tags/")
                        && error.kind == ErrorKind::NotFound =>
                {
                    continue
                }
                Err(_) => {
                    failed = true;
                    continue;
                }
            };
            let workflows =
                match crate::ci::workflows::list_workflows_for_ref(&store, &commit_sha).await {
                    Ok(workflows) => workflows,
                    Err(_) => {
                        failed = true;
                        continue;
                    }
                };
            let message = match crate::ci::workflows::commit_message_for_ref(
                &repo_store(&repo, self.ci.cluster.as_ref()),
                &commit_sha,
            )
            .await
            {
                Ok(message) => message,
                Err(_) => {
                    failed = true;
                    continue;
                }
            };
            let ref_update = crate::ci::push::RefUpdate {
                reference: update.reference.clone(),
                old: (update.old != ZERO_SHA).then(|| update.old.clone()),
                new: update.new.clone(),
                changed_paths: None,
            };
            let matched = crate::ci::push::matching_push_workflows(
                &repo_store(&repo, self.ci.cluster.as_ref()),
                &ref_update,
                workflows,
                Some(&message),
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
            if matched.is_empty() {
                continue;
            }
            // A run's github.event is the push document the webhook sends.
            let event = match crate::ci::push::push_document(
                &self.pool,
                &repo_store(&repo, self.ci.cluster.as_ref()),
                &repo,
                &operation.owner,
                actor_id,
                &ref_update,
            )
            .await
            {
                Ok(event) => event,
                Err(error) => {
                    tracing::warn!(repo_id = repo.id, %error, "push document unavailable");
                    failed = true;
                    continue;
                }
            };
            for workflow in matched {
                let pipeline =
                    crate::ci::workflows::infer_pipeline_from_workflow_path(&workflow.path);
                // Recheck live identity/access for each dispatch, in particular
                // CD/full. Neither event=push nor the prior pre-hook is a bypass.
                let fresh_repo = codeyard_db_repos::repositories::records::get_repository_by_id(
                    &self.pool, repo.id,
                )
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
                ensure!(fresh_identity.id == actor_id, "identity changed");
                ensure!(
                    self.bare_path(&fresh_repo).await? == operation.path,
                    "repository location changed"
                );
                let result = self
                    .ci
                    .trigger_event_run(
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
                        event.clone(),
                        None,
                    )
                    .await;
                if matches!(
                    result,
                    Err(codeyard_db_core::ci_admission::AdmissionError::WorkflowDisabled(_))
                ) {
                    continue;
                }
                if let Err(error) = result {
                    tracing::warn!(repo_id = repo.id, %error, "push CI dispatch failed");
                    failed = true;
                }
            }
        }
        ensure!(
            !failed,
            crate::repositories::git_sessions::PostReceiveFailed
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
        let path = path.to_owned();
        let fallback = repo.default_branch.clone();
        let created: Vec<String> = updates
            .iter()
            .filter(|update| update.old == ZERO_SHA)
            .filter_map(|update| update.reference.strip_prefix("refs/heads/"))
            .map(str::to_owned)
            .collect();
        // HEAD selection and its database default must belong to the same push, so only those
        // steps hold the repository lock. The storage walk below runs after it is released.
        {
            let quorum = operation
                .state
                .lock()
                .map_err(|_| anyhow::anyhow!("operation state unavailable"))?
                .quorum;
            let publication = if quorum {
                Some(codeyard_git::locks::publication(&path).await?)
            } else {
                None
            };
            let _lock = if quorum {
                None
            } else {
                Some(codeyard_git::locks::lock(&path).await)
            };
            let head_path = path.clone();
            let held = operation.clone();
            let (branch, head_change) = tokio::task::spawn_blocking(move || {
                let _operation = held;
                let repository = git2::Repository::open_bare(&head_path)?;
                let head = repository.find_reference("HEAD")?;
                let mut branch = head
                    .symbolic_target()
                    .and_then(|name| name.strip_prefix("refs/heads/"))
                    .map(str::to_owned)
                    .unwrap_or(fallback);
                let mut head_change = None;
                // A first push can create only a branch other than the initial HEAD. Git
                // leaves HEAD unborn, so the first pushed branch becomes the default.
                if repository.head().is_err() {
                    if let Some(first) = crate::repositories::reads::preferred_branch(
                        created.iter().map(String::as_str),
                    ) {
                        let target = format!("refs/heads/{first}");
                        if quorum {
                            head_change = Some(codeyard_db_core::models::RefChange {
                                name: "HEAD".into(),
                                kind: "symbolic".into(),
                                old_target: head.symbolic_target().map(str::to_owned),
                                new_target: Some(target),
                            });
                        } else {
                            repository.set_head(&target)?;
                        }
                        branch = first.to_owned();
                    }
                }
                Ok::<_, anyhow::Error>((branch, head_change))
            })
            .await??;
            if let Some(change) =
                head_change.filter(|change| change.old_target != change.new_target)
            {
                let cluster = self.ci.cluster.as_ref().context("cluster unavailable")?;
                let view = codeyard_db_cluster::cluster::writes::recovery_view(&self.pool, repo.id)
                    .await?;
                let uuid = crate::cluster::rpc::repository::repository_uuid(&repo.disk_path)?;
                publication
                    .as_ref()
                    .context("publication lease missing")?
                    .scope(cluster.publish_ref_changes(
                        uuid,
                        &path,
                        repo.id,
                        view.term,
                        &[change],
                        async {
                            self.operation_repository(&operation).await?;
                            Ok(())
                        },
                    ))
                    .await?;
            }
            codeyard_db_repos::repositories::records::update_repo_default_branch(
                &self.pool, repo.id, &branch,
            )
            .await?;
        }
        repo_store(repo, self.ci.cluster.as_ref()).refresh_storage_size(self.pool.clone(), repo.id);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::repositories::git_sessions::broker::HookState;

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
            actor_id: 1,
            repo_id,
            owner: "owner".to_owned(),
            path: PathBuf::new(),
            publication_lease: None,
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
        for index in 0..codeyard_git::MAX_REPOSITORY_REFS - 10 {
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
