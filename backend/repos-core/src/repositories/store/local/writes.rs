use super::*;
use crate::repositories::store::local::begin_coordinated_ref_change;
use crate::repositories::store::local::live_write;
use crate::repositories::store::local::unavailable;
use crate::repositories::store::local::LocalStore;
use crate::repositories::store::local::RefChange;
use codeyard_cluster_core::writes::OutcomeUnknown;
use codeyard_db_core::models::RefChange as WriteRef;
use codeyard_repos::protection::BranchWrite;
use codeyard_repos::protection::History;
use codeyard_repos::protection::WriteVia;

pub(super) fn oid_change(name: &str, old: Option<&str>, new: Option<&str>) -> WriteRef {
    WriteRef {
        name: name.to_owned(),
        kind: "oid".into(),
        old_target: old.filter(|oid| *oid != editor::ZERO).map(str::to_owned),
        new_target: new.filter(|oid| *oid != editor::ZERO).map(str::to_owned),
    }
}

impl LocalStore {
    pub(super) async fn quorum_context(
        &self,
        pool: &codeyard_db_core::DbPool,
        repo: i64,
    ) -> Result<Option<(&crate::context::NodeContext, i64)>, RepoError> {
        let repository = codeyard_db_repos::repositories::records::get_repository_by_id(pool, repo)
            .await
            .map_err(|_| RepoError::authority_unavailable())?
            .ok_or_else(RepoError::write_denied)?;
        if repository.storage_node_id.is_none()
            || codeyard_db_cluster::cluster::writes::write_settings(pool)
                .await
                .map_err(RepoError::from)?
                .write_quorum
                .is_none()
        {
            return Ok(None);
        }
        let (context, id) = self
            .context
            .as_ref()
            .ok_or_else(RepoError::storage_node_unavailable)?;
        if *id != repo {
            return Err(RepoError::storage_node_unavailable());
        }
        self.admitted_path(false).await?;
        let term = self
            .term
            .get()
            .ok_or_else(RepoError::storage_node_unavailable)?
            .0;
        Ok(Some((context, term)))
    }

    pub(super) async fn begin_publisher_change(
        &self,
        pool: &codeyard_db_core::DbPool,
        repo: i64,
    ) -> Result<RefChange, RepoError> {
        self.admitted_path(false).await?;
        begin_coordinated_ref_change(pool, repo, self.path(), self.node_id).await
    }

    /// False leaves the legacy publication unchanged. The caller holds and scopes the lease.
    pub(super) async fn publish_quorum_refs<F>(
        &self,
        pool: &codeyard_db_core::DbPool,
        repo: i64,
        changes: &[WriteRef],
        authorize: F,
    ) -> Result<bool, RepoError>
    where
        F: std::future::Future<Output = Result<(), RepoError>>,
    {
        let Some((context, term)) = self.quorum_context(pool, repo).await? else {
            return Ok(false);
        };
        let changes: Vec<_> = changes
            .iter()
            .filter(|change| change.old_target != change.new_target)
            .cloned()
            .collect();
        if changes.is_empty() {
            authorize.await?;
            return Ok(true);
        }
        let uuid = crate::wire::repository_uuid(&self.disk_path)?;
        context
            .publish_ref_changes(uuid, self.path(), repo, term, &changes, async {
                authorize.await.map_err(anyhow::Error::from)
            })
            .await
            .map_err(|error| {
                if error.is::<OutcomeUnknown>() {
                    RepoError::new(ErrorKind::Unavailable, error.to_string())
                } else {
                    unavailable("write quorum unavailable")
                }
            })?;
        Ok(true)
    }

    pub(super) async fn publish_quorum_head<F>(
        &self,
        pool: &codeyard_db_core::DbPool,
        repo: i64,
        target: &str,
        authorize: F,
    ) -> Result<bool, RepoError>
    where
        F: std::future::Future<Output = Result<(), RepoError>>,
    {
        if self.quorum_context(pool, repo).await?.is_none() {
            return Ok(false);
        }
        let old = self
            .head_target()
            .await?
            .ok_or_else(|| unavailable("Detached HEAD requires recovery"))?;
        let changes = [WriteRef {
            name: "HEAD".into(),
            kind: "symbolic".into(),
            old_target: Some(old),
            new_target: Some(target.to_owned()),
        }];
        self.publish_quorum_refs(pool, repo, &changes, authorize)
            .await
    }

