//! A foreign fork stages fetched objects privately and uses the normal copy publisher.
use super::*;
use crate::repositories::store::{fork_pack_bytes, CopyOptions, RepoStore};
use codeyard_db_core::cells::{pulls::HeadRequest, repositories::Admission};
use tokio::io::AsyncWriteExt;

impl LocalStore {
    pub(super) async fn foreign_pull_base(
        &self,
        pool: &codeyard_db_core::DbPool,
        repository: i64,
        admission: &Admission,
    ) -> Result<codeyard_db_core::models::Repository, RepoError> {
        let source = admission.read(pool).await.map_err(RepoError::from)?;
        if source.repository.forked_from_id != Some(repository)
            || source.repository.id == repository
        {
            return Err(RepoError::write_denied());
        }
        self.admitted_path(true).await?;
        let base = codeyard_db_repos::repositories::creation::read_repository_source(
            pool,
            &admission.actor,
            repository,
        )
        .await
        .map_err(RepoError::from)?
        .ok_or_else(RepoError::write_denied)?;
        if !repository_matches_path(&base, self.path(), self.node_id) {
            return Err(RepoError::write_denied());
        }
        foreign_ingress(pool, admission).await?;
        Ok(base)
    }
    pub(in crate::repositories::store) async fn compare_foreign_pull_head(
        &self,
        pool: &codeyard_db_core::DbPool,
        repository: i64,
        admission: &Admission,
        base: &str,
        branch: &str,
    ) -> Result<(String, codeyard_git::CommitComparison), RepoError> {
        tokio::time::timeout(crate::wire::TIMEOUT, async {
            self.foreign_pull_base(pool, repository, admission).await?;
            let parent = self
                .path()
                .parent()
                .ok_or_else(|| unavailable("Invalid repository path"))?;
            let (temporary, snapshot) = stage(parent, pool, admission, false, Some(branch)).await?;
            let head = snapshot
                .refs
                .get(&format!("refs/heads/{branch}"))
                .ok_or_else(|| RepoError::new(ErrorKind::NotFound, "Head branch not found"))?
                .clone();
            let staged = RepoStore::from_disk_path(
                temporary
                    .path()
                    .to_str()
                    .ok_or_else(|| unavailable("Invalid copy path"))?,
            );
            let comparison = self
                .comparison_from(staged.local()?, base, &head, 0, 0)
                .await?;
            self.foreign_pull_base(pool, repository, admission).await?;
            Ok((head, comparison))
        })
        .await
        .map_err(|_| RepoError::storage_node_unavailable())?
    }
    #[expect(
        clippy::too_many_arguments,
        reason = "Foreign head publication carries the retained proof, branch, pull and CAS revision."
    )]
    pub(in crate::repositories::store) async fn fetch_foreign_pull_head(
        &self,
        pool: &codeyard_db_core::DbPool,
        repository: i64,
        admission: &Admission,
        number: i32,
        branch: &str,
        head: &str,
        expected_head: Option<&str>,
    ) -> Result<(), RepoError> {
        tokio::time::timeout(crate::wire::TIMEOUT, async {
            self.foreign_pull_base(pool, repository, admission).await?;
            let source = PullHeadSource::Foreign {
                admission,
                branch,
                number,
            };
            let actor = crate::store_types::PullHeadActor {
                user: admission.actor.user_id,
                token: admission.actor.token_id(),
            };
            source
                .check(pool, repository, admission.source.repository.id, actor)
                .await?;
            let current = git::repository::current_pull_head(self.path(), number)
                .await
                .map_err(RepoError::from)?;
            if current.as_deref() == Some(head) {
                return Ok(());
            }
            if current.as_deref() != expected_head {
                return Err(RepoError::from(anyhow::Error::new(
                    git::repository::PullHeadConflict,
                )));
            }
            let parent = self
                .path()
                .parent()
                .ok_or_else(|| unavailable("Invalid repository path"))?;
            let (temporary, snapshot) = stage(parent, pool, admission, false, Some(branch)).await?;
            if snapshot
                .refs
                .get(&format!("refs/heads/{branch}"))
                .map(String::as_str)
                != Some(head)
            {
                return Err(RepoError::from(anyhow::Error::new(
                    git::repository::PullHeadConflict,
                )));
            }
            let prepared = git::repository::prepare_pull_head(self.path(), temporary.path(), head)
                .await
                .map_err(RepoError::from)?;
            self.publish_prepared_pull_head(
                pool,
                repository,
                admission.source.repository.id,
                actor,
                source,
                number,
                head,
                expected_head,
                prepared,
            )
            .await
        })
        .await
        .map_err(|_| RepoError::storage_node_unavailable())?
    }
    pub(in crate::repositories::store) async fn follow_foreign_pull_head(
        &self,
        pool: &codeyard_db_core::DbPool,
        repository: i64,
        request: &HeadRequest,
        expected_head: Option<&str>,
    ) -> Result<Option<String>, RepoError> {
        tokio::time::timeout(crate::wire::TIMEOUT, async {
            self.admitted_path(true).await?;
            let base =
                codeyard_db_repos::repositories::records::get_repository_by_id(pool, repository)
                    .await
                    .map_err(|_| RepoError::authority_unavailable())?
                    .ok_or_else(RepoError::write_denied)?;
            if base.is_stub || !repository_matches_path(&base, self.path(), self.node_id) {
                return Err(RepoError::write_denied());
            }
            let binding = &request.binding;
            let source = PullHeadSource::Followed(binding);
            let actor = crate::store_types::PullHeadActor {
                user: request.publisher_id,
                token: None,
            };
            source
                .check(pool, repository, binding.head_repo_id, actor)
                .await?;
            let parent = self
                .path()
                .parent()
                .ok_or_else(|| unavailable("Invalid repository path"))?;
            let Some((temporary, snapshot)) =
                stage_export(parent, pool, Export::Pull(request)).await?
            else {
                return Ok(None);
            };
            let Some(head) = snapshot
                .refs
                .get(&format!("refs/heads/{}", binding.head_branch))
                .cloned()
            else {
                return Ok(None);
            };
            let current = git::repository::current_pull_head(self.path(), binding.number)
                .await
                .map_err(RepoError::from)?;
            if current.as_deref() == Some(head.as_str()) {
                return Ok(Some(head));
            }
            if current.as_deref() != expected_head {
                return Err(RepoError::from(anyhow::Error::new(
                    git::repository::PullHeadConflict,
                )));
            }
            let prepared = git::repository::prepare_pull_head(self.path(), temporary.path(), &head)
                .await
                .map_err(RepoError::from)?;
            self.publish_prepared_pull_head(
                pool,
                repository,
                binding.head_repo_id,
                actor,
                source,
                binding.number,
                &head,
                expected_head,
                prepared,
            )
            .await?;
            Ok(Some(head))
        })
        .await
        .map_err(|_| RepoError::storage_node_unavailable())?
    }
    pub(in crate::repositories::store) async fn initialize_foreign_copy(
        &self,
        pool: &codeyard_db_core::DbPool,
        repository: i64,
        admission: &Admission,
        options: &CopyOptions,
    ) -> Result<(), RepoError> {
        tokio::time::timeout(crate::wire::TIMEOUT, async {
            if options.template {
                return Err(RepoError::write_denied());
            }
            CopySource::Foreign(admission)
                .check(pool, &admission.actor, repository)
                .await?;
            live_write(
                pool,
                repository,
                admission.actor.user_id,
                None,
                &History::SingleCommit,
                self.path(),
                self.node_id,
            )
            .await?;
            access::credential_user(pool, &admission.actor, true)
                .await
                .map_err(RepoError::from)?;
            if let Some((context, id)) = &self.context {
                if codeyard_db_cluster::cluster::writes::write_settings(pool)
                    .await
                    .map_err(RepoError::from)?
                    .write_quorum
                    .is_some()
                {
                    self.initialize_quorum(context, *id, &options.default_branch)
                        .await?;
                    self.admitted_path(false).await?;
                }
            }
            let parent = self
                .path()
                .parent()
                .ok_or_else(|| unavailable("Invalid copy path"))?;
            let (temporary, snapshot) =
                stage(parent, pool, admission, options.default_branch_only, None).await?;
            if snapshot.default_branch != options.default_branch {
                return Err(RepoError::write_denied());
            }
            self.publish_copy(
                pool,
                repository,
                admission.actor.clone(),
                CopySource::Foreign(admission),
                temporary,
                &snapshot.default_branch,
            )
            .await
        })
        .await
        .map_err(|_| RepoError::storage_node_unavailable())?
    }
}

