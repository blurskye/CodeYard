use crate::rate_limit::RateLimiter;
use anyhow::Result;
use bytes::Bytes;
use codeyard_ci_engine::runs::CiRuns;
use codeyard_db_core::models::AccessLevel;
use codeyard_db_core::models::Repository;
use codeyard_db_core::models::User;
use codeyard_db_core::DbPool;
use codeyard_db_identity::users::ssh_keys::get_user_by_key_fingerprint;
use codeyard_db_repos::repositories::records::get_git_repository;
use codeyard_git::repository::process as git_process;
use codeyard_repos_core::repositories::git_sessions::GitTasks;
use codeyard_repos_core::repositories::git_sessions::ReceiveCredential;
use codeyard_repos_core::repositories::store::repo_store;
use codeyard_repos_core::repositories::store::RepoStore;
use codeyard_repos_core::store_types::GitService;
use codeyard_repos_core::store_types::GitSession;
use codeyard_repos_core::store_types::GitStreams;
use russh::keys::PublicKey;
use russh::server::Auth;
use russh::server::ChannelOpenHandle;
use russh::server::Msg;
use russh::server::Session;
use russh::Channel;
use russh::ChannelId;
use russh::ChannelMsg;
use russh::ChannelWriteHalf;
use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::Arc;
use tokio::io::AsyncWriteExt;
use tokio::sync::oneshot;
use tracing::debug;
use tracing::error;
use tracing::info;
use tracing::warn;

/// How long the final status and close messages of a Git channel may take to reach the peer.
const FINAL_MESSAGE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// Waits for a Git session slot of the backend that `store` names, or for the channel to close.
async fn wait_for_session_slot(
    store: &RepoStore,
    queued: &tokio_util::sync::CancellationToken,
) -> Result<tokio::sync::OwnedSemaphorePermit> {
    tokio::select! {
        biased;
        _ = queued.cancelled() => Err(anyhow::anyhow!("Git channel closed")),
        permit = store.session_permit() => permit,
    }
}

#[derive(Default)]
struct ChannelState {
    channel: Option<ChannelWriteHalf<Msg>>,
    input: Option<tokio::io::DuplexStream>,
    _reader: Option<tokio_util::task::AbortOnDropHandle<()>>,
    queued: Option<tokio_util::sync::CancellationToken>,
    git_protocol: Option<String>,
}

impl Drop for ChannelState {
    fn drop(&mut self) {
        if let Some(queued) = self.queued.take() {
            queued.cancel();
        }
    }
}

const MAX_ACTIVE_GIT_CHANNELS: usize = 8;

#[derive(Debug, PartialEq, Eq)]
pub enum AuthOutcome {
    Accepted,
    Unavailable,
}

enum KeyIdentity {
    Account(Box<User>, i64),
    Deploy(i64),
}

/// Only directory-enrolled routers may carry an already verified client key.
pub trait RouterAuthorization: Send + Sync {
    fn authorize<'a>(
        &'a self,
        key: &'a PublicKey,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<bool>> + Send + 'a>>;
}

pub struct GitSshHandler {
    pub(crate) router_authorization: Option<Arc<dyn RouterAuthorization>>,
    router_peer: Option<PublicKey>,
    router_client_key: Option<String>,
    foreign_key: Option<String>,
    pool: DbPool,
    user: Option<User>,
    key_id: Option<i64>,
    // Set instead of `user` when a deploy key authenticated.
    deploy_key: Option<i64>,
    // Channel IDs are local to this connection, not server-global. Own the map
    // directly: an Arc here would permit cross-connection state sharing.
    channels: HashMap<u32, ChannelState>,
    git_slots: Arc<tokio::sync::Semaphore>,
    pub(super) rate_limiter: RateLimiter,
    client_ip: Option<IpAddr>,
    // Sent once the client authenticates, which ends the server's login deadline.
    authenticated: Option<oneshot::Sender<AuthOutcome>>,
    id: usize,
    ci_service: Arc<CiRuns>,
    pub(crate) git_tasks: Arc<GitTasks>,
}

impl GitSshHandler {
    pub fn new(pool: DbPool, rate_limiter: RateLimiter, ci_service: Arc<CiRuns>) -> Self {
        Self {
            router_authorization: None,
            router_peer: None,
            router_client_key: None,
            foreign_key: None,
            pool,
            user: None,
            key_id: None,
            deploy_key: None,
            channels: HashMap::new(),
            git_slots: Arc::new(tokio::sync::Semaphore::new(MAX_ACTIVE_GIT_CHANNELS)),
            rate_limiter,
            client_ip: None,
            authenticated: None,
            id: 0,
            ci_service,
            git_tasks: Arc::default(),
        }
    }

    pub(crate) fn set_client_ip(&mut self, ip: IpAddr) {
        self.client_ip = Some(ip);
    }

    /// Whether the client address is banned for repeated failures.
    pub(crate) async fn client_is_blocked(&self) -> bool {
        match self.client_ip {
            Some(ip) => self.rate_limiter.is_blocked(ip).await,
            None => false,
        }
    }

