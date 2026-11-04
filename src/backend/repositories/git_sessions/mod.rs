//! Shared Git sessions and live receive-hook authority for every transport.
use crate::backend::ci::CiRuns;
use anyhow::Result;
use codeyard_db_core::DbPool;
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
        if tasks.len() >= 2 * codeyard_git::repository::process::session_capacity() {
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
        for _ in 0..2 * codeyard_git::repository::process::session_capacity() {
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

/// Typed policy refusals let ingress project transport-specific denials without
/// reimplementing credential, token-bound, or repository policy.
#[derive(Debug, thiserror::Error)]
pub(crate) enum CredentialRefusal {
    #[error("credential unavailable")]
    Credential,
    #[error("repository not found")]
    Missing,
    #[error("repository access denied")]
    Denied,
    #[error("token is read-only")]
    ReadOnly,
    #[error("resource not accessible by token")]
    TokenScope,
    #[error("resource not accessible by integration")]
    Integration,
    #[error("the organization refuses classic tokens")]
    Classic,
    #[error("repository is locked for migration")]
    Migration,
    #[error("repository archived")]
    Archived,
}

impl ReceiveCredential {
    /// Rechecks the live key or token and the repository's current access policy.
    pub(crate) async fn authorize(
        self,
        pool: &DbPool,
        repo: &codeyard_db_core::models::Repository,
        write: bool,
    ) -> Result<(
        codeyard_db_core::models::User,
        codeyard_db_core::models::AccessLevel,
    )> {
        anyhow::ensure!(
            !(repo.is_gist && write && matches!(self, Self::DeployKey(_))),
            CredentialRefusal::Denied
        );
        // A token's App or fine-grained bounds, loaded once for both checks below.
        let (identity, bounds) = match self {
            Self::SshKey(id) => {
                anyhow::ensure!(id > 0, CredentialRefusal::Credential);
                let identity =
                    codeyard_db_identity::users::ssh_keys::get_user_by_ssh_key_id(pool, id)
                        .await?
                        .ok_or(CredentialRefusal::Credential)?;
                anyhow::ensure!(
                    !identity.user.is_banned && !identity.user.must_change_password,
                    CredentialRefusal::Credential
                );
                let canonical =
                    validation::canonical_db_key(&identity.key.public_key, &identity.key.key_type)?;
                anyhow::ensure!(
                    codeyard_db_identity::users::ssh_keys::calculate_fingerprint(&canonical)?
                        == identity.key.fingerprint,
                    CredentialRefusal::Credential
                );
                (identity.user, None)
            }
            Self::DeployKey(id) => {
                let admitted =
                    crate::backend::repositories::deploy_keys::authorize(pool, id, repo, write)
                        .await?;
                anyhow::ensure!(
                    !write || !codeyard_db_repos::repository_exports::locked(pool, repo.id).await?,
                    CredentialRefusal::Migration
                );
                return Ok(admitted);
            }
            Self::Token(id) => {
                let public_read = !write
                    && !repo.is_private
                    && codeyard_db_cluster::node_config::get_node_configuration(pool)
                        .await?
                        .is_some_and(|config| config.setup_completed && !config.is_private());
                let bounds = codeyard_identity::github_apps::authority(pool, id).await?;
                anyhow::ensure!(
                    codeyard_identity::oauth_apps::allows_git(
                        pool,
                        id,
                        bounds.as_ref(),
                        repo.is_gist,
                        public_read
                    )
                    .await?,
                    CredentialRefusal::TokenScope
                );
                let user = codeyard_db_identity::users::tokens::token_user(pool, id, false)
                    .await?
                    .ok_or(CredentialRefusal::Credential)?;
                (user, bounds)
            }
        };
        anyhow::ensure!(
            !identity.username.ends_with("[bot]") || matches!(self, Self::Token(_)),
            CredentialRefusal::Denied
        );
        if let Self::Token(id) = self {
            if let Some(bounds) = bounds {
                let granted = bounds.repository(pool, identity.id, repo.id).await?;
                if !granted.as_ref().is_some_and(|granted| {
                    codeyard_model::github_apps::allows(granted, "contents", write)
                }) {
                    if bounds.personal().is_some()
                        && (granted.is_none()
                            || !codeyard_db_repos::repositories::access::git_access(
                                pool,
                                identity.id,
                                repo.id,
                                false,
                            )
                            .await?
                            .can_read())
                    {
                        anyhow::bail!(CredentialRefusal::Missing);
                    }
                    anyhow::bail!(CredentialRefusal::Integration);
                }
            } else if let Some(org) = repo.owner_org_id {
                anyhow::ensure!(
                    !codeyard_db_identity::fine_grained_tokens::classic_refused(pool, id, org)
                        .await?,
                    CredentialRefusal::Classic
                );
            }
        }
        anyhow::ensure!(
            !repo.is_gist || !write || repo.owner_user_id == Some(identity.id),
            CredentialRefusal::Denied
        );
        if identity.username == codeyard_db_core::queries::ci_tokens::BOT {
            let Self::Token(id) = self else {
                anyhow::bail!(CredentialRefusal::TokenScope);
            };
            let token = codeyard_db_actions::ci_tokens::get(pool, id)
                .await?
                .ok_or(CredentialRefusal::Credential)?;
            let permissions: codeyard_model::permissions::Permissions =
                serde_json::from_str(&token.permissions)?;
            anyhow::ensure!(token.repo_id == repo.id, CredentialRefusal::Missing);
            anyhow::ensure!(
                permissions.allows("contents", write),
                CredentialRefusal::TokenScope
            );
        }
        // Read the account, gist owner and repository grants in one snapshot; refuse writes below.
        let level =
            codeyard_db_repos::repositories::access::git_access(pool, identity.id, repo.id, false)
                .await?;
        anyhow::ensure!(level.can_read(), CredentialRefusal::Missing);
        if let Self::Token(id) = self {
            anyhow::ensure!(
                !write
                    || codeyard_db_identity::users::tokens::token_user(pool, id, true)
                        .await?
                        .is_some(),
                CredentialRefusal::ReadOnly
            );
        }
        anyhow::ensure!(!write || level.can_write(), CredentialRefusal::Denied);
        anyhow::ensure!(!write || !repo.is_archived, CredentialRefusal::Archived);
        anyhow::ensure!(
            !write || !codeyard_db_repos::repository_exports::locked(pool, repo.id).await?,
            CredentialRefusal::Migration
        );
        Ok((identity, level))
    }

    pub(crate) fn id(self) -> i64 {
        match self {
            Self::SshKey(id) | Self::DeployKey(id) | Self::Token(id) => id,
        }
    }
}

pub struct ReceiveSession<'a> {
    pub repo_id: i64,
    pub path: &'a std::path::Path,
    /// The application binary that runs the receive and reference-transaction hooks.
    pub executable: std::path::PathBuf,
    pub session: crate::backend::repositories::store::GitSession,
}

pub async fn receive_session(
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
