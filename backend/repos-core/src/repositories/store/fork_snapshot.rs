//! Fork copies export only visible branches and tags, under the original credential.
use super::{Backend, RepoStore};
use crate::store_types::GitStreams;
use anyhow::{ensure, Context, Result};
use codeyard_db_core::{cells::repositories::Snapshot, queries::credentials::Actor, DbPool};

pub fn pack_bytes() -> Result<u64> {
    let maximum = match std::env::var("FORK_MAX_PACK_BYTES") {
        Ok(value) => value
            .parse::<u64>()
            .context("Invalid FORK_MAX_PACK_BYTES")?,
        Err(std::env::VarError::NotPresent) => 128 * 1024 * 1024,
        Err(error) => return Err(error.into()),
    };
    ensure!(
        (1..=64 * 1024 * 1024 * 1024).contains(&maximum),
        "Invalid FORK_MAX_PACK_BYTES"
    );
    Ok(maximum)
}

impl RepoStore {
    pub async fn fork_listing(
        &self,
        pool: &DbPool,
        actor: &Actor,
        repository: i64,
        default_branch_only: bool,
    ) -> Result<Snapshot> {
        ensure!(
            actor.credential.is_some(),
            "Fork snapshot requires its original credential"
        );
        let source =
            codeyard_db_repos::repositories::creation::read_fork_source(pool, actor, repository)
                .await?
                .context("Fork source is unavailable")?;
        ensure!(
            crate::wire::repository_uuid(&source.disk_path)?
                == crate::wire::repository_uuid(self.location())?,
            "Fork snapshot store has another repository"
        );
        let mut refs = self.refs().await?;
        refs.retain(|name, _| {
            if default_branch_only {
                name == &format!("refs/heads/{}", source.default_branch)
            } else {
                name.starts_with("refs/heads/") || name.starts_with("refs/tags/")
            }
        });
        Ok(Snapshot {
            repository_id: repository,
            reader_id: actor.user_id,
            default_branch: source.default_branch,
            refs,
        })
    }
    pub async fn fork_snapshot(
        &self,
        pool: &DbPool,
        actor: &Actor,
        repository: i64,
        default_branch_only: bool,
        streams: GitStreams<'_>,
    ) -> Result<Snapshot> {
        if let Backend::Remote(remote) = &self.0 {
            return remote
                .fork_snapshot(actor, repository, default_branch_only, streams)
                .await;
        }
        let snapshot = self
            .fork_listing(pool, actor, repository, default_branch_only)
            .await?;
        let targets: Vec<_> = snapshot.refs.values().cloned().collect();
        let mut bounded = crate::git_transport::LimitedWriter::new(streams.output, pack_bytes()?);
        let status = self
            .replica_pack(
                &targets,
                &[],
                GitStreams {
                    input: streams.input,
                    output: &mut bounded,
                    errors: streams.errors,
                },
            )
            .await?;
        ensure!(status.success(), "Fork snapshot pack failed");
        codeyard_db_repos::repositories::creation::read_fork_source(pool, actor, repository)
            .await?
            .context("Fork source access was revoked")?;
        Ok(snapshot)
    }
}