    pub(super) fn set_authenticated_signal(&mut self, signal: oneshot::Sender<AuthOutcome>) {
        self.authenticated = Some(signal);
    }

    async fn router_live(&self, key: &PublicKey) -> Result<bool> {
        match &self.router_authorization {
            Some(authorizer) => {
                tokio::time::timeout(std::time::Duration::from_secs(5), authorizer.authorize(key))
                    .await?
            }
            None => Ok(false),
        }
    }

    /// Refuses a request that is not supported (shell, subsystem, pty).
    fn refuse_channel(&mut self, channel: ChannelId, session: &mut Session, message: &str) {
        self.end_channel(channel, session, message, false);
    }

    /// Fails a Git command with its message. The command is accepted first, as hosted Git
    /// services do: a failed exec request makes recent OpenSSH clients exit without printing the
    /// message sent before it.
    fn refuse_command(&mut self, channel: ChannelId, session: &mut Session, message: &str) {
        self.end_channel(channel, session, message, true);
    }

    fn end_channel(
        &mut self,
        channel: ChannelId,
        session: &mut Session,
        message: &str,
        accepted: bool,
    ) {
        self.channels.remove(&u32::from(channel));
        if accepted {
            let _ = session.channel_success(channel);
        }
        let _ = session.extended_data(channel, 1, Bytes::from(format!("{}\n", message)));
        if !accepted {
            let _ = session.channel_failure(channel);
        }
        let _ = session.exit_status_request(channel, 1);
        let _ = session.eof(channel);
        let _ = session.close(channel);
    }

    /// Charge explicit authentication failures, not key exchange, key scans or database delays.
    async fn reject_key(&mut self) -> Result<Auth, anyhow::Error> {
        if let Some(ip) = self.client_ip {
            self.rate_limiter.record_failure(ip).await;
        }
        Ok(Auth::reject())
    }

    async fn key_identity(&self, public_key: &PublicKey) -> Result<Option<KeyIdentity>> {
        if public_key.algorithm().to_string().starts_with("sk-") {
            return Ok(None);
        }
        if let Some(ip) = self.client_ip {
            if self.rate_limiter.is_blocked(ip).await {
                return Ok(None);
            }
        }
        let key = public_key.to_openssh()?;
        let fingerprint = codeyard_db_identity::users::ssh_keys::calculate_fingerprint(&key)?;
        let account = get_user_by_key_fingerprint(&self.pool, &fingerprint).await?;
        let deploy = codeyard_db_identity::deploy_keys::get_deploy_key_by_fingerprint(
            &self.pool,
            &fingerprint,
        )
        .await?;
        match (account, deploy) {
            (Some(account), None)
                if !account.user.is_banned && !account.user.must_change_password =>
            {
                Ok(Some(KeyIdentity::Account(
                    Box::new(account.user),
                    account.key.id,
                )))
            }
            (None, Some(key)) => Ok(codeyard_db_identity::deploy_keys::usable_deploy_key(
                &self.pool, key.id,
            )
            .await?
            .map(|key| KeyIdentity::Deploy(key.id))),
            (None, None) => {
                use codeyard_db_core::cells::authorization::{self, Proof, Query};
                let proof = Proof::Ssh { fingerprint };
                if authorization::authorize(&self.pool, &proof)
                    .await?
                    .is_none()
                {
                    return Ok(None);
                }
                let hash = proof.hash();
                Ok(authorization::lookup(
                    &self.pool,
                    &Query {
                        kind: "ssh",
                        id: None,
                        hash: Some(&hash),
                        write: false,
                    },
                )
                .await?
                .and_then(|credential| {
                    credential
                        .answer
                        .credential_id
                        .map(|id| KeyIdentity::Account(Box::new(credential.user), id))
                }))
            }
            _ => Ok(None),
        }
    }

    /// The credential a Git command runs under. A refusal is written to the channel and gives
    /// `None`. The command's task checks the credential again after it gets a session slot, so
    /// a key or grant revoked during the wait is refused.
    async fn git_credential(
        &mut self,
        channel: ChannelId,
        session: &mut Session,
        repo: &Repository,
        write: bool,
    ) -> Result<Option<ReceiveCredential>> {
        use codeyard_repos_core::repositories::git_sessions::CredentialRefusal;
        let credential = if let Some(key) = self.deploy_key {
            ReceiveCredential::DeployKey(key)
        } else if self.user.as_ref().is_some_and(|user| user.is_stub) {
            ReceiveCredential::foreign_ssh(
                self.key_id
                    .ok_or_else(|| anyhow::anyhow!("SSH key unavailable"))?,
                self.foreign_key
                    .as_deref()
                    .ok_or_else(|| anyhow::anyhow!("SSH fingerprint unavailable"))?,
            )?
        } else if self.user.is_some() {
            ReceiveCredential::SshKey(
                self.key_id
                    .ok_or_else(|| anyhow::anyhow!("SSH key unavailable"))?,
            )
        } else {
            let _ = session.channel_failure(channel);
            return Ok(None);
        };
        let authorized = credential.authorize_session(&self.pool, repo, write).await;
        let message = match authorized {
            Ok((actor, _))
                if self.deploy_key.is_some()
                    || self.user.as_ref().is_some_and(|user| user.id == actor.id) =>
            {
                return Ok(Some(credential))
            }
            Ok(_) => Self::repository_denial_message(None, write),
            Err(_) if self.deploy_key.is_some() => Self::deploy_key_denial_message(write),
            Err(error) => match error.downcast_ref::<CredentialRefusal>() {
                Some(CredentialRefusal::Missing | CredentialRefusal::Credential) => {
                    Self::repository_denial_message(
                        Some((repo.is_private, AccessLevel::None)),
                        write,
                    )
                }
                Some(_) => Self::repository_denial_message(
                    Some((repo.is_private, AccessLevel::Read)),
                    write,
                ),
                None => return Err(error),
            },
        };
        self.refuse_command(channel, session, message.trim_end());
        Ok(None)
    }