    #[expect(
        clippy::too_many_arguments,
        reason = "The repository, actor, updates and both guards are each needed before the quorum vote."
    )]
    pub(super) async fn prepare_publications<F>(
        &self,
        pool: &codeyard_db_core::DbPool,
        repo: i64,
        actor: git::PublicationActor,
        login: &str,
        updates: &[git::PublicationUpdate],
        publication: &Arc<git::locks::PublicationGuard>,
        change: &Arc<RefChange>,
        authorize: F,
    ) -> Result<(), RepoError>
    where
        F: std::future::Future<Output = Result<(), RepoError>>,
    {
        let quorum = self.quorum_context(pool, repo).await?.is_some();
        let updates = updates.to_vec();
        let login = login.to_owned();
        let held = (publication.clone(), change.clone());
        let changes = self
            .blocking(move |path| {
                let _held = held;
                if quorum {
                    git::plan_branch_publications(path, repo, actor, &login, &updates)
                } else {
                    git::prepare_branch_publications(path, repo, actor, &login, &updates)?;
                    Ok(Vec::new())
                }
            })
            .await?;
        if !changes.is_empty() {
            let changes: Vec<_> = changes
                .iter()
                .map(|change| {
                    oid_change(
                        &change.reference,
                        change.old.as_deref(),
                        change.new.as_deref(),
                    )
                })
                .collect();
            publication
                .scope(self.publish_quorum_refs(pool, repo, &changes, authorize))
                .await?;
        }
        Ok(())
    }

    pub(super) async fn check_ref_publication(
        &self,
        pool: &codeyard_db_core::DbPool,
        repo: i64,
        authority: &codeyard_db_core::queries::credentials::Actor,
        reference: &str,
        old: Option<&str>,
        new: Option<&str>,
    ) -> Result<(), RepoError> {
        let write = reference
            .strip_prefix("refs/heads/")
            .map(|branch| BranchWrite {
                branch,
                old,
                new,
                via: WriteVia::Direct,
            });
        let repository = live_write(
            pool,
            repo,
            authority.user_id,
            write.as_ref(),
            &History::Stored {
                repository: self.path(),
            },
            self.path(),
            self.node_id,
        )
        .await?;
        crate::repositories::access::check_publication_actor(pool, authority, &repository).await?;
        if reference.starts_with("refs/tags/") {
            crate::repositories::releases::immutable::check_current_tag_updates(
                pool,
                &repository,
                &[codeyard_repos::validation::RefUpdate {
                    reference: reference.to_owned(),
                    old: old.unwrap_or(editor::ZERO).to_owned(),
                    new: new.unwrap_or(editor::ZERO).to_owned(),
                }],
            )
            .await?;
        }
        Ok(())
    }

    pub(super) async fn verify_merge_heads(
        &self,
        heads: &[(&str, &str)],
        base_branch: Option<&str>,
    ) -> Result<(), RepoError> {
        let base_ref = base_branch
            .map(|branch| {
                git::branch_ref(branch).ok_or_else(|| codeyard_git::error::invalid_ref("branch"))
            })
            .transpose()?;
        let heads = heads
            .iter()
            .map(|(name, target)| {
                let name = git::merge_head_ref(name, target).map_err(RepoError::from)?;
                if base_ref.as_deref() == Some(name.as_str()) {
                    return Err(codeyard_git::error::invalid_ref("merge head"));
                }
                Ok((name, (*target).to_owned()))
            })
            .collect::<Result<Vec<_>, RepoError>>()?;
        self.blocking(move |path| {
            let repo = git2::Repository::open_bare(path)?;
            for (name, target) in heads {
                anyhow::ensure!(
                    repo.find_reference(&name)?
                        .target()
                        .map(|oid| oid.to_string())
                        .as_deref()
                        == Some(target.as_str()),
                    git::RefConflict("Merge head changed".into())
                );
            }
            Ok(())
        })
        .await
    }

    /// Staged publishers import objects only, never refs, before asking stores to vote.
    pub(super) async fn import_prepared_objects(
        &self,
        pool: &codeyard_db_core::DbPool,
        repo: i64,
        source: &Path,
        targets: &[String],
    ) -> Result<(), RepoError> {
        if source != self.path() && !targets.is_empty() {
            if let Some((context, _)) = self.quorum_context(pool, repo).await? {
                context
                    .import_write_objects(source, self.path(), targets)
                    .await
                    .map_err(RepoError::from)?;
            }
        }
        Ok(())
    }
}

