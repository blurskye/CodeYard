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