    /// The same message for a repository that is missing and one the key cannot reach, so a deploy
    /// key learns nothing about other repositories.
    fn deploy_key_denial_message(is_write: bool) -> String {
        format!(
            "Permission denied: this deploy key cannot {} this repository\n",
            if is_write { "push to" } else { "read" }
        )
    }

    fn repository_denial_message(
        repository: Option<(bool, AccessLevel)>,
        is_write: bool,
    ) -> String {
        if repository.is_none_or(|(is_private, access)| is_private && !access.can_read()) {
            return "Repository not found\n".to_owned();
        }

        format!(
            "Permission denied: {} access required for {}\n",
            if is_write { "write" } else { "read" },
            if is_write {
                "git-receive-pack"
            } else {
                "git-upload-pack"
            }
        )
    }
}

impl russh::server::Server for GitSshHandler {
    type Handler = Self;

    fn new_client(&mut self, addr: Option<std::net::SocketAddr>) -> Self {
        self.id = self.id.wrapping_add(1);
        // Only server resources are shared. Identity and channels start fresh,
        // even if this server handler itself has authenticated/channel state.
        let mut client = Self::new(
            self.pool.clone(),
            self.rate_limiter.clone(),
            self.ci_service.clone(),
        );
        client.git_tasks = self.git_tasks.clone();
        client.router_authorization = self.router_authorization.clone();
        client.id = self.id;
        client.client_ip = addr.map(|address| address.ip());
        client
    }

    fn handle_session_error(&mut self, error: <Self::Handler as russh::server::Handler>::Error) {
        error!("Session error: {:?}", error);
    }
}

impl russh::server::Handler for GitSshHandler {
    type Error = anyhow::Error;

    /// Only public keys authenticate. A password attempt still counts as a failure.
    async fn auth_password(&mut self, _user: &str, _password: &str) -> Result<Auth, Self::Error> {
        self.reject_key().await
    }

