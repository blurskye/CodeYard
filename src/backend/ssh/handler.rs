use crate::backend::ci::CiRuns;
use crate::backend::db::models::{AccessLevel, User};
use crate::backend::db::repositories::{access::get_user_repo_access, records::get_repository};
use crate::backend::db::users::ssh_keys::get_user_by_key_fingerprint;
use crate::backend::db::DbPool;
use crate::backend::git::repository::process as git_process;
use crate::backend::repositories::deploy_keys;
use crate::backend::repositories::git_sessions::{validation, GitTasks, ReceiveCredential};
use crate::backend::repositories::store::{repo_store, GitService, GitSession, GitStreams};
use crate::backend::ssh::rate_limit::RateLimiter;
use anyhow::Result;
use bytes::Bytes;
use russh::keys::PublicKey;
use russh::server::{Auth, ChannelOpenHandle, Msg, Session};
use russh::{Channel, ChannelId};
use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::Arc;
use tokio::sync::oneshot;
use tracing::{debug, error, info, warn};

struct ChannelState {
    channel: Option<Channel<Msg>>,
}

const MAX_ACTIVE_GIT_CHANNELS: usize = 8;

pub struct GitSshHandler {
    pool: DbPool,
    user: Option<User>,
    key_id: Option<i64>,
    // Set instead of `user` when a deploy key authenticated.
    deploy_key: Option<i64>,
    // Channel IDs are local to this connection, not server-global. Own the map
    // directly: an Arc here would permit cross-connection state sharing.
    channels: HashMap<u32, ChannelState>,
    rate_limiter: RateLimiter,
    client_ip: Option<IpAddr>,
    // Set by the first rejected key; one connection adds at most one failure.
    failure_recorded: bool,
    // Sent once the client authenticates, which ends the server's login deadline.
    authenticated: Option<oneshot::Sender<()>>,
    id: usize,
    ci_service: Arc<CiRuns>,
    pub(crate) git_tasks: Arc<GitTasks>,
}

