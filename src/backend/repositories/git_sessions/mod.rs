//! Shared Git sessions and live receive-hook authority for every transport.
use crate::backend::ci::CiRuns;
use crate::backend::db::DbPool;
use anyhow::Result;
use std::future::Future;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use tokio::task::JoinSet;

#[cfg(target_os = "linux")]
mod broker;
#[cfg(target_os = "linux")]
mod hooks;
#[cfg(target_os = "linux")]
pub(crate) mod private_files;
#[cfg(target_os = "linux")]
pub(crate) mod protocol;
pub(crate) mod validation;

/// Serves until `stop` is set. After that only hooks of pushes already running are served,
/// and other requests are refused until the connections in progress finish. The caller
/// bounds that wait.
pub async fn run_gateway(
    socket: PathBuf,
    repos_root: PathBuf,
    pool: DbPool,
    ci: Arc<CiRuns>,
    git_user: String,
    stop: tokio::sync::watch::Receiver<bool>,
) -> Result<()> {
    #[cfg(target_os = "linux")]
    {
        broker::run(socket, repos_root, pool, ci, git_user, stop).await
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (socket, repos_root, pool, ci, git_user, stop);
        anyhow::bail!("OpenSSH gateway is currently available only on Linux Unix sockets")
    }
}

/// Git tasks outlive their connections, but not their server's shutdown drain.
pub(crate) struct GitTasks(Mutex<Option<JoinSet<()>>>);

impl Default for GitTasks {
    fn default() -> Self {
        Self(Mutex::new(Some(JoinSet::new())))
    }
}

impl GitTasks {
    pub(crate) fn spawn(&self, task: impl Future<Output = ()> + Send + 'static) -> bool {
        let mut tasks = self.0.lock().unwrap_or_else(|error| error.into_inner());
        let Some(tasks) = tasks.as_mut() else {
            return false;
        };
        while tasks.try_join_next().is_some() {}
        if tasks.len() >= 2 * crate::backend::git::repository::process::session_capacity() {
            return false;
        }
        tasks.spawn(task);
        true
    }

    pub(crate) async fn drain(&self) {
        // Taking the set stops admission. Dropping this drain at the server's
        // deadline aborts unfinished tasks rather than detaching them again.
        let tasks = self
            .0
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .take();
        if let Some(mut tasks) = tasks {
            while tasks.join_next().await.is_some() {}
        }
    }
}

#[cfg(test)]
mod task_tests {
    use super::*;

    #[tokio::test]
    async fn detached_git_tasks_have_a_lifetime_bound() {
        let tasks = GitTasks::default();
        for _ in 0..2 * crate::backend::git::repository::process::session_capacity() {
            assert!(tasks.spawn(std::future::pending()));
        }
        assert!(!tasks.spawn(async {}));
        // Dropping the owner aborts the pending work.
    }
}

#[derive(Debug, thiserror::Error)]
#[error("Git writes were accepted but post-push bookkeeping or CI dispatch failed")]
pub(crate) struct PostReceiveFailed;

/// The credential is rechecked by the receive hooks, not just at HTTP/SSH ingress.
#[derive(Clone, Copy, Debug, serde::Serialize, serde::Deserialize)]
pub enum ReceiveCredential {
    SshKey(i64),
    DeployKey(i64),
    Token(i64),
}

impl ReceiveCredential {
    /// Rechecks the live key or token and the repository's current access policy.
    pub(crate) async fn authorize(
        self,
        pool: &DbPool,
        repo: &crate::backend::db::models::Repository,
        write: bool,
    ) -> Result<(
        crate::backend::db::models::User,
        crate::backend::db::models::AccessLevel,
    )> {
        use crate::backend::{
            db,
            repositories::{access, deploy_keys},
        };
        let identity = match self {
            Self::SshKey(id) => {
                anyhow::ensure!(id > 0, "invalid key ID");
                let identity = db::users::ssh_keys::get_user_by_ssh_key_id(pool, id)
                    .await?
                    .ok_or_else(|| anyhow::anyhow!("key unavailable"))?;
                anyhow::ensure!(
                    !identity.user.is_banned && !identity.user.must_change_password,
                    "identity unavailable"
                );
                let canonical =
                    validation::canonical_db_key(&identity.key.public_key, &identity.key.key_type)?;
                anyhow::ensure!(
                    db::users::ssh_keys::calculate_fingerprint(&canonical)?
                        == identity.key.fingerprint,
                    "stored key fingerprint mismatch"
                );
                identity.user
            }
            Self::DeployKey(id) => return deploy_keys::authorize(pool, id, repo, write).await,
            Self::Token(id) => db::users::tokens::token_user(pool, id, write)
                .await?
                .ok_or_else(|| anyhow::anyhow!("token unavailable"))?,
        };
        let level = access::user_access(pool, &identity, repo).await?;
        anyhow::ensure!(
            if write {
                level.can_write()
            } else {
                level.can_read()
            },
            "repository access denied"
        );
        anyhow::ensure!(!write || !repo.is_archived, "repository archived");
        Ok((identity, level))
    }

    pub(crate) fn id(self) -> i64 {
        match self {
            Self::SshKey(id) | Self::DeployKey(id) | Self::Token(id) => id,
        }
    }
}

pub(crate) struct ReceiveSession<'a> {
    pub repo_id: i64,
    pub path: &'a std::path::Path,
    pub session: crate::backend::repositories::store::GitSession,
}

pub(crate) async fn receive_session(
    pool: DbPool,
    ci: Arc<CiRuns>,
    receive: ReceiveSession<'_>,
    streams: crate::backend::repositories::store::GitStreams<'_>,
) -> Result<std::process::ExitStatus> {
    #[cfg(target_os = "linux")]
    {
        broker::receive_session(pool, ci, receive, streams).await
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (pool, ci, receive, streams);
        anyhow::bail!("Git receive requires Linux")
    }
}