    async fn auth_keyboard_interactive<'a>(
        &'a mut self,
        _user: &str,
        _submethods: &str,
        _response: Option<russh::server::Response<'a>>,
    ) -> Result<Auth, Self::Error> {
        self.reject_key().await
    }

    async fn auth_publickey_offered(
        &mut self,
        user: &str,
        public_key: &PublicKey,
    ) -> Result<Auth, Self::Error> {
        if user == "codeyard-router" {
            return Ok(if self.router_live(public_key).await? {
                Auth::Accept
            } else {
                Auth::reject()
            });
        }
        match tokio::time::timeout(
            std::time::Duration::from_secs(5),
            self.key_identity(public_key),
        )
        .await
        {
            Ok(Ok(Some(_))) => Ok(Auth::Accept),
            Ok(Ok(None)) => Ok(Auth::reject()),
            _ => {
                if let Some(signal) = self.authenticated.take() {
                    let _ = signal.send(AuthOutcome::Unavailable);
                }
                Err(anyhow::anyhow!("SSH identity lookup unavailable"))
            }
        }
    }

    async fn auth_publickey(
        &mut self,
        user: &str,
        public_key: &PublicKey,
    ) -> Result<Auth, Self::Error> {
        self.router_peer = None;
        self.router_client_key = None;
        self.foreign_key = None;
        self.user = None;
        self.key_id = None;
        self.deploy_key = None;
        if user == "codeyard-router" {
            if !self.router_live(public_key).await? {
                return self.reject_key().await;
            }
            self.router_peer = Some(public_key.clone());
            if let Some(signal) = self.authenticated.take() {
                let _ = signal.send(AuthOutcome::Accepted);
            }
            return Ok(Auth::Accept);
        }
        // Re-read after signature verification; an unsigned offer is never authority.
        let identity = match tokio::time::timeout(
            std::time::Duration::from_secs(5),
            self.key_identity(public_key),
        )
        .await
        {
            Ok(Ok(Some(identity))) => identity,
            Ok(Ok(None)) => return self.reject_key().await,
            _ => {
                if let Some(signal) = self.authenticated.take() {
                    let _ = signal.send(AuthOutcome::Unavailable);
                }
                return Err(anyhow::anyhow!("SSH identity lookup unavailable"));
            }
        };
        match identity {
            KeyIdentity::Account(user, id) => {
                self.foreign_key = user
                    .is_stub
                    .then(|| {
                        codeyard_db_identity::users::ssh_keys::calculate_fingerprint(
                            &public_key.to_openssh()?,
                        )
                    })
                    .transpose()?;
                self.user = Some(*user);
                self.key_id = Some(id);
            }
            KeyIdentity::Deploy(id) => self.deploy_key = Some(id),
        }
        if let Some(signal) = self.authenticated.take() {
            let _ = signal.send(AuthOutcome::Accepted);
        }
        Ok(Auth::Accept)
    }

    async fn channel_open_session(
        &mut self,
        channel: Channel<Msg>,
        reply: ChannelOpenHandle,
        _session: &mut Session,
    ) -> Result<(), Self::Error> {
        debug!("Channel opened: {:?}", channel.id());
        self.channels.retain(|_, state| {
            !state
                .queued
                .as_ref()
                .is_some_and(|token| token.is_cancelled())
        });
        if self.channels.len() >= MAX_ACTIVE_GIT_CHANNELS {
            reply
                .reject(russh::ChannelOpenFailure::ResourceShortage)
                .await;
            return Ok(());
        }
        let id = u32::from(channel.id());
        let (mut incoming, outgoing) = channel.split();
        let (input, mut writer) = tokio::io::duplex(64 * 1024);
        let reader = tokio::spawn(async move {
            let mut executing = false;
            while let Some(message) = incoming.wait().await {
                match message {
                    ChannelMsg::Exec { .. } => executing = true,
                    ChannelMsg::Data { data } if executing => {
                        if writer.write_all(&data).await.is_err() {
                            break;
                        }
                    }
                    ChannelMsg::Data { .. } | ChannelMsg::Eof | ChannelMsg::Close => break,
                    // Environment and other pre-exec requests must be drained before
                    // russh can invoke the exec callback. Their callbacks decide acceptance.
                    _ => {}
                }
            }
        });
        self.channels.insert(
            id,
            ChannelState {
                channel: Some(outgoing),
                input: Some(input),
                _reader: Some(tokio_util::task::AbortOnDropHandle::new(reader)),
                queued: None,
                git_protocol: None,
            },
        );
        reply.accept().await;
        Ok(())
    }

    async fn env_request(
        &mut self,
        channel: ChannelId,
        variable_name: &str,
        variable_value: &str,
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        if variable_name == "CODEYARD_SSH_KEY" {
            let accepted = if let Some(peer) = self.router_peer.clone() {
                if !self.router_live(&peer).await? || variable_value.len() > 16384 {
                    false
                } else if let Some(existing) = &self.router_client_key {
                    existing == variable_value
                } else if let Ok(key) = PublicKey::from_openssh(variable_value) {
                    match tokio::time::timeout(
                        std::time::Duration::from_secs(5),
                        self.key_identity(&key),
                    )
                    .await??
                    {
                        Some(KeyIdentity::Account(user, id)) => {
                            self.foreign_key = user
                                .is_stub
                                .then(|| {
                                    codeyard_db_identity::users::ssh_keys::calculate_fingerprint(
                                        variable_value,
                                    )
                                })
                                .transpose()?;
                            self.user = Some(*user);
                            self.key_id = Some(id);
                            self.router_client_key = Some(variable_value.to_owned());
                            true
                        }
                        Some(KeyIdentity::Deploy(id)) => {
                            self.deploy_key = Some(id);
                            self.router_client_key = Some(variable_value.to_owned());
                            true
                        }
                        None => false,
                    }
                } else {
                    false
                }
            } else {
                false
            };
            if accepted {
                let _ = session.channel_success(channel);
            } else {
                let _ = session.channel_failure(channel);
            }
            return Ok(());
        }
        let accepted = self
            .channels
            .get_mut(&u32::from(channel))
            .filter(|state| state.channel.is_some())
            .filter(|_| variable_name == "GIT_PROTOCOL")
            .and_then(|state| {
                let protocol = codeyard_repos::validation::protocol(Some(variable_value)).ok()?;
                state.git_protocol = protocol.map(str::to_owned);
                Some(())
            })
            .is_some();
        if accepted {
            let _ = session.channel_success(channel);
        } else {
            let _ = session.channel_failure(channel);
        }
        Ok(())
    }

    async fn shell_request(
        &mut self,
        channel: ChannelId,
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        self.refuse_channel(
            channel,
            session,
            "Shell access is disabled. Only Git commands are supported.",
        );
        Ok(())
    }

    async fn subsystem_request(
        &mut self,
        channel: ChannelId,
        _name: &str,
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        self.refuse_channel(channel, session, "SSH subsystems are disabled.");
        Ok(())
    }

    async fn pty_request(
        &mut self,
        channel: ChannelId,
        _term: &str,
        _col_width: u32,
        _row_height: u32,
        _pix_width: u32,
        _pix_height: u32,
        _modes: &[(russh::Pty, u32)],
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        self.refuse_channel(channel, session, "PTY access is disabled.");
        Ok(())
    }

    async fn exec_request(
        &mut self,
        channel: ChannelId,
        data: &[u8],
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        if let Some(peer) = &self.router_peer {
            if !self.router_live(peer).await? {
                self.refuse_command(channel, session, "Router key revoked");
                return Ok(());
            }
        }
        if let Some(context) = &self.ci_service.cluster {
            let node = tokio::time::timeout(
                std::time::Duration::from_secs(2),
                codeyard_db_cluster::cluster::get_node_by_id(&self.pool, context.node_id),
            )
            .await;
            if !matches!(node, Ok(Ok(Some(node))) if node.status == "active" && node.state == "active")
            {
                return Err(anyhow::anyhow!("Node is draining; retry on another node"));
            }
        }
        let command = String::from_utf8_lossy(data).trim().to_owned();

        if self.user.is_none() && self.deploy_key.is_none() {
            self.refuse_command(channel, session, "Not authenticated");
            return Ok(());
        }

        if self
            .channels
            .get(&u32::from(channel))
            .is_none_or(|state| state.channel.is_none())
        {
            self.refuse_command(
                channel,
                session,
                "A Git command is already active on this channel",
            );
            return Ok(());
        }
        // Parse git command
        let git_cmd = match codeyard_repos::validation::parse_command(&command) {
            Ok(cmd) => cmd,
            Err(_) => {
                self.refuse_command(
                    channel,
                    session,
                    "Invalid command. Only git operations are allowed.",
                );
                return Ok(());
            }
        };

        info!(
            "Git command for {}/{} (write={})",
            git_cmd.owner, git_cmd.repo, git_cmd.write
        );

        // Check if repo exists and user has access
        let repo = match tokio::time::timeout(
            std::time::Duration::from_secs(5),
            get_git_repository(&self.pool, &git_cmd.owner, &git_cmd.repo),
        )
        .await
        {
            Ok(Ok(Some(repo))) => repo,
            Ok(Ok(None)) => {
                let msg = if self.deploy_key.is_some() {
                    Self::deploy_key_denial_message(git_cmd.write)
                } else {
                    Self::repository_denial_message(None, git_cmd.write)
                };
                self.refuse_command(channel, session, msg.trim_end());
                return Ok(());
            }
            _ => {
                self.refuse_command(channel, session, "Git authorization unavailable");
                return Ok(());
            }
        };
        // Admission errors fail this channel, not unrelated multiplexed commands.
        let credential = match tokio::time::timeout(
            std::time::Duration::from_secs(5),
            self.git_credential(channel, session, &repo, git_cmd.write),
        )
        .await
        {
            Ok(Ok(Some(credential))) => credential,
            Ok(Ok(None)) => return Ok(()),
            _ => {
                self.refuse_command(channel, session, "Git authorization unavailable");
                return Ok(());
            }
        };
        info!(
            "Access granted: {:?} for {}/{}",
            credential, git_cmd.owner, git_cmd.repo
        );
        let Ok(task_slot) = self.git_slots.clone().try_acquire_owned() else {
            self.refuse_command(channel, session, "Git sessions busy");
            return Ok(());
        };
        let mut io = std::mem::take(
            self.channels
                .get_mut(&u32::from(channel))
                .expect("open channel"),
        );
        let queued = tokio_util::sync::CancellationToken::new();
        self.channels.get_mut(&u32::from(channel)).unwrap().queued = Some(queued.clone());
        let ssh = io.channel.as_ref().expect("open channel");
        let mut output = ssh.make_writer();
        let mut errors = ssh.make_writer_ext(Some(1));
        let handle = session.handle();
        let pool = self.pool.clone();
        let cluster = self.ci_service.cluster.clone();
        let mut options = GitSession::new(
            if git_cmd.write {
                GitService::ReceivePack
            } else {
                GitService::UploadPack
            },
            false,
        );
        options.git_protocol = io.git_protocol.take();
        options.credential = Some(credential);
        options.owner = git_cmd.owner;
        let receive = git_cmd.write.then(|| {
            (
                self.pool.clone(),
                self.ci_service.clone() as Arc<dyn codeyard_repos_core::receive_ci::ReceiveCi>,
            )
        });
        if !self.git_tasks.spawn(async move {
            let _task_slot = task_slot;
            // Waiting in russh's serial callback would stop the active channel from
            // receiving the data and window adjustments it needs to release its slot.
            let result = async {
                let queued_store = repo_store(&repo, cluster.as_ref());
                let mut _permit = wait_for_session_slot(&queued_store, &queued).await?;
                let repo = tokio::time::timeout(std::time::Duration::from_secs(5), codeyard_db_repos::repositories::records::get_repository_by_id(&pool, repo.id))
                    .await??.ok_or_else(|| anyhow::anyhow!("repository unavailable"))?;
                let store = repo_store(&repo, cluster.as_ref());
                if store.is_local() != queued_store.is_local() {
                    // The repository moved to the other backend while this task waited, so the
                    // slot it holds counts the wrong one.
                    drop(_permit);
                    _permit = wait_for_session_slot(&store, &queued).await?;
                }
                match credential {
                    ReceiveCredential::DeployKey(id) => { let _ = tokio::time::timeout(std::time::Duration::from_secs(1), codeyard_repos::deploy_keys::record_use(&pool, id)).await; }
                    ReceiveCredential::SshKey(id) => { let _ = tokio::time::timeout(std::time::Duration::from_secs(1), codeyard_db_identity::users::ssh_keys::touch_ssh_key(&pool, id)).await; }
                    _ => {}
                }
                let (actor,_) = tokio::time::timeout(std::time::Duration::from_secs(5), credential.authorize_session(&pool, &repo, git_cmd.write)).await??;
                let visitor=match credential { ReceiveCredential::DeployKey(id)=>codeyard_db_repos::repositories::traffic::Identity::deploy_key(id), _=>codeyard_db_repos::repositories::traffic::Identity::account(actor.id) };
                let observation=codeyard_repos::traffic::clone::Observation::default();
                let mut input=observation.input(io.input.as_mut().expect("open input"));
                let mut output=observation.output(&mut output);
                let execution = store.git_session(options, receive, GitStreams {
                    input: &mut input, output: &mut output, errors: &mut errors,
                });
                let status = if git_cmd.write { execution.await? } else {
                    tokio::select! {
                        biased;
                        _ = queued.cancelled() => anyhow::bail!("Git channel closed"),
                        status = execution => status?,
                    }
                };
                drop(_permit);
                drop(_task_slot);
                if !git_cmd.write && status.success() && observation.initial_transfer() {
                    if let Some(visitor)=visitor { let _ = tokio::time::timeout(std::time::Duration::from_secs(1), codeyard_repos::traffic::observe(&pool,repo.id,&visitor,&codeyard_db_repos::repositories::traffic::Metric::clone_transfer())).await; }
                }
                Ok::<_,anyhow::Error>(status)
            }.await;
            // The peer may never read these messages. The timeout keeps them from holding the
            // task slot, which ends with this block either way.
            let finish = async {
                let status = match result {
                    Ok(status) => status.code().unwrap_or(1).clamp(0, 255) as u32,
                    Err(error) => {
                        warn!(%error, "Git channel failed or was denied");
                        let message = if error.is::<git_process::SessionsBusy>()
                            || error
                                .downcast_ref::<codeyard_git::error::RepoError>()
                                .is_some_and(|error| error.is_sessions_busy())
                        {
                            "Git sessions busy\n"
                        } else if error
                            .is::<codeyard_repos_core::repositories::git_sessions::PostReceiveFailed>(
                        ) {
                            "ERR Git writes were accepted; post-push bookkeeping or CI dispatch failed.\n"
                        } else if error
                            .downcast_ref::<codeyard_git::error::RepoError>()
                            .is_some_and(|error| error.is_storage_node_unavailable())
                        {
                            "ERR storage node unavailable\n"
                        } else {
                            "ERR Git operation failed or was denied\n"
                        };
                        let _ = handle
                            .extended_data(channel, 1, Bytes::from(message.to_owned()))
                            .await;
                        1
                    }
                };
                let _ = handle.exit_status_request(channel, status).await;
                let _ = handle.eof(channel).await;
                let _ = handle.close(channel).await;
            };
            let _ = tokio::time::timeout(FINAL_MESSAGE_TIMEOUT, finish).await;
            queued.cancel();
        }) {
            self.refuse_command(channel, session, "Git server busy or shutting down");
            return Ok(());
        }
        let _ = session.channel_success(channel);
        Ok(())
    }

    // Russh feeds data and EOF into the channel's bounded reader before invoking
    // these callbacks. Reading it directly also gives stdout SSH-window backpressure.

    async fn channel_close(
        &mut self,
        channel: ChannelId,
        _session: &mut Session,
    ) -> Result<(), Self::Error> {
        debug!("Channel closed: {:?}", channel);
        self.channels.remove(&u32::from(channel));
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use russh::server::Server;

    fn user() -> User {
        serde_json::from_value(serde_json::json!({
            "id": 1, "username": "fixture", "home_cell": 0, "is_stub": false,
            "is_admin": false, "is_banned": false,
            "must_change_password": false, "email_verified": false,
            "two_factor_enabled": false,
            "created_at": "2026-01-01T00:00:00Z",
            "updated_at": "2026-01-01T00:00:00Z"
        }))
        .unwrap()
    }

    #[test]
    fn private_repository_denials_match_missing_repository() {
        for is_write in [false, true] {
            let missing = GitSshHandler::repository_denial_message(None, is_write);
            let private =
                GitSshHandler::repository_denial_message(Some((true, AccessLevel::None)), is_write);
            assert_eq!(missing, private);
            assert_eq!(missing, "Repository not found\n");
        }

        assert_eq!(
            GitSshHandler::repository_denial_message(Some((false, AccessLevel::None)), true,),
            "Permission denied: write access required for git-receive-pack\n"
        );
        assert_eq!(
            GitSshHandler::repository_denial_message(Some((true, AccessLevel::Read)), true),
            "Permission denied: write access required for git-receive-pack\n"
        );
    }

    #[test]
    fn closing_a_channel_cancels_queued_work() {
        let token = tokio_util::sync::CancellationToken::new();
        let state = ChannelState {
            queued: Some(token.clone()),
            git_protocol: None,
            channel: None,
            input: None,
            _reader: None,
        };
        assert!(!token.is_cancelled());
        drop(state);
        assert!(token.is_cancelled());
    }

    #[test]
    fn git_commands_strip_only_one_transport_suffix() {
        for (path, expected) in [
            ("owner/project", "project"),
            ("owner/project.git", "project"),
            ("/owner/project.git.git", "project.git"),
            ("owner/project.git.git.git", "project.git.git"),
        ] {
            for (service, write) in [("git-upload-pack", false), ("git-receive-pack", true)] {
                let parsed =
                    codeyard_repos::validation::parse_command(&format!("{service} '{path}'"))
                        .unwrap();
                assert_eq!(parsed.owner, "owner");
                assert_eq!(parsed.repo, expected);
                assert_eq!(parsed.write, write);
            }
        }
        assert!(
            codeyard_repos::validation::parse_command("git-upload-pack 'owner/a/b.git'").is_err()
        );
        assert!(codeyard_repos::validation::parse_command("not-git 'owner/project.git'").is_err());
    }

    #[test]
    fn builtin_git_commands_use_the_shared_parser() {
        // The built-in server uses the OpenSSH helper's parser, which refuses extra slashes.
        assert!(
            codeyard_repos::validation::parse_command("git-upload-pack //alice/repo.git").is_err()
        );
        assert!(
            codeyard_repos::validation::parse_command("git-upload-pack 'alice/repo.git'").is_ok()
        );
    }

    #[tokio::test]
    async fn builtin_auth_rechecks_account_and_key_state() {
        use russh::server::Handler;
        let native = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .map(codeyard_db_core::pool::Pool::from)
            .unwrap();
        let pool = DbPool::from(native.clone());
        codeyard_db_core::run_migrations(&pool).await.unwrap();
        let owner = codeyard_db_identity::users::accounts::create_user(&pool, "alice", None)
            .await
            .unwrap();
        let public =
            "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIOMqqnkVzrm0SdG6UOoqKLsabgH5C9okWi0dh2l9GKJl";
        let key =
            codeyard_db_identity::users::ssh_keys::add_ssh_key(&pool, owner.id, "key", public)
                .await
                .unwrap();
        let public = PublicKey::from_openssh(public).unwrap();
        let ci = Arc::new(CiRuns::new(
            pool.clone(),
            Default::default(),
            Arc::new(tokio::sync::Notify::new()),
            None,
            Arc::new(codeyard_notifications::broadcast::NotificationBroadcaster::new(&pool)),
        ));
        let mut handler = GitSshHandler::new(pool.clone(), RateLimiter::new(), ci);
        // Enrollment stays valid for host OpenSSH; the built-in verifier cannot require touch.
        let security_key = "sk-ssh-ed25519@openssh.com AAAAGnNrLXNzaC1lZDI1NTE5QG9wZW5zc2guY29tAAAAIOMqqnkVzrm0SdG6UOoqKLsabgH5C9okWi0dh2l9GKJlAAAAC3NzaDpmaXh0dXJl";
        codeyard_db_identity::users::ssh_keys::add_ssh_key(
            &pool,
            owner.id,
            "security key",
            security_key,
        )
        .await
        .unwrap();
        assert!(matches!(
            handler
                .auth_publickey("git", &PublicKey::from_openssh(security_key).unwrap())
                .await
                .unwrap(),
            Auth::Reject { .. }
        ));
        assert!(handler.user.is_none() && handler.key_id.is_none());
        assert!(matches!(
            handler.auth_publickey("git", &public).await.unwrap(),
            Auth::Accept
        ));
        assert_eq!(handler.user.as_ref().unwrap().id, owner.id);
        for column in ["is_banned", "must_change_password"] {
            sqlx::query(&format!("UPDATE users SET {column} = TRUE WHERE id = ?1"))
                .bind(owner.id)
                .execute(&native)
                .await
                .unwrap();
            assert!(matches!(
                handler.auth_publickey("git", &public).await.unwrap(),
                Auth::Reject { .. }
            ));
            assert!(handler.user.is_none() && handler.key_id.is_none());
            sqlx::query(&format!("UPDATE users SET {column} = FALSE WHERE id = ?1"))
                .bind(owner.id)
                .execute(&native)
                .await
                .unwrap();
        }
        codeyard_db_identity::users::ssh_keys::delete_ssh_key(&pool, key.id)
            .await
            .unwrap();
        assert!(matches!(
            handler.auth_publickey("git", &public).await.unwrap(),
            Auth::Reject { .. }
        ));
        assert!(handler.user.is_none() && handler.key_id.is_none());
        pool.close().await;
    }

    #[tokio::test]
    async fn rejected_keys_do_not_end_or_charge_a_successful_connection() {
        use russh::keys::{ssh_key::private::Ed25519Keypair, PrivateKey};
        use russh::server::Handler;
        let native = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .map(codeyard_db_core::pool::Pool::from)
            .unwrap();
        let pool = DbPool::from(native);
        codeyard_db_core::run_migrations(&pool).await.unwrap();
        let owner = codeyard_db_identity::users::accounts::create_user(&pool, "alice", None)
            .await
            .unwrap();
        let public =
            "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIOMqqnkVzrm0SdG6UOoqKLsabgH5C9okWi0dh2l9GKJl";
        codeyard_db_identity::users::ssh_keys::add_ssh_key(&pool, owner.id, "key", public)
            .await
            .unwrap();
        let public = PublicKey::from_openssh(public).unwrap();
        let unknown = PrivateKey::from(Ed25519Keypair::from_seed(&[7u8; 32]))
            .public_key()
            .clone();
        let ci = Arc::new(CiRuns::new(
            pool.clone(),
            Arc::new(codeyard_ci::config::CiConfig::default()),
            Arc::new(tokio::sync::Notify::new()),
            None,
            Arc::new(codeyard_notifications::broadcast::NotificationBroadcaster::new(&pool)),
        ));
        let limiter = RateLimiter::new();
        let ip: IpAddr = "192.0.2.7".parse().unwrap();

        // A client with several keys offers each one: the unknown ones are rejected and the
        // registered key on the same connection still authenticates.
        let mut handler = GitSshHandler::new(pool.clone(), limiter.clone(), ci.clone());
        handler.set_client_ip(ip);
        for _ in 0..6 {
            assert!(matches!(
                handler
                    .auth_publickey_offered("git", &unknown)
                    .await
                    .unwrap(),
                Auth::Reject { .. }
            ));
        }
        assert!(!limiter.is_blocked(ip).await);
        assert!(matches!(
            handler.auth_publickey("git", &public).await.unwrap(),
            Auth::Accept
        ));

        // Successful connections must never retain charges for intermediate keys.
        for _ in 0..6 {
            let mut failed = GitSshHandler::new(pool.clone(), limiter.clone(), ci.clone());
            failed.set_client_ip(ip);
            assert!(matches!(
                failed
                    .auth_publickey_offered("git", &unknown)
                    .await
                    .unwrap(),
                Auth::Reject { .. }
            ));
        }
        assert!(!limiter.is_blocked(ip).await);
        // The connection owner accounts for failed connections when they end.
        for attempt in 0..5 {
            assert_eq!(limiter.record_failure(ip).await, attempt == 4);
        }
        assert!(limiter.is_blocked(ip).await);
        pool.close().await;
    }

    #[tokio::test]
    async fn new_clients_do_not_inherit_identity_or_overlapping_channel_ids() {
        // Closed lazy fixture: no database connection or queries can occur.
        let pool = sqlx::postgres::PgPoolOptions::new()
            .connect_lazy("postgres://fixture:fixture@127.0.0.1:1/unused")
            .map(codeyard_db_core::pool::Pool::from)
            .unwrap();
        pool.close().await;
        let pool = DbPool::from(pool);
        let ci_service = Arc::new(CiRuns::new(
            pool.clone(),
            Arc::new(codeyard_ci::config::CiConfig::default()),
            Arc::new(tokio::sync::Notify::new()),
            None,
            Arc::new(codeyard_notifications::broadcast::NotificationBroadcaster::new(&pool)),
        ));
        let mut server = GitSshHandler::new(pool, RateLimiter::new(), ci_service);
        server.user = Some(user());
        server.key_id = Some(1);
        server.channels.insert(0, ChannelState::default());
        let mut first = server.new_client(Some("127.0.0.1:1234".parse().unwrap()));
        let mut second = server.new_client(Some("127.0.0.2:1234".parse().unwrap()));
        assert!(first.user.is_none());
        assert!(second.user.is_none());
        assert!(first.key_id.is_none());
        assert!(second.key_id.is_none());
        assert!(first.channels.is_empty());
        assert!(second.channels.is_empty());
        assert_ne!(first.id, second.id);
        assert_ne!(first.client_ip, second.client_ip);
        first.user = Some(user());
        first.channels.insert(0, ChannelState::default());
        second.channels.insert(0, ChannelState::default());
        first.channels.remove(&0);
        assert!(second.channels.contains_key(&0));
        assert!(server.channels.contains_key(&0));
        assert!(second.user.is_none());
    }
}