impl LocalStore {
    /// An empty canonical HEAD has no objects to transfer. Each voter creates and verifies
    /// its own term before the database enables serving; copies publish their refs later.
    pub(super) async fn initialize_quorum(
        &self,
        context: &crate::context::NodeContext,
        repository: i64,
        default_branch: &str,
    ) -> Result<(), RepoError> {
        use codeyard_db_cluster::cluster::writes as journal;
        use codeyard_db_core::models::RefValue;
        let reference = git::branch_ref(default_branch)
            .ok_or_else(|| codeyard_git::error::invalid_ref("branch"))?;
        let pool = context.database()?;
        let row = codeyard_db_repos::repositories::records::get_repository_by_id(pool, repository)
            .await
            .map_err(RepoError::from)?
            .ok_or_else(RepoError::write_denied)?;
        let primary = row
            .storage_node_id
            .ok_or_else(RepoError::storage_node_unavailable)?;
        let uuid = crate::wire::repository_uuid(&row.disk_path)?;
        let nodes = codeyard_db_cluster::cluster::list_nodes(pool)
            .await
            .map_err(RepoError::from)?;
        if !nodes.iter().any(|node| {
            node.id == context.node_id
                && node.public_key == context.key.public_hex()
                && node.is_serving_storage()
        }) {
            return Err(RepoError::storage_node_unavailable());
        }
        let other = if primary == context.node_id {
            if codeyard_db_cluster::cluster::get_write_version(pool, repository)
                .await
                .map_err(RepoError::from)?
                != Some(0)
            {
                return Err(RepoError::write_denied());
            }
            let other = codeyard_db_core::models::ClusterNode::ranked_storage(&nodes)
                .into_iter()
                .find(|node| node.id != primary)
                .ok_or_else(RepoError::storage_node_unavailable)?
                .id;
            journal::bootstrap_writes(
                pool,
                repository,
                primary,
                &[primary, other],
                &[RefValue {
                    name: "HEAD".into(),
                    kind: "symbolic".into(),
                    target: reference.clone(),
                }],
            )
            .await
            .map_err(RepoError::from)?;
            Some(other)
        } else {
            None
        };
        let view = journal::recovery_view(pool, repository)
            .await
            .map_err(RepoError::from)?;
        if view.serving
            || view.term != 1
            || view.committed_version != 0
            || view.unfinished.is_some()
            || view.refs.len() != 1
            || view.refs[0].name != "HEAD"
            || view.refs[0].kind != "symbolic"
            || view.refs[0].target != reference
            || !view
                .voters
                .iter()
                .any(|voter| voter.node_id == context.node_id)
        {
            return Err(RepoError::storage_node_unavailable());
        }
        let baseline = crate::repositories::store::primary_path(&context.repos_path, uuid);
        let _lock = git::locks::lock(&baseline).await;
        if !tokio::fs::try_exists(&baseline)
            .await
            .map_err(|error| RepoError::from(anyhow::Error::from(error)))?
        {
            git::initialize_repository(&baseline, None, default_branch)
                .await
                .map_err(RepoError::from)?;
        }
        super::recover_term(pool, repository, context.node_id, &context.repos_path)
            .await
            .map_err(RepoError::from)?;
        if let Some(other) = other {
            let (reply, _) = Box::pin(context.call(
                other,
                crate::wire::Request {
                    repository: uuid,
                    operation: crate::wire::Operation::Initialize {
                        source: None,
                        default_branch: default_branch.into(),
                    },
                    actor: None,
                    ci_run: None,
                },
                b"",
            ))
            .await?;
            reply.result.map_err(|failure| match failure {
                crate::wire::Failure::Repository(error) => error,
                _ => RepoError::storage_node_unavailable(),
            })?;
            if !journal::enable_initialized_repository(pool, repository, primary)
                .await
                .map_err(RepoError::from)?
            {
                return Err(RepoError::storage_node_unavailable());
            }
        }
        Ok(())
    }