impl GitSshHandler {
    pub fn new(pool: DbPool, rate_limiter: RateLimiter, ci_service: Arc<CiRuns>) -> Self {
        Self {
            pool,
            user: None,
            key_id: None,
            deploy_key: None,
            channels: HashMap::new(),
            rate_limiter,
            client_ip: None,
            failure_recorded: false,
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

    pub(crate) fn set_authenticated_signal(&mut self, signal: oneshot::Sender<()>) {
        self.authenticated = Some(signal);
    }

    fn refuse_channel(&mut self, channel: ChannelId, session: &mut Session, message: &str) {
        self.channels.remove(&u32::from(channel));
        let _ = session.extended_data(channel, 1, Bytes::from(format!("{}\n", message)));
        let _ = session.channel_failure(channel);
        let _ = session.exit_status_request(channel, 1);
        let _ = session.eof(channel);
        let _ = session.close(channel);
    }

    /// Rejects one attempt so the client can offer its next key. A connection adds at
    /// most one failure, however many attempts it makes.
    async fn reject_key(&mut self) -> Result<Auth, anyhow::Error> {
        if !self.failure_recorded {
            self.failure_recorded = true;
            if let Some(ip) = self.client_ip {
                if self.rate_limiter.record_failure(ip).await {
                    warn!("IP {} banned after too many failed attempts", ip);
                }
            }
        }
        Ok(Auth::reject())
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

    async fn auth_publickey(
        &mut self,
        user: &str,
        public_key: &PublicKey,
    ) -> Result<Auth, Self::Error> {
        self.user = None;
        self.key_id = None;
        self.deploy_key = None;
        if let Some(ip) = self.client_ip {
            if self.rate_limiter.is_blocked(ip).await {
                warn!("Blocked IP {} attempted connection", ip);
                return Err(anyhow::anyhow!(
                    "Too many failed attempts. Try again later."
                ));
            }
        }

        info!(
            "SSH auth attempt for user {:?} with key type {:?}",
            user,
            public_key.algorithm()
        );

        let openssh_str = public_key.to_openssh()?;
        let fingerprint_str =
            match crate::backend::db::users::ssh_keys::calculate_fingerprint(&openssh_str) {
                Ok(fingerprint) => fingerprint,
                Err(_) => return self.reject_key().await,
            };
        debug!("Computed fingerprint: {}", fingerprint_str);

        match get_user_by_key_fingerprint(&self.pool, &fingerprint_str).await {
            Ok(Some(user_with_key)) => {
                if user_with_key.user.is_banned || user_with_key.user.must_change_password {
                    warn!(
                        "Rejected banned user '{}' (id={}) over SSH",
                        user_with_key.user.username, user_with_key.user.id
                    );
                    return self.reject_key().await;
                }

                info!(
                    "Authenticated user '{}' (id={}) with key '{}'",
                    user_with_key.user.username, user_with_key.user.id, user_with_key.key.name
                );
                self.key_id = Some(user_with_key.key.id);
                self.user = Some(user_with_key.user);
                if let Some(signal) = self.authenticated.take() {
                    let _ = signal.send(());
                }
                Ok(Auth::Accept)
            }
            Ok(None) => {
                match crate::backend::db::deploy_keys::get_deploy_key_by_fingerprint(
                    &self.pool,
                    &fingerprint_str,
                )
                .await
                {
                    Ok(Some(key)) => {
                        info!("Authenticated deploy key '{}' (id={})", key.title, key.id);
                        self.deploy_key = Some(key.id);
                        if let Some(signal) = self.authenticated.take() {
                            let _ = signal.send(());
                        }
                        Ok(Auth::Accept)
                    }
                    Ok(None) => {
                        warn!("No user found for key fingerprint: {}", fingerprint_str);
                        self.reject_key().await
                    }
                    Err(e) => {
                        error!("Database error during auth: {}", e);
                        Err(e)
                    }
                }
            }
            // A database failure is not a rejected key, so it does not count against the client.
            Err(e) => {
                error!("Database error during auth: {}", e);
                Err(e)
            }
        }
    }

    async fn channel_open_session(
        &mut self,
        channel: Channel<Msg>,
        reply: ChannelOpenHandle,
        _session: &mut Session,
    ) -> Result<(), Self::Error> {
        debug!("Channel opened: {:?}", channel.id());
        if self.channels.len() >= MAX_ACTIVE_GIT_CHANNELS {
            reply
                .reject(russh::ChannelOpenFailure::ResourceShortage)
                .await;
            return Ok(());
        }
        self.channels.insert(
            u32::from(channel.id()),
            ChannelState {
                channel: Some(channel),
            },
        );
        reply.accept().await;
        Ok(())
    }

    async fn exec_request(
        &mut self,
        channel: ChannelId,
        data: &[u8],
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        let command = String::from_utf8_lossy(data).trim().to_owned();

        if self.user.is_none() && self.deploy_key.is_none() {
            let msg = "Not authenticated\n";
            let _ = session.data(channel, Bytes::from(msg.to_string()));
            let _ = session.channel_failure(channel);
            return Ok(());
        }

        if self
            .channels
            .get(&u32::from(channel))
            .is_none_or(|state| state.channel.is_none())
        {
            self.refuse_channel(
                channel,
                session,
                "A Git command is already active on this channel",
            );
            return Ok(());
        }
        // Parse git command
        let git_cmd = match validation::parse_command(&command) {
            Ok(cmd) => cmd,
            Err(_) => {
                let msg = "Invalid command. Only git operations are allowed.\n";
                let _ = session.data(channel, Bytes::from(msg.to_string()));
                let _ = session.channel_failure(channel);
                return Ok(());
            }
        };

        info!(
            "Git command for {}/{} (write={})",
            git_cmd.owner, git_cmd.repo, git_cmd.write
        );

        // Check if repo exists and user has access
        let repo = match get_repository(&self.pool, &git_cmd.owner, &git_cmd.repo).await? {
            Some(r) => r,
            None => {
                // A deploy key cannot tell a missing repository from one it cannot reach.
                let msg = if self.deploy_key.is_some() {
                    Self::deploy_key_denial_message(git_cmd.write)
                } else {
                    Self::repository_denial_message(None, git_cmd.write)
                };
                let _ = session.data(channel, Bytes::from(msg));
                let _ = session.channel_failure(channel);
                return Ok(());
            }
        };

        // A deploy key reaches only its own repository, with the access its checks grant.
        let credential = if let Some(key_id) = self.deploy_key {
            match deploy_keys::authorize(&self.pool, key_id, &repo, git_cmd.write).await {
                Ok(_) => {
                    deploy_keys::record_use(&self.pool, key_id).await;
                    info!(
                        "Access granted: deploy key {} for {}/{}",
                        key_id, git_cmd.owner, git_cmd.repo
                    );
                    ReceiveCredential::DeployKey(key_id)
                }
                Err(_) => {
                    let msg = Self::deploy_key_denial_message(git_cmd.write);
                    let _ = session.data(channel, Bytes::from(msg));
                    let _ = session.channel_failure(channel);
                    return Ok(());
                }
            }
        } else {
            let Some(user) = self.user.clone() else {
                let _ = session.channel_failure(channel);
                return Ok(());
            };
            // Check permissions
            let access = get_user_repo_access(&self.pool, user.id, repo.id).await?;
            let required_access = if git_cmd.write {
                AccessLevel::Write
            } else {
                AccessLevel::Read
            };

            let key_id = self
                .key_id
                .ok_or_else(|| anyhow::anyhow!("SSH key unavailable"))?;
            let current_key =
                crate::backend::db::users::ssh_keys::get_user_by_ssh_key_id(&self.pool, key_id)
                    .await?;
            let current = current_key.is_some_and(|identity| {
                identity.user.id == user.id
                    && !identity.user.is_banned
                    && !identity.user.must_change_password
            });
            if !current || access < required_access || (git_cmd.write && repo.is_archived) {
                let msg =
                    Self::repository_denial_message(Some((repo.is_private, access)), git_cmd.write);
                let _ = session.data(channel, Bytes::from(msg));
                let _ = session.channel_failure(channel);
                return Ok(());
            }

            info!(
                "Access granted: user {} has {:?} access to {}/{}",
                user.username, access, git_cmd.owner, git_cmd.repo
            );
            ReceiveCredential::SshKey(key_id)
        };

        let permit = match git_process::session_permit() {
            Ok(permit) => permit,
            Err(_) => {
                self.refuse_channel(channel, session, "Git sessions busy");
                return Ok(());
            }
        };
        let mut ssh = self
            .channels
            .get_mut(&u32::from(channel))
            .and_then(|state| state.channel.take())
            .expect("open channel");
        let mut output = ssh.make_writer();
        let mut errors = ssh.make_writer_ext(Some(1));
        let handle = session.handle();
        let store = repo_store(&repo, self.ci_service.cluster.as_ref());
        let mut options = GitSession::new(
            if git_cmd.write {
                GitService::ReceivePack
            } else {
                GitService::UploadPack
            },
            false,
        );
        options.credential = Some(credential);
        options.owner = git_cmd.owner;
        let receive = git_cmd
            .write
            .then(|| (self.pool.clone(), self.ci_service.clone()));
        if !self.git_tasks.spawn(async move {
            let _permit = permit;
            let result = store
                .git_session(
                    options,
                    receive,
                    GitStreams {
                        input: &mut ssh.make_reader(),
                        output: &mut output,
                        errors: &mut errors,
                    },
                )
                .await;
            let status = match result {
                Ok(status) => status.code().unwrap_or(1).clamp(0, 255) as u32,
                Err(error) => {
                    warn!("Git channel failed or was denied");
                    let message = if error
                        .is::<crate::backend::repositories::git_sessions::PostReceiveFailed>(
                    ) {
                        "ERR Git writes were accepted; post-push bookkeeping or CI dispatch failed.\n"
                    } else if error
                        .downcast_ref::<crate::backend::repositories::store::RepoError>()
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
        }) {
            self.refuse_channel(channel, session, "Git server shutting down");
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
            "id": 1, "username": "fixture",
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
    fn git_commands_strip_only_one_transport_suffix() {
        for (path, expected) in [
            ("owner/project", "project"),
            ("owner/project.git", "project"),
            ("/owner/project.git.git", "project.git"),
            ("owner/project.git.git.git", "project.git.git"),
        ] {
            for (service, write) in [("git-upload-pack", false), ("git-receive-pack", true)] {
                let parsed = validation::parse_command(&format!("{service} '{path}'")).unwrap();
                assert_eq!(parsed.owner, "owner");
                assert_eq!(parsed.repo, expected);
                assert_eq!(parsed.write, write);
            }
        }
        assert!(validation::parse_command("git-upload-pack 'owner/a/b.git'").is_err());
        assert!(validation::parse_command("not-git 'owner/project.git'").is_err());
    }

    #[test]
    fn builtin_git_commands_use_the_shared_parser() {
        // The built-in server uses the OpenSSH helper's parser, which refuses extra slashes.
        assert!(validation::parse_command("git-upload-pack //alice/repo.git").is_err());
        assert!(validation::parse_command("git-upload-pack 'alice/repo.git'").is_ok());
    }

    #[tokio::test]
    async fn builtin_auth_rechecks_account_and_key_state() {
        use russh::server::Handler;
        let native = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        let pool = DbPool::from(native.clone());
        crate::backend::db::run_migrations(&pool).await.unwrap();
        let owner = crate::backend::db::users::accounts::create_user(&pool, "alice", None)
            .await
            .unwrap();
        let public =
            "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIOMqqnkVzrm0SdG6UOoqKLsabgH5C9okWi0dh2l9GKJl";
        let key = crate::backend::db::users::ssh_keys::add_ssh_key(&pool, owner.id, "key", public)
            .await
            .unwrap();
        let public = PublicKey::from_openssh(public).unwrap();
        let ci = Arc::new(CiRuns::new(
            pool.clone(),
            Default::default(),
            Arc::new(tokio::sync::Notify::new()),
            None,
            Arc::new(crate::backend::notifications::NotificationBroadcaster::new(
                &pool,
            )),
        ));
        let mut handler = GitSshHandler::new(pool.clone(), RateLimiter::new(), ci);
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
        crate::backend::db::users::ssh_keys::delete_ssh_key(&pool, key.id)
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
    async fn rejected_keys_do_not_end_the_connection_and_add_one_failure_per_connection() {
        use russh::keys::{ssh_key::private::Ed25519Keypair, PrivateKey};
        use russh::server::Handler;
        let native = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        let pool = DbPool::from(native);
        crate::backend::db::run_migrations(&pool).await.unwrap();
        let owner = crate::backend::db::users::accounts::create_user(&pool, "alice", None)
            .await
            .unwrap();
        let public =
            "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIOMqqnkVzrm0SdG6UOoqKLsabgH5C9okWi0dh2l9GKJl";
        crate::backend::db::users::ssh_keys::add_ssh_key(&pool, owner.id, "key", public)
            .await
            .unwrap();
        let public = PublicKey::from_openssh(public).unwrap();
        let unknown = PrivateKey::from(Ed25519Keypair::from_seed(&[7u8; 32]))
            .public_key()
            .clone();
        let ci = Arc::new(CiRuns::new(
            pool.clone(),
            Arc::new(crate::backend::ci::CiConfig::default()),
            Arc::new(tokio::sync::Notify::new()),
            None,
            Arc::new(crate::backend::notifications::NotificationBroadcaster::new(
                &pool,
            )),
        ));
        let limiter = RateLimiter::new();
        let ip: IpAddr = "192.0.2.7".parse().unwrap();

        // A client with several keys: the unknown ones are rejected and the
        // registered key on the same connection still authenticates.
        let mut handler = GitSshHandler::new(pool.clone(), limiter.clone(), ci.clone());
        handler.set_client_ip(ip);
        for _ in 0..6 {
            assert!(matches!(
                handler.auth_publickey("git", &unknown).await.unwrap(),
                Auth::Reject { .. }
            ));
        }
        assert!(!limiter.is_blocked(ip).await);
        assert!(matches!(
            handler.auth_publickey("git", &public).await.unwrap(),
            Auth::Accept
        ));

        // The first connection counted one failure. The ban threshold is five
        // failures (rate_limit.rs), so three more connections leave the address
        // unbanned and a fourth bans it.
        for _ in 0..3 {
            let mut failed = GitSshHandler::new(pool.clone(), limiter.clone(), ci.clone());
            failed.set_client_ip(ip);
            assert!(matches!(
                failed.auth_publickey("git", &unknown).await.unwrap(),
                Auth::Reject { .. }
            ));
        }
        assert!(!limiter.is_blocked(ip).await);
        let mut failed = GitSshHandler::new(pool.clone(), limiter.clone(), ci.clone());
        failed.set_client_ip(ip);
        assert!(matches!(
            failed.auth_publickey("git", &unknown).await.unwrap(),
            Auth::Reject { .. }
        ));
        assert!(limiter.is_blocked(ip).await);
        pool.close().await;
    }

    #[tokio::test]
    async fn new_clients_do_not_inherit_identity_or_overlapping_channel_ids() {
        // Closed lazy fixture: no database connection or queries can occur.
        let pool = sqlx::postgres::PgPoolOptions::new()
            .connect_lazy("postgres://fixture:fixture@127.0.0.1:1/unused")
            .unwrap();
        pool.close().await;
        let pool = DbPool::from(pool);
        let ci_service = Arc::new(CiRuns::new(
            pool.clone(),
            Arc::new(crate::backend::ci::CiConfig::default()),
            Arc::new(tokio::sync::Notify::new()),
            None,
            Arc::new(crate::backend::notifications::NotificationBroadcaster::new(
                &pool,
            )),
        ));
        let mut server = GitSshHandler::new(pool, RateLimiter::new(), ci_service);
        server.user = Some(user());
        server.key_id = Some(1);
        server.channels.insert(0, ChannelState { channel: None });
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
        first.channels.insert(0, ChannelState { channel: None });
        second.channels.insert(0, ChannelState { channel: None });
        first.channels.remove(&0);
        assert!(second.channels.contains_key(&0));
        assert!(server.channels.contains_key(&0));
        assert!(second.user.is_none());
    }
}
