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
        anyhow::ensure!(
            !(repo.is_gist && write && matches!(self, Self::DeployKey(_))),
            "Gists require an account credential"
        );
        // Checked before the deploy-key return below, so every write credential is covered. A
        // migration lock keeps the export's snapshot complete.
        anyhow::ensure!(
            !write || !db::repository_exports::locked(pool, repo.id).await?,
            "repository is locked for migration"
        );
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
            Self::Token(id) => {
                let public_read = !write && !repo.is_private && db::node_config::get_node_configuration(pool).await?
                    .is_some_and(|config| config.setup_completed && !config.is_private());
                anyhow::ensure!(
                    crate::backend::identity::oauth_apps::allows_git(pool, id, repo.is_gist, public_read)
                        .await?,
                    "OAuth repository access denied"
                );
                db::users::tokens::token_user(pool, id, write)
                    .await?
                    .ok_or_else(|| anyhow::anyhow!("token unavailable"))?
            }
        };
        anyhow::ensure!(
            !identity.username.ends_with("[bot]") || matches!(self, Self::Token(_)),
            "bot identities require a scoped token"
        );
        if let Self::Token(id) = self {
            if let Some(bounds) = crate::backend::identity::github_apps::authority(pool, id).await?
            {
                anyhow::ensure!(
                    bounds
                        .allows(pool, identity.id, repo.id, "contents", write)
                        .await?,
                    "integration repository access denied"
                );
            } else if let Some(org) = repo.owner_org_id {
                anyhow::ensure!(
                    !db::fine_grained_tokens::classic_refused(pool, id, org).await?,
                    "the organization refuses classic tokens"
                );
            }
        }
        anyhow::ensure!(
            !repo.is_gist || !write || repo.owner_user_id == Some(identity.id),
            "Only the gist owner may push"
        );
        if identity.username == db::ci_tokens::BOT {
            let Self::Token(id) = self else {
                anyhow::bail!("bot requires a run token");
            };
            let token = db::ci_tokens::get(pool, id)
                .await?
                .ok_or_else(|| anyhow::anyhow!("run token unavailable"))?;
            let permissions: crate::backend::ci::tokens::Permissions =
                serde_json::from_str(&token.permissions)?;
            anyhow::ensure!(
                token.repo_id == repo.id && permissions.allows("contents", write),
                "run token repository access denied"
            );
        }
        let level = access::user_git_access(pool, &identity, repo).await?;
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
    /// The application binary that runs as the pre-receive and post-receive hook.
    pub executable: std::path::PathBuf,
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