    #[expect(
        clippy::too_many_arguments,
        reason = "Copy publication retains both repositories and the initiating credential."
    )]
    pub(super) async fn publish_quorum_copy(
        &self,
        pool: &codeyard_db_core::DbPool,
        repository: i64,
        source_id: i64,
        authority: &codeyard_db_core::queries::credentials::Actor,
        source: &LocalStore,
        staged: &LocalStore,
        refs: &std::collections::HashMap<String, String>,
    ) -> Result<(), RepoError> {
        use tokio::io::AsyncWriteExt;
        // Only objects move before authorization. No copied reference is visible yet.
        let pack = tempfile::NamedTempFile::new()
            .map_err(|error| RepoError::from(anyhow::Error::from(error)))?;
        let mut output = tokio::fs::File::from_std(
            pack.reopen()
                .map_err(|error| RepoError::from(anyhow::Error::from(error)))?,
        );
        let limits = crate::git_transport::WriteLimits::load(pool)
            .await
            .map_err(RepoError::from)?;
        let targets: Vec<_> = refs.values().cloned().collect();
        let status = staged
            .replica_pack(
                &targets,
                &[],
                crate::store_types::GitStreams {
                    input: &mut tokio::io::empty(),
                    output: &mut crate::git_transport::LimitedWriter::new(
                        &mut output,
                        limits.pack_bytes,
                    ),
                    errors: &mut tokio::io::sink(),
                },
            )
            .await
            .map_err(RepoError::from)?;
        if !status.success() {
            return Err(unavailable("Repository copy pack failed"));
        }
        output
            .flush()
            .await
            .map_err(|error| RepoError::from(anyhow::Error::from(error)))?;
        self.index_pack(pack.path(), true).await?;
        let changes: Vec<_> = refs
            .iter()
            .map(|(name, target)| oid_change(name, None, Some(target)))
            .collect();
        let publication = git::locks::publication(self.path())
            .await
            .map_err(RepoError::from)?;
        publication
            .scope(self.publish_quorum_refs(pool, repository, &changes, async {
                super::live_read(
                    pool,
                    source_id,
                    authority.user_id,
                    source.path(),
                    source.node_id,
                )
                .await?;
                if let Some(token) = authority.token_id() {
                    let row =
                        codeyard_identity::repository_access::current_repository(pool, source_id)
                            .await
                            .map_err(|_| RepoError::authority_unavailable())?;
                    crate::repositories::git_sessions::ReceiveCredential::Token(token)
                        .authorize(pool, &row, false)
                        .await
                        .map_err(RepoError::from)?;
                }
                let destination =
                    codeyard_identity::repository_access::current_repository(pool, repository)
                        .await
                        .map_err(|_| RepoError::authority_unavailable())?;
                if destination.is_gist {
                    if destination.owner_user_id != Some(authority.user_id)
                        || destination.is_archived
                    {
                        return Err(RepoError::write_denied());
                    }
                } else {
                    super::live_write(
                        pool,
                        repository,
                        authority.user_id,
                        None,
                        &History::SingleCommit,
                        self.path(),
                        self.node_id,
                    )
                    .await?;
                }
                codeyard_identity::repository_access::credential_user(pool, authority, true)
                    .await
                    .map_err(RepoError::from)?;
                Ok(())
            }))
            .await?;
        Ok(())
    }
}
