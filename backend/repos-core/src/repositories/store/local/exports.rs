use super::*;
use crate::repositories::store::local::live_read;
use crate::repositories::store::local::unavailable;
use crate::repositories::store::local::LocalStore;

impl LocalStore {
    pub(crate) async fn lock_migration(
        &self,
        pool: &codeyard_db_core::DbPool,
        repo: i64,
        actor: i64,
        migration: i64,
    ) -> Result<(), RepoError> {
        let _lock = git::locks::lock(self.path()).await;
        live_read(pool, repo, actor, self.path(), self.node_id).await?;
        codeyard_db_repos::repository_exports::lock_repository(pool, actor, migration, repo)
            .await
            .map_err(|_| RepoError::write_denied())
    }

    pub(crate) async fn migration_archive(
        &self,
        pool: &codeyard_db_core::DbPool,
        repo: i64,
        actor: i64,
    ) -> Result<Vec<u8>, RepoError> {
        let _lock = git::locks::lock(self.path()).await;
        live_read(pool, repo, actor, self.path(), self.node_id).await?;
        access::check_admin(pool, repo, actor)
            .await
            .map_err(|_| RepoError::write_denied())?;
        let refs = self.mirror_refs().await?;
        let head = self.head_target().await?;
        let mut tips: Vec<_> = refs.values().cloned().collect();
        tips.sort_unstable();
        tips.dedup();
        let temporary = tokio::task::spawn_blocking(|| {
            let temporary = tempfile::tempdir()?;
            git2::Repository::init_bare(temporary.path().join("repository.git"))?;
            anyhow::Ok(temporary)
        })
        .await
        .map_err(|_| unavailable("Migration storage unavailable"))?
        .map_err(|_| unavailable("Migration storage unavailable"))?;
        let directory = temporary.path().join("repository.git");
        let target = LocalStore::new(
            directory
                .to_str()
                .ok_or_else(|| unavailable("Migration storage unavailable"))?,
            None,
        );
        if !tips.is_empty() {
            let input = format!("{}\n", tips.join("\n"));
            let mut command = process::git_command().map_err(RepoError::from)?;
            command
                .arg("--git-dir")
                .arg(self.path())
                .args(["pack-objects", "--revs", "--stdout"]);
            let (status, pack) = process::buffered(
                command,
                input.as_bytes(),
                codeyard_repos::imports::MAX_IMPORT_PACK_BYTES,
                process::RPC_TIMEOUT,
            )
            .await
            .map_err(RepoError::from)?;
            if !status.success() {
                return Err(unavailable("Migration Git export failed"));
            }
            let file = temporary.path().join("repository.pack");
            tokio::fs::write(&file, &pack)
                .await
                .map_err(|_| unavailable("Migration storage unavailable"))?;
            drop(pack);
            target.index_pack(&file, false).await?;
        }
        target.set_refs(&refs).await?;
        if let Some(head) = head {
            target.set_head_target(&head).await?;
        }
        tokio::task::spawn_blocking(move || -> anyhow::Result<Vec<u8>> {
            let encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
            let mut archive = tar::Builder::new(encoder);
            for name in ["HEAD", "config"] {
                archive.append_path_with_name(directory.join(name), name)?;
            }
            for name in ["objects", "refs"] {
                archive.append_dir_all(name, directory.join(name))?;
            }
            let bytes = archive.into_inner()?.finish()?;
            anyhow::ensure!(
                bytes.len() <= codeyard_support::object_limits::MAX_OBJECT_BYTES as usize,
                "Migration archive too large"
            );
            drop(temporary);
            Ok(bytes)
        })
        .await
        .map_err(|_| unavailable("Migration archive failed"))?
        .map_err(|_| unavailable("Migration archive failed"))
    }
}
