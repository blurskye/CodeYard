//! Shared Git sessions and live receive-hook authority for every transport.
use crate::receive_ci::ReceiveCi;
use anyhow::Result;
use codeyard_database::DbPool;
use std::future::Future;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use tokio::task::JoinSet;

#[cfg(target_os = "linux")]
pub mod broker;
#[cfg(any(test, feature = "test-hooks"))]
#[cfg(target_os = "linux")]
pub mod hook_barrier;
#[cfg(target_os = "linux")]
mod hooks;
#[cfg(target_os = "linux")]
pub mod private_files;
#[cfg(target_os = "linux")]
pub mod protocol;

/// Serves until `stop` is set. After that only hooks of pushes already running are served,
/// and other requests are refused until the connections in progress finish. The caller
/// bounds that wait.
pub async fn run_gateway(
    socket: PathBuf,
    repos_root: PathBuf,
    pool: DbPool,
    ci: Arc<dyn ReceiveCi>,
    git_user: String,
    stop: tokio::sync::watch::Receiver<bool>,
) -> Result<()> {
    #[cfg(target_os = "linux")]
    {
        crate::repositories::git_sessions::broker::run(socket, repos_root, pool, ci, git_user, stop)
            .await
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (socket, repos_root, pool, ci, git_user, stop);
        anyhow::bail!("OpenSSH gateway is currently available only on Linux Unix sockets")
    }
}

/// Git tasks outlive their connections, but not their server's shutdown drain.
pub struct GitTasks(Mutex<Option<JoinSet<()>>>);

impl Default for GitTasks {
    fn default() -> Self {
        Self(Mutex::new(Some(JoinSet::new())))
    }
}

impl GitTasks {
    pub fn spawn(&self, task: impl Future<Output = ()> + Send + 'static) -> bool {
        let mut tasks = self.0.lock().unwrap_or_else(|error| error.into_inner());
        let Some(tasks) = tasks.as_mut() else {
            return false;
        };
        while tasks.try_join_next().is_some() {}
        if tasks.len() >= 2 * codeyard_git::repository::process::session_capacity() {
            return false;
        }
        // The task keeps the starting operation's foreign proofs, as other follow-up work does.
        tasks.spawn(codeyard_database::cells::authorization::inherit(task));
        true
    }

    pub async fn drain(&self) {
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
pub struct PostReceiveFailed;

/// The credential is rechecked by the receive hooks, not just at HTTP/SSH ingress.
#[derive(Clone, Copy, serde::Serialize, serde::Deserialize)]
pub enum ReceiveCredential {
    SshKey(i64),
    DeployKey(i64),
    Token(i64),
    ForeignToken { id: i64, hash: [u8; 32] },
    ForeignSsh { id: i64, fingerprint: [u8; 32] },
}

impl std::fmt::Debug for ReceiveCredential {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let name = match self {
            Self::SshKey(_) => "SshKey",
            Self::DeployKey(_) => "DeployKey",
            Self::Token(_) => "Token",
            Self::ForeignToken { .. } => "ForeignToken",
            Self::ForeignSsh { .. } => "ForeignSsh",
        };
        f.debug_tuple(name).field(&self.id()).finish()
    }
}

/// Typed policy refusals let ingress project transport-specific denials without
/// reimplementing credential, token-bound, or repository policy.
#[derive(Debug, thiserror::Error)]
pub enum CredentialRefusal {
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
    fn foreign_token(id: i64, hash: &str) -> Result<Self> {
        codeyard_types::cell_ids::Prefix::for_id(id)?;
        anyhow::ensure!(
            codeyard_types::tokens::is_hex_token(hash, 64),
            CredentialRefusal::Credential
        );
        let hash = hex::decode(hash)?
            .try_into()
            .map_err(|_| anyhow::anyhow!(CredentialRefusal::Credential))?;
        Ok(Self::ForeignToken { id, hash })
    }