/// Fork copies and pull heads share one bounded, private snapshot preparation path.
pub(super) async fn stage(
    parent: &Path,
    pool: &codeyard_db_core::DbPool,
    admission: &Admission,
    default_branch_only: bool,
    branch: Option<&str>,
) -> Result<
    (
        tempfile::TempDir,
        codeyard_db_core::cells::repositories::Snapshot,
    ),
    RepoError,
> {
    let mut request = admission
        .snapshot_request(
            default_branch_only,
            fork_pack_bytes().map_err(RepoError::from)?,
        )
        .map_err(RepoError::from)?;
    request.branch = branch.map(str::to_owned);
    stage_export(parent, pool, Export::Fork(&request))
        .await?
        .ok_or_else(RepoError::authority_unavailable)
}

pub(super) enum Export<'a> {
    Fork(&'a codeyard_db_core::cells::repositories::SnapshotRequest),
    Pull(&'a HeadRequest),
}

async fn stage_export(
    parent: &Path,
    pool: &codeyard_db_core::DbPool,
    export: Export<'_>,
) -> Result<
    Option<(
        tempfile::TempDir,
        codeyard_db_core::cells::repositories::Snapshot,
    )>,
    RepoError,
> {
    let temporary = tempfile::Builder::new()
        .prefix(".fork-")
        .tempdir_in(parent)
        .map_err(|error| RepoError::from(anyhow::Error::from(error)))?;
    let pack = tempfile::NamedTempFile::new_in(parent)
        .map_err(|error| RepoError::from(anyhow::Error::from(error)))?;
    let mut output = tokio::fs::File::from_std(
        pack.reopen()
            .map_err(|error| RepoError::from(anyhow::Error::from(error)))?,
    );
    let provider = pool
        .foreign_authorization()
        .ok_or_else(RepoError::authority_unavailable)?;
    let snapshot = match export {
        Export::Fork(request) => {
            request.validate().map_err(RepoError::from)?;
            let snapshot = provider
                .snapshot(request, &mut output)
                .await
                .map_err(RepoError::from)?;
            snapshot.validate(request).map_err(RepoError::from)?;
            snapshot
        }
        Export::Pull(request) => {
            request.validate().map_err(RepoError::from)?;
            let Some(snapshot) = provider
                .pull_snapshot(request, &mut output)
                .await
                .map_err(RepoError::from)?
            else {
                return Ok(None);
            };
            request
                .validate_snapshot(&snapshot)
                .map_err(RepoError::from)?;
            snapshot
        }
    };
    output
        .flush()
        .await
        .map_err(|error| RepoError::from(anyhow::Error::from(error)))?;
    output
        .sync_all()
        .await
        .map_err(|error| RepoError::from(anyhow::Error::from(error)))?;
    drop(output);
    let staged = RepoStore::from_disk_path(
        temporary
            .path()
            .to_str()
            .ok_or_else(|| unavailable("Invalid copy path"))?,
    );
    staged.initialize(None, &snapshot.default_branch).await?;
    staged.index_pack(pack.path(), true).await?;
    staged.set_refs(&snapshot.refs).await?;
    Ok(Some((temporary, snapshot)))
}
