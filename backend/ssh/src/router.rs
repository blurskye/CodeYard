//! SSH termination and a pinned, router-authenticated hop to the owning cell.
use crate::{
    handler::AuthOutcome,
    rate_limit::RateLimiter,
    server::{duplicate_socket, ConnectionHandler, ShutdownOnDrop},
};
use anyhow::{ensure, Context, Result};
use bytes::Bytes;
use russh::{
    keys::{PrivateKey, PrivateKeyWithHashAlg, PublicKey},
    server::{Auth, ChannelOpenHandle, Handle, Msg, Session},
    Channel, ChannelId, ChannelMsg, PublicKeyOrCertificate,
};
use std::{
    collections::HashMap,
    future::Future,
    net::{IpAddr, SocketAddr},
    pin::Pin,
    sync::Arc,
    time::Duration,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt, DuplexStream},
    sync::oneshot,
};
use tokio_util::task::AbortOnDropHandle;

pub struct Target {
    pub addresses: Vec<SocketAddr>,
    pub pins: Vec<String>,
}
pub trait Directory: Send + Sync {
    fn resolve<'a>(
        &'a self,
        owner: Option<&'a str>,
        fingerprint: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<Option<Target>>> + Send + 'a>>;
}
struct ChannelState {
    input: Option<DuplexStream>,
    protocol: Option<String>,
    _reader: AbortOnDropHandle<()>,
    _relay: Option<AbortOnDropHandle<()>>,
}
pub struct Handler {
    directory: Arc<dyn Directory>,
    identity: Arc<PrivateKey>,
    key: Option<PublicKey>,
    fingerprint: Option<String>,
    channels: HashMap<u32, ChannelState>,
    rate: RateLimiter,
    ip: Option<IpAddr>,
    authenticated: Option<oneshot::Sender<AuthOutcome>>,
}
impl Handler {
    pub fn new(directory: Arc<dyn Directory>, identity: PrivateKey) -> Self {
        Self {
            directory,
            identity: Arc::new(identity),
            key: None,
            fingerprint: None,
            channels: HashMap::new(),
            rate: RateLimiter::new(),
            ip: None,
            authenticated: None,
        }
    }
    fn refuse(channel: ChannelId, session: &mut Session, message: &str) {
        let _ = session.channel_success(channel);
        let _ = session.extended_data(channel, 1, Bytes::from(format!("{message}\n")));
        let _ = session.exit_status_request(channel, 1);
        let _ = session.eof(channel);
        let _ = session.close(channel);
    }
    async fn known(&self, key: &PublicKey) -> Result<Option<String>> {
        if self.client_is_blocked().await {
            return Ok(None);
        }
        let public = key.to_openssh()?;
        let fingerprint = codeyard_db_identity::users::ssh_keys::calculate_fingerprint(&public)?;
        let known = tokio::time::timeout(
            Duration::from_secs(10),
            self.directory.resolve(None, &fingerprint),
        )
        .await??;
        Ok(known.map(|_| fingerprint))
    }
    async fn start(
        &mut self,
        channel: ChannelId,
        command: Option<String>,
        session: &mut Session,
    ) -> Result<()> {
        let Some(key) = self.key.clone() else {
            Self::refuse(channel, session, "Not authenticated");
            return Ok(());
        };
        let fingerprint = self.fingerprint.clone().context("Missing SSH identity")?;
        let owner = command
            .as_ref()
            .map(|command| codeyard_repos::validation::parse_command(command))
            .transpose()?;
        let target = tokio::time::timeout(
            Duration::from_secs(10),
            self.directory.resolve(
                owner.as_ref().map(|command| command.owner.as_str()),
                &fingerprint,
            ),
        )
        .await??;
        let Some(target) = target else {
            Self::refuse(channel, session, "Repository or account not found");
            return Ok(());
        };
        ensure!(
            !target.addresses.is_empty()
                && target.addresses.len() <= 1000
                && !target.pins.is_empty()
                && target.pins.len() <= 1000,
            "Invalid SSH cell route"
        );
        let Some(state) = self.channels.get_mut(&u32::from(channel)) else {
            return Ok(());
        };
        let Some(input) = state.input.take() else {
            Self::refuse(channel, session, "A command is already active");
            return Ok(());
        };
        let protocol = state.protocol.clone();
        let identity = self.identity.clone();
        let handle = session.handle();
        let address = target.addresses[0];
        let _ = session.channel_success(channel);
        state._relay = Some(AbortOnDropHandle::new(tokio::spawn(async move {
            let result = tokio::time::timeout(
                Duration::from_secs(3600),
                relay(
                    address,
                    target.pins,
                    identity,
                    key,
                    input,
                    protocol,
                    command,
                    handle.clone(),
                    channel,
                ),
            )
            .await;
            if !matches!(result, Ok(Ok(()))) {
                let _=handle.extended_data(channel,1,Bytes::from_static(b"The account's cell is unavailable or refused the SSH session. Please retry.\n")).await;
                let _ = handle.exit_status_request(channel, 1).await;
                let _ = handle.eof(channel).await;
                let _ = handle.close(channel).await;
            }
        })));
        Ok(())
    }
}
impl ConnectionHandler for Handler {
    fn set_client_ip(&mut self, ip: IpAddr) {
        self.ip = Some(ip);
    }
    fn set_authenticated_signal(&mut self, signal: oneshot::Sender<AuthOutcome>) {
        self.authenticated = Some(signal);
    }
    async fn client_is_blocked(&self) -> bool {
        match self.ip {
            Some(ip) => self.rate.is_blocked(ip).await,
            None => false,
        }
    }
    async fn drain(&self) {}
}
impl russh::server::Server for Handler {
    type Handler = Self;
    fn new_client(&mut self, address: Option<SocketAddr>) -> Self {
        Self {
            directory: self.directory.clone(),
            identity: self.identity.clone(),
            key: None,
            fingerprint: None,
            channels: HashMap::new(),
            rate: self.rate.clone(),
            ip: address.map(|address| address.ip()),
            authenticated: None,
        }
    }
}
impl russh::server::Handler for Handler {
    type Error = anyhow::Error;
    async fn auth_publickey_offered(&mut self, _user: &str, key: &PublicKey) -> Result<Auth> {
        Ok(if self.known(key).await?.is_some() {
            Auth::Accept
        } else {
            Auth::reject()
        })
    }
    async fn auth_publickey(&mut self, _user: &str, key: &PublicKey) -> Result<Auth> {
        self.key = None;
        self.fingerprint = None;
        if let Some(fingerprint) = self.known(key).await? {
            self.key = Some(key.clone());
            self.fingerprint = Some(fingerprint);
            if let Some(signal) = self.authenticated.take() {
                let _ = signal.send(AuthOutcome::Accepted);
            }
            Ok(Auth::Accept)
        } else {
            if let Some(ip) = self.ip {
                self.rate.record_failure(ip).await;
            }
            Ok(Auth::reject())
        }
    }
    async fn channel_open_session(
        &mut self,
        channel: Channel<Msg>,
        reply: ChannelOpenHandle,
        _session: &mut Session,
    ) -> Result<()> {
        if self.channels.len() >= 8 {
            reply
                .reject(russh::ChannelOpenFailure::ResourceShortage)
                .await;
            return Ok(());
        }
        let id = u32::from(channel.id());
        let (mut incoming, _outgoing) = channel.split();
        let (input, mut writer) = tokio::io::duplex(64 * 1024);
        let reader = AbortOnDropHandle::new(tokio::spawn(async move {
            let mut executing = false;
            while let Some(message) = incoming.wait().await {
                match message {
                    ChannelMsg::Exec { .. } | ChannelMsg::RequestShell { .. } => executing = true,
                    ChannelMsg::Data { data } if executing => {
                        if writer.write_all(&data).await.is_err() {
                            break;
                        }
                    }
                    ChannelMsg::Data { .. } | ChannelMsg::Eof | ChannelMsg::Close => break,
                    _ => {}
                }
            }
        }));
        self.channels.insert(
            id,
            ChannelState {
                input: Some(input),
                protocol: None,
                _reader: reader,
                _relay: None,
            },
        );
        reply.accept().await;
        Ok(())
    }
    async fn channel_close(&mut self, channel: ChannelId, _session: &mut Session) -> Result<()> {
        self.channels.remove(&u32::from(channel));
        Ok(())
    }
    async fn env_request(
        &mut self,
        channel: ChannelId,
        name: &str,
        value: &str,
        session: &mut Session,
    ) -> Result<()> {
        let accepted = if name == "GIT_PROTOCOL" {
            match codeyard_repos::validation::protocol(Some(value)) {
                Ok(protocol) => self
                    .channels
                    .get_mut(&u32::from(channel))
                    .map(|state| state.protocol = protocol.map(str::to_owned))
                    .is_some(),
                Err(_) => false,
            }
        } else {
            false
        };
        if accepted {
            let _ = session.channel_success(channel);
        } else {
            let _ = session.channel_failure(channel);
        }
        Ok(())
    }
    async fn subsystem_request(
        &mut self,
        channel: ChannelId,
        _name: &str,
        session: &mut Session,
    ) -> Result<()> {
        let _ = session.channel_failure(channel);
        let _ = session.close(channel);
        Ok(())
    }
    async fn pty_request(
        &mut self,
        channel: ChannelId,
        _term: &str,
        _width: u32,
        _height: u32,
        _pixel_width: u32,
        _pixel_height: u32,
        _modes: &[(russh::Pty, u32)],
        session: &mut Session,
    ) -> Result<()> {
        let _ = session.channel_failure(channel);
        let _ = session.close(channel);
        Ok(())
    }
    async fn exec_request(
        &mut self,
        channel: ChannelId,
        bytes: &[u8],
        session: &mut Session,
    ) -> Result<()> {
        let command = std::str::from_utf8(bytes)
            .ok()
            .filter(|command| codeyard_repos::validation::parse_command(command).is_ok())
            .map(str::to_owned);
        match command {
            Some(command) => {
                if self.start(channel, Some(command), session).await.is_err() {
                    Self::refuse(
                        channel,
                        session,
                        "The account's cell is unavailable. Please retry.",
                    );
                }
            }
            None => Self::refuse(channel, session, "Only Git commands are supported"),
        }
        Ok(())
    }
    async fn shell_request(&mut self, channel: ChannelId, session: &mut Session) -> Result<()> {
        if self.start(channel, None, session).await.is_err() {
            Self::refuse(
                channel,
                session,
                "The account's cell is unavailable. Please retry.",
            );
        }
        Ok(())
    }
}
struct Peer(Vec<String>);
impl russh::client::Handler for Peer {
    type Error = anyhow::Error;
    async fn check_server_key(&mut self, key: &PublicKeyOrCertificate) -> Result<bool> {
        let PublicKeyOrCertificate::PublicKey { key, .. } = key else {
            return Ok(false);
        };
        let fingerprint = key.fingerprint(russh::keys::HashAlg::Sha256);
        let fingerprint = fingerprint
            .as_bytes()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        // Registry node keys pin Ed25519 keys; SHA256 fingerprints also cover other host algorithms.
        let raw = key.to_bytes()?;
        let ed25519 = (key.algorithm().as_str() == "ssh-ed25519" && raw.len() == 51).then(|| {
            raw[19..]
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect::<String>()
        });
        Ok(self
            .0
            .iter()
            .any(|pin| pin == &fingerprint || ed25519.as_ref() == Some(pin)))
    }
}
async fn relay(
    address: SocketAddr,
    pins: Vec<String>,
    identity: Arc<PrivateKey>,
    key: PublicKey,
    mut input: DuplexStream,
    protocol: Option<String>,
    command: Option<String>,
    handle: Handle,
    id: ChannelId,
) -> Result<()> {
    let stream = tokio::time::timeout(
        Duration::from_secs(5),
        tokio::net::TcpStream::connect(address),
    )
    .await??;
    let (stream, control) = duplicate_socket(stream)?;
    let _control = ShutdownOnDrop(control);
    let config = russh::client::Config {
        inactivity_timeout: Some(Duration::from_secs(30)),
        ..Default::default()
    };
    let mut connection = tokio::time::timeout(
        Duration::from_secs(10),
        russh::client::connect_stream(Arc::new(config), stream, Peer(pins)),
    )
    .await??;
    let operation = async {
        ensure!(
            connection
                .authenticate_publickey(
                    "codeyard-router",
                    PrivateKeyWithHashAlg::new(identity, None)
                )
                .await?
                .success(),
            "Router authentication refused"
        );
        let mut channel = connection.channel_open_session().await?;
        channel
            .set_env(true, "CODEYARD_SSH_KEY", key.to_openssh()?)
            .await?;
        loop {
            match tokio::time::timeout(Duration::from_secs(10), channel.wait()).await? {
                Some(ChannelMsg::Success) => break,
                Some(ChannelMsg::Failure) | None => anyhow::bail!("SSH identity refused"),
                _ => {}
            }
        }
        if let Some(protocol) = protocol {
            channel.set_env(false, "GIT_PROTOCOL", protocol).await?;
        }
        match command {
            Some(command) => channel.exec(true, command).await?,
            None => channel.request_shell(true).await?,
        }
        let (mut reader, writer) = channel.split();
        let send = async {
            let mut total = 0u64;
            let mut buffer = vec![0; 65536];
            loop {
                let read = tokio::time::timeout(Duration::from_secs(30), input.read(&mut buffer))
                    .await??;
                if read == 0 {
                    writer.eof().await?;
                    return anyhow::Ok(());
                }
                total = total
                    .checked_add(read as u64)
                    .context("SSH input too large")?;
                ensure!(total <= 2 * 1024 * 1024 * 1024, "SSH input too large");
                writer
                    .data_bytes(Bytes::copy_from_slice(&buffer[..read]))
                    .await?;
            }
        };
        let receive = async {
            let mut total = 0u64;
            while let Some(message) =
                tokio::time::timeout(Duration::from_secs(30), reader.wait()).await?
            {
                match message {
                    ChannelMsg::Data { data } => {
                        total += data.len() as u64;
                        ensure!(total <= 64 * 1024 * 1024 * 1024, "SSH output too large");
                        handle
                            .data(id, data)
                            .await
                            .map_err(|_| anyhow::anyhow!("SSH client closed"))?;
                    }
                    ChannelMsg::ExtendedData { data, ext } => {
                        total += data.len() as u64;
                        ensure!(total <= 64 * 1024 * 1024 * 1024, "SSH output too large");
                        handle
                            .extended_data(id, ext, data)
                            .await
                            .map_err(|_| anyhow::anyhow!("SSH client closed"))?;
                    }
                    ChannelMsg::ExitStatus { exit_status } => {
                        let _ = handle.exit_status_request(id, exit_status).await;
                    }
                    ChannelMsg::Eof => {
                        let _ = handle.eof(id).await;
                    }
                    ChannelMsg::Close => break,
                    _ => {}
                }
            }
            let _ = handle.close(id).await;
            anyhow::Ok(())
        };
        tokio::try_join!(send, receive)?;
        anyhow::Ok(())
    }
    .await;
    let _ = connection
        .disconnect(russh::Disconnect::ByApplication, "", "")
        .await;
    let _ = tokio::time::timeout(Duration::from_secs(5), connection).await;
    operation
}
