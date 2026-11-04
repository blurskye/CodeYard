use crate::transport::NodeAddress;
use anyhow::{ensure, Context, Result};
use codeyard_support::node_identity::parse_public_key;
use serde::{Deserialize, Serialize};
use std::net::SocketAddr;
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CorePeer {
    pub public_key: String,
    pub address: NodeAddress,
    /// Configured hostname retained even when startup DNS is unavailable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
}

impl CorePeer {
    pub fn node(&self) -> Result<iroh::PublicKey> {
        parse_public_key(&self.public_key)
    }

    pub async fn resolved_address(&self) -> Result<NodeAddress> {
        let Some(host) = &self.target else {
            return Ok(self.address.clone());
        };
        let target = host.strip_prefix("tcp://").unwrap_or(host);
        let addresses =
            tokio::time::timeout(std::time::Duration::from_secs(5), resolve(target)).await??;
        ensure!(!addresses.is_empty(), "core hostname has no addresses");
        Ok(if host.starts_with("tcp://") {
            NodeAddress {
                tcp: addresses,
                ..Default::default()
            }
        } else {
            NodeAddress {
                addrs: addresses,
                ..Default::default()
            }
        })
    }

    pub fn endpoint_addr(&self) -> Result<iroh::EndpointAddr> {
        self.address.endpoint_addr(self.node()?)
    }
}

/// Resolves a `host:port` entry of CLUSTER_PEERS.
pub async fn resolve(host: &str) -> Result<Vec<SocketAddr>> {
    Ok(tokio::net::lookup_host(host)
        .await
        .with_context(|| format!("cannot resolve CLUSTER_PEERS address `{host}`"))?
        .collect())
}

