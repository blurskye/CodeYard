//! Fork copies export only visible branches and tags, under the original credential.
use super::{Backend, RepoStore};
use crate::store_types::GitStreams;
use anyhow::{ensure, Context, Result};
use codeyard_database::{cells::repositories::Snapshot, queries::credentials::Actor, DbPool};

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
        branch: Option<&str>,
    ) -> Result<Snapshot> {
        ensure!(
            branch.is_none_or(
                |branch| !default_branch_only && codeyard_git::branch_ref(branch).is_some()
            ),
            "Invalid snapshot branch selection"
        );
        ensure!(
            actor.credential.is_some(),
            "Fork snapshot requires its original credential"
        );
        self.listing(pool, actor, repository, default_branch_only, branch)
            .await
    }
    /// A pull head export names one branch and reads it as the publisher its head cell
    /// recorded when the pull request was opened with that publisher's credential.
    pub async fn pull_listing(
        &self,
        pool: &DbPool,
        publisher: i64,
        repository: i64,
        branch: &str,
    ) -> Result<Snapshot> {
        ensure!(
            codeyard_git::branch_ref(branch).is_some(),
            "Invalid snapshot branch selection"
        );
        self.listing(
            pool,
            &Actor::from(publisher),
            repository,
            false,
            Some(branch),
        )
        .await
    }
    async fn listing(
        &self,
        pool: &DbPool,
        actor: &Actor,
        repository: i64,
        default_branch_only: bool,
        branch: Option<&str>,
    ) -> Result<Snapshot> {
        let source = codeyard_db_repos::repositories::creation::read_repository_source(
            pool, actor, repository,
        )
        .await?
        .context("Fork source is unavailable")?;
        ensure!(
            crate::wire::repository_uuid(&source.disk_path)?
                == crate::wire::repository_uuid(self.location())?,
            "Fork snapshot store has another repository"
        );
        let mut refs = self.refs().await?;
        refs.retain(|name, _| {
            if let Some(branch) = branch {
                name == &format!("refs/heads/{branch}")
            } else if default_branch_only {
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
        branch: Option<&str>,
        streams: GitStreams<'_>,
    ) -> Result<Snapshot> {
        ensure!(
            actor.credential.is_some(),
            "Fork snapshot requires its original credential"
        );
        self.export(
            pool,
            actor,
            repository,
            default_branch_only,
            branch,
            streams,
        )
        .await
    }
    pub async fn pull_snapshot(
        &self,
        pool: &DbPool,
        publisher: i64,
        repository: i64,
        branch: &str,
        streams: GitStreams<'_>,
    ) -> Result<Snapshot> {
        ensure!(
            codeyard_git::branch_ref(branch).is_some(),
            "Invalid snapshot branch selection"
        );
        self.export(
            pool,
            &Actor::from(publisher),
            repository,
            false,
            Some(branch),
            streams,
        )
        .await
    }
    async fn export(
        &self,
        pool: &DbPool,
        actor: &Actor,
        repository: i64,
        default_branch_only: bool,
        branch: Option<&str>,
        streams: GitStreams<'_>,
    ) -> Result<Snapshot> {
        if let Backend::Remote(remote) = &self.0 {
            return remote
                .fork_snapshot(actor, repository, default_branch_only, branch, streams)
                .await;
        }
        let snapshot = self
            .listing(pool, actor, repository, default_branch_only, branch)
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
        codeyard_db_repos::repositories::creation::read_repository_source(pool, actor, repository)
            .await?
            .context("Fork source access was revoked")?;
        Ok(snapshot)
    }
}
