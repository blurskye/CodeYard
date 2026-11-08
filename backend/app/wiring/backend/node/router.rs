//! Router role wiring. It opens no cell database and owns both public listeners.
use anyhow::{ensure, Context, Result};
use codeyard_router::{
    directory::{Client, Reply, Request},
    routes::Routes,
};
use codeyard_support::node_identity::{load_or_create, NodeKey};
use std::{
    net::SocketAddr,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

#[derive(Clone)]
pub struct DirectorySettings {
    pub address: SocketAddr,
    pub key: iroh::PublicKey,
}
impl DirectorySettings {
    pub fn from_options(address: Option<SocketAddr>, key: Option<&str>) -> Result<Option<Self>> {
        match (address, key) {
            (None, None) => Ok(None),
            (Some(address), Some(key)) => Ok(Some(Self {
                address,
                key: codeyard_support::node_identity::parse_public_key(key)?,
            })),
            _ => anyhow::bail!(
                "DIRECTORY_ADDRESS and DIRECTORY_PUBLIC_KEY must be configured together"
            ),
        }
    }
    pub fn client(&self, state: &Path) -> Result<Client> {
        Client::new(
            load_or_create(state)?,
            self.address,
            self.key,
            Default::default(),
        )
    }
}
#[derive(Clone)]
pub struct Directory {
    pub client: Client,
    pub routes: Routes,
}
impl Directory {
    pub fn new(settings: &DirectorySettings, state: &Path) -> Result<Self> {
        let client = settings.client(state)?;
        Ok(Self {
            routes: Routes::new(client.clone(), Default::default())?,
            client,
        })
    }
}
impl codeyard_ssh::handler::RouterAuthorization for Directory {
    fn authorize<'a>(
        &'a self,
        key: &'a russh::keys::PublicKey,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<bool>> + Send + 'a>> {
        Box::pin(async move {
            let bytes = key.to_bytes()?;
            if key.algorithm().as_str() != "ssh-ed25519" || bytes.len() != 51 {
                return Ok(false);
            }
            let public_key = hex::encode(&bytes[19..]);
            match self
                .client
                .request(&Request::RouterKey { public_key })
                .await?
            {
                Reply::RouterKey(enabled) => Ok(enabled),
                _ => anyhow::bail!("Invalid directory router authorization response"),
            }
        })
    }
}
impl codeyard_ssh::router::Directory for Directory {
    fn resolve<'a>(
        &'a self,
        owner: Option<&'a str>,
        fingerprint: &'a str,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<Output = Result<Option<codeyard_ssh::router::Target>>>
                + Send
                + 'a,
        >,
    > {
        Box::pin(async move {
            let cell = match owner {
                Some(owner) => self.routes.by_name(owner).await?,
                None => {
                    self.routes
                        .by_key(codeyard_types::cells::KeyKind::Ssh, fingerprint)
                        .await?
                }
            };
            let Some(cell) = cell else { return Ok(None) };
            Ok(Some(codeyard_ssh::router::Target {
                addresses: cell
                    .ssh_addresses
                    .iter()
                    .map(|address| address.parse())
                    .collect::<std::result::Result<Vec<_>, _>>()?,
                pins: cell.node_keys.clone(),
            }))
        })
    }
}
pub struct Config {
    pub directory: DirectorySettings,
    pub region: String,
    pub state: PathBuf,
    pub host_key: PathBuf,
    pub http: SocketAddr,
    pub ssh: Option<SocketAddr>,
}
pub async fn serve(config: Config) -> Result<()> {
    codeyard_support::fs::private_dir(&config.state)?;
    let _state = codeyard_support::fs::lock_state_dir(&config.state)?;
    let key = load_or_create(&config.state)?;
    let directory = Directory::new(&config.directory, &config.state)?;
    ensure!(
        matches!(
            directory
                .client
                .request(&Request::RouterKey {
                    public_key: key.public_hex()
                })
                .await
                .with_context(|| format!(
                    "Enroll router key {} in the directory",
                    key.public_hex()
                ))?,
            Reply::RouterKey(true)
        ),
        "Router key is not enrolled as a read-only router"
    );
    let transport = codeyard_router::http::Transport::new(Default::default())?;
    let frontend = codeyard_router::frontend::Frontend::new(
        directory.routes.clone(),
        transport.clone(),
        codeyard_api_rest::routing_catalog().map_err(anyhow::Error::msg)?,
        codeyard_router::frontend::Config {
            region: config.region,
            ..Default::default()
        },
    )?;
    let http = tokio::net::TcpListener::bind(config.http).await?;
    let ssh = match config.ssh {
        Some(address) => Some(tokio::net::TcpListener::bind(address).await?),
        None => None,
    };
    let mut ssh_server = match &ssh {
        Some(_) => {
            let identity = router_identity(&key);
            let handler = codeyard_ssh::router::Handler::new(Arc::new(directory), identity);
            Some(codeyard_ssh::server::SshServer::with_handler(
                config
                    .host_key
                    .to_str()
                    .context("Host key path is not UTF-8")?,
                handler,
            )?)
        }
        None => None,
    };
    let (stopping, stop) = tokio::sync::watch::channel(false);
    let http_stop = stop.clone();
    let http_server = async move {
        let mut stop = http_stop;
        axum::serve(
            http,
            frontend
                .router()
                .into_make_service_with_connect_info::<SocketAddr>(),
        )
        .with_graceful_shutdown(async move {
            let _ = stop.wait_for(|stopped| *stopped).await;
        })
        .await
        .map_err(anyhow::Error::from)
    };
    let ssh_server = async move {
        match (ssh, ssh_server.as_mut()) {
            (Some(listener), Some(server)) => server.run_listener(listener, stop.clone()).await,
            _ => {
                let mut stop = stop.clone();
                let _ = stop.wait_for(|stopped| *stopped).await;
                Ok(())
            }
        }
    };
    let mut servers = Box::pin(async { tokio::try_join!(http_server, ssh_server).map(|_| ()) });
    tracing::info!(http=%config.http, "router listening");
    let (result, signalled) = tokio::select! {
        result=&mut servers=>(result.and_then(|()|Err(anyhow::anyhow!("A router listener stopped unexpectedly"))),false),
        signal=super::runtime::shutdown_signal()=>(signal,true),
    };
    stopping.send_replace(true);
    if signalled {
        let _ = tokio::time::timeout(Duration::from_secs(30), &mut servers).await;
    }
    drop(servers);
    transport.shutdown().await;
    result
}
fn router_identity(key: &NodeKey) -> russh::keys::PrivateKey {
    russh::keys::PrivateKey::from(russh::keys::ssh_key::private::Ed25519Keypair::from_seed(
        &key.iroh_secret().to_bytes(),
    ))
}