    /// Keep the exact initiating token proof through prepared ref publication.
    pub fn publication(
        actor: &codeyard_database::queries::credentials::Actor,
    ) -> Result<Option<Self>> {
        use codeyard_database::queries::users::accounts::OwnedAccountCredential;
        match &actor.credential {
            Some(OwnedAccountCredential::Token(id, _)) => Ok(Some(Self::Token(*id))),
            Some(OwnedAccountCredential::ForeignToken(id, hash)) => {
                Self::foreign_token(*id, hash).map(Some)
            }
            Some(OwnedAccountCredential::Session(_)) | None => Ok(None),
        }
    }

    pub fn personal(
        token: &codeyard_database::models::PersonalAccessToken,
        user: &codeyard_database::models::User,
    ) -> Result<Self> {
        if user.is_stub {
            Self::foreign_token(token.id, &token.token_hash)
        } else {
            Ok(Self::Token(token.id))
        }
    }
    pub fn foreign_ssh(id: i64, fingerprint: &str) -> Result<Self> {
        use base64::Engine;
        let fingerprint = base64::engine::general_purpose::STANDARD_NO_PAD
            .decode(
                fingerprint
                    .strip_prefix("SHA256:")
                    .ok_or(CredentialRefusal::Credential)?,
            )?
            .try_into()
            .map_err(|_| anyhow::anyhow!(CredentialRefusal::Credential))?;
        Ok(Self::ForeignSsh { id, fingerprint })
    }
    fn proof(self) -> Result<(i64, codeyard_database::cells::authorization::Proof)> {
        use base64::Engine;
        use codeyard_database::cells::authorization::Proof;
        Ok(match self {
            Self::ForeignToken { id, hash } => (
                id,
                Proof::Token {
                    home: codeyard_types::cell_ids::Prefix::for_id(id)?,
                    hash: hex::encode(hash),
                },
            ),
            Self::ForeignSsh { id, fingerprint } => (
                id,
                Proof::Ssh {
                    fingerprint: format!(
                        "SHA256:{}",
                        base64::engine::general_purpose::STANDARD_NO_PAD.encode(fingerprint)
                    ),
                },
            ),
            _ => anyhow::bail!(CredentialRefusal::Credential),
        })
    }
    async fn foreign(
        self,
        pool: &DbPool,
    ) -> Result<codeyard_database::cells::authorization::Credential> {
        let (id, proof) = self.proof()?;
        codeyard_database::cells::authorization::admit(pool, &proof)
            .await?
            .filter(|credential| credential.answer.credential_id == Some(id))
            .ok_or_else(|| CredentialRefusal::Credential.into())
    }
    pub async fn allows_workflows(self, pool: &DbPool) -> Result<bool> {
        use codeyard_database::cells::authorization::Authority;
        match self {
            Self::Token(id) => codeyard_identity::oauth_apps::allows_workflows(pool, id).await,
            Self::ForeignToken { .. } => {
                let credential = self.foreign(pool).await?;
                Ok(credential.answer.write
                    && match credential.answer.authority {
                        Authority::Classic => true,
                        Authority::OAuth { scopes, .. } => {
                            codeyard_types::oauth_apps::has_scope(&scopes, "workflow")
                        }
                        Authority::FineGrained { permissions, .. } => {
                            permissions.as_object().is_some_and(|permissions| {
                                codeyard_types::fine_grained::allows(permissions, "workflows", true)
                            })
                        }
                        Authority::App { permissions, .. } => {
                            codeyard_types::github_apps::allows(&permissions, "workflows", true)
                        }
                        _ => false,
                    })
            }
            _ => Ok(true),
        }
    }
    /// Rechecks the live key or token and the repository's current access policy.
    pub async fn authorize(
        self,
        pool: &DbPool,
        repo: &codeyard_database::models::Repository,
        write: bool,
    ) -> Result<(
        codeyard_database::models::User,
        codeyard_database::models::AccessLevel,
    )> {
        self.check(pool, repo, write, None).await
    }
    /// A Git receive also admits a maintainer who may edit the head branch of an open pull
    /// request from this fork. `updates` are the pushed references once Git reports them;
    /// before that, one such branch is enough to start the session.
    pub async fn authorize_receive(
        self,
        pool: &DbPool,
        repo: &codeyard_database::models::Repository,
        updates: Option<&[codeyard_repos::validation::RefUpdate]>,
    ) -> Result<(
        codeyard_database::models::User,
        codeyard_database::models::AccessLevel,
    )> {
        self.check(pool, repo, true, Some(updates)).await
    }
    /// Starts a Git session: a fetch needs read access, a receive `authorize_receive`.
    pub async fn authorize_session(
        self,
        pool: &DbPool,
        repo: &codeyard_database::models::Repository,
        write: bool,
    ) -> Result<(
        codeyard_database::models::User,
        codeyard_database::models::AccessLevel,
    )> {
        self.check(pool, repo, write, write.then_some(None)).await
    }
    async fn check(
        self,
        pool: &DbPool,
        repo: &codeyard_database::models::Repository,
        write: bool,
        receive: Option<Option<&[codeyard_repos::validation::RefUpdate]>>,
    ) -> Result<(
        codeyard_database::models::User,
        codeyard_database::models::AccessLevel,
    )> {
        anyhow::ensure!(
            !(repo.is_gist && write && matches!(self, Self::DeployKey(_))),
            CredentialRefusal::Denied
        );
        // A token's App or fine-grained bounds, loaded once for both checks below.
        let mut foreign_deadline = None;
        let (identity, bounds) = match self {
            Self::ForeignToken { .. } | Self::ForeignSsh { .. } => {
                let credential = self.foreign(pool).await?;
                foreign_deadline = Some(credential.deadline());
                anyhow::ensure!(
                    !write || credential.answer.write,
                    CredentialRefusal::ReadOnly
                );
                if matches!(self, Self::ForeignToken { .. }) {
                    let public_read = !write
                        && !repo.is_private
                        && codeyard_db_placement::node_config::get_node_configuration(pool)
                            .await?
                            .is_some_and(|configuration| {
                                configuration.setup_completed && !configuration.is_private()
                            });
                    anyhow::ensure!(
                        public_read
                            || credential.answer.authority.repository(
                                credential.user.id,
                                repo,
                                "contents",
                                write
                            )?,
                        CredentialRefusal::TokenScope
                    );
                    if matches!(
                        credential.answer.authority,
                        codeyard_database::cells::authorization::Authority::Classic
                    ) {
                        if let Some(org) = repo.owner_org_id {
                            anyhow::ensure!(
                                codeyard_db_identity::fine_grained_tokens::policy(pool, org)
                                    .await?
                                    .classic_allowed,
                                CredentialRefusal::Classic
                            );
                        }
                    }
                }
                (credential.user, None)
            }
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
                let canonical = codeyard_repos::validation::canonical_db_key(
                    &identity.key.public_key,
                    &identity.key.key_type,
                )?;
                anyhow::ensure!(
                    codeyard_db_identity::users::ssh_keys::calculate_fingerprint(&canonical)?
                        == identity.key.fingerprint,
                    CredentialRefusal::Credential
                );
                (identity.user, None)
            }
            Self::DeployKey(id) => {
                let admitted =
                    codeyard_repos::deploy_keys::authorize(pool, id, repo, write).await?;
                anyhow::ensure!(
                    !write || !codeyard_db_repos::repository_exports::locked(pool, repo.id).await?,
                    CredentialRefusal::Migration
                );
                return Ok(admitted);
            }
            Self::Token(id) => {
                let public_read = !write
                    && !repo.is_private
                    && codeyard_db_placement::node_config::get_node_configuration(pool)
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
            !identity.username.ends_with("[bot]")
                || matches!(self, Self::Token(_) | Self::ForeignToken { .. }),
            CredentialRefusal::Denied
        );
        if let Self::Token(id) = self {
            if let Some(bounds) = bounds {
                let granted = bounds.repository(pool, identity.id, repo.id).await?;
                if !granted.as_ref().is_some_and(|granted| {
                    codeyard_types::github_apps::allows(granted, "contents", write)
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
        if identity.username == codeyard_database::queries::ci_tokens::BOT
            && !matches!(self, Self::ForeignToken { .. })
        {
            let Self::Token(id) = self else {
                anyhow::bail!(CredentialRefusal::TokenScope);
            };
            let token = codeyard_db_actions::ci_tokens::get(pool, id)
                .await?
                .ok_or(CredentialRefusal::Credential)?;
            let permissions: codeyard_types::permissions::Permissions =
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
        let maintainer = match receive {
            Some(updates) if write && !level.can_write() => {
                maintainer_edits(pool, self, &identity, repo, updates).await?
            }
            _ => false,
        };
        anyhow::ensure!(level.can_read() || maintainer, CredentialRefusal::Missing);
        if let Self::Token(id) = self {
            anyhow::ensure!(
                !write
                    || codeyard_db_identity::users::tokens::token_user(pool, id, true)
                        .await?
                        .is_some(),
                CredentialRefusal::ReadOnly
            );
        }
        anyhow::ensure!(
            !write || level.can_write() || maintainer,
            CredentialRefusal::Denied
        );
        anyhow::ensure!(!write || !repo.is_archived, CredentialRefusal::Archived);
        anyhow::ensure!(
            !write || !codeyard_db_repos::repository_exports::locked(pool, repo.id).await?,
            CredentialRefusal::Migration
        );
        anyhow::ensure!(
            foreign_deadline.is_none_or(
                |deadline| deadline.current(chrono::Utc::now(), std::time::Instant::now())
            ),
            CredentialRefusal::Credential
        );
        Ok((identity, level))
    }

    pub fn id(self) -> i64 {
        match self {
            Self::SshKey(id)
            | Self::DeployKey(id)
            | Self::Token(id)
            | Self::ForeignToken { id, .. }
            | Self::ForeignSsh { id, .. } => id,
        }
    }
}

/// The most consented pull requests one fork is searched for.
const MAINTAINER_BINDINGS: usize = 1000;

/// Whether every pushed branch, or before Git reports them at least one branch, is the head
/// of an open pull request whose maintainer edits this actor may make. Only branch updates
/// qualify; a maintainer never deletes a branch, pushes a tag or writes another branch.
async fn maintainer_edits(
    pool: &DbPool,
    credential: ReceiveCredential,
    actor: &codeyard_database::models::User,
    head: &codeyard_database::models::Repository,
    updates: Option<&[codeyard_repos::validation::RefUpdate]>,
) -> Result<bool> {
    use std::collections::BTreeSet;
    let Some(base_id) = head.forked_from_id else {
        return Ok(false);
    };
    if head.is_gist
        || head.is_stub
        || head.owner_kind != "user"
        || matches!(credential, ReceiveCredential::DeployKey(_))
    {
        return Ok(false);
    }
    let wanted = match updates {
        None => None,
        Some(updates) => {
            let mut wanted = BTreeSet::new();
            // Git's transaction may name HEAD for the branch it points to; that branch is listed too.
            for update in updates.iter().filter(|update| update.reference != "HEAD") {
                let Some(branch) = update.reference.strip_prefix("refs/heads/") else {
                    return Ok(false);
                };
                if update.new == codeyard_repos::validation::ZERO_SHA {
                    return Ok(false);
                }
                wanted.insert(branch.to_owned());
            }
            if wanted.is_empty() {
                return Ok(false);
            }
            Some(wanted)
        }
    };
    let asked = |branch: &str| wanted.as_ref().is_none_or(|wanted| wanted.contains(branch));
    let done = |granted: &BTreeSet<String>| match &wanted {
        None => !granted.is_empty(),
        Some(wanted) => wanted.is_subset(granted),
    };
    let mut granted = BTreeSet::new();
    let base = codeyard_db_repos::repositories::records::get_repository_by_id(pool, base_id)
        .await?
        .ok_or(CredentialRefusal::Missing)?;
    if !base.is_stub {
        let open = codeyard_db_repos::pull_requests::records::list_open_pull_request_revisions(
            pool, head.id,
        )
        .await?;
        for revision in open.into_iter().filter(|pull| {
            pull.repo_id == base.id
                && pull.head_repo_id == Some(head.id)
                && asked(&pull.head_branch)
        }) {
            if granted.contains(&revision.head_branch) {
                continue;
            }
            let Some(pull) = codeyard_db_repos::pull_requests::records::get_pull_request_by_id(
                pool,
                revision.id,
            )
            .await?
            else {
                continue;
            };
            if codeyard_repos::maintainer_edit::can_update_head(pool, actor, &base, head, &pull)
                .await?
            {
                granted.insert(pull.head_branch);
                if done(&granted) {
                    return Ok(true);
                }
            }
        }
        return Ok(false);
    }
    // A base in another cell decides the maintainer's write access there; this cell keeps
    // the publisher's consent and the fork owner's grant.
    if !matches!(
        credential,
        ReceiveCredential::ForeignSsh { .. } | ReceiveCredential::ForeignToken { .. }
    ) {
        return Ok(false);
    }
    let (_, proof) = credential.proof()?;
    let provider = pool
        .foreign_authorization()
        .ok_or_else(|| anyhow::anyhow!("Base cell is unavailable"))?;
    let mut after = 0;
    let mut seen = 0;
    while seen < MAINTAINER_BINDINGS {
        let page =
            codeyard_db_repos::pull_requests::cells::bindings(pool, head.id, after, 100).await?;
        let Some(last) = page.last() else {
            break;
        };
        after = last.number;
        seen += page.len();
        for binding in page {
            if binding.base_repo_id != base.id
                || !asked(&binding.head_branch)
                || granted.contains(&binding.head_branch)
            {
                continue;
            }
            let Some(grant) = provider.pull_grant(&binding, Some(&proof)).await? else {
                continue;
            };
            grant.validate(&binding)?;
            let grantor = match grant.maintainer_edit_granted_by_id {
                Some(id) if Some(id) == head.personal_owner_id() => {
                    codeyard_db_identity::users::accounts::get_user_by_id(pool, id).await?
                }
                _ => None,
            };
            if grant.actor_id == Some(actor.id)
                && grantor.is_some_and(|owner| !owner.is_banned && !owner.must_change_password)
                && codeyard_db_repos::pull_requests::cells::publisher(pool, &binding).await?
                    == Some(grant.publisher_id)
                && codeyard_repos::maintainer_edit::publication_authorized(
                    pool,
                    Some(grant.publisher_id),
                    head,
                )
                .await?
            {
                granted.insert(binding.head_branch);
                if done(&granted) {
                    return Ok(true);
                }
            }
        }
    }
    Ok(false)
}

pub struct ReceiveSession<'a> {
    pub repo_id: i64,
    pub path: &'a std::path::Path,
    /// The application binary that runs the receive and reference-transaction hooks.
    pub executable: std::path::PathBuf,
    pub session: crate::store_types::GitSession,
}

pub async fn receive_session(
    pool: DbPool,
    ci: Arc<dyn ReceiveCi>,
    receive: ReceiveSession<'_>,
    streams: crate::store_types::GitStreams<'_>,
) -> Result<std::process::ExitStatus> {
    #[cfg(target_os = "linux")]
    {
        crate::repositories::git_sessions::broker::receive_session(pool, ci, receive, streams).await
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (pool, ci, receive, streams);
        anyhow::bail!("Git receive requires Linux")
    }
}
