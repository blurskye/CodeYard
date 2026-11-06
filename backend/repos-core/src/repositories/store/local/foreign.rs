//! A foreign fork stages fetched objects privately and uses the normal copy publisher.
use super::*;
use crate::repositories::store::{fork_pack_bytes, CopyOptions, RepoStore};
use codeyard_db_core::cells::repositories::Admission;
use tokio::io::AsyncWriteExt;

impl LocalStore {
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
    let mut request = admission
        .snapshot_request(
            default_branch_only,
            fork_pack_bytes().map_err(RepoError::from)?,
        )
        .map_err(RepoError::from)?;
    request.branch = branch.map(str::to_owned);
    request.validate().map_err(RepoError::from)?;
    let provider = pool
        .foreign_authorization()
        .ok_or_else(RepoError::authority_unavailable)?;
    let snapshot = provider
        .snapshot(&request, &mut output)
        .await
        .map_err(RepoError::from)?;
    snapshot.validate(&request).map_err(RepoError::from)?;
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
    Ok((temporary, snapshot))
}
