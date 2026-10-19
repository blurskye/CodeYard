//! Core addresses and the dialing-only context for a worker with no database.
use super::{load_or_create, parse_public_key, NodeAddress, NodeContext, Settings};
use anyhow::{ensure, Context, Result};
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    net::SocketAddr,
    path::Path,
    sync::{Arc, RwLock},
};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CorePeer {
    pub public_key: String,
    pub address: NodeAddress,
}

impl CorePeer {
    pub fn node(&self) -> Result<iroh::PublicKey> {
        parse_public_key(&self.public_key)
    }

    pub fn endpoint_addr(&self) -> Result<iroh::EndpointAddr> {
        self.address.endpoint_addr(self.node()?)
    }
}

/// Resolves a `host:port` entry of CLUSTER_PEERS.
async fn resolve(host: &str) -> Result<Vec<SocketAddr>> {
    Ok(tokio::net::lookup_host(host)
        .await
        .with_context(|| format!("cannot resolve CLUSTER_PEERS address `{host}`"))?
        .collect())
}

pub fn save_core(state_dir: &Path, ticket: &super::JoinTicket) -> Result<()> {
    std::fs::create_dir_all(state_dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(state_dir, std::fs::Permissions::from_mode(0o700))?;
    }
    let peer = CorePeer {
        public_key: ticket.node.to_string(),
        address: ticket.address.clone(),
    };
    std::fs::write(
        state_dir.join("cluster-peer.json"),
        serde_json::to_vec(&peer)?,
    )?;
    Ok(())
}

pub async fn worker_context(
    state_dir: &Path,
    settings: Settings,
) -> Result<(NodeContext, Vec<CorePeer>)> {
    ensure!(
        settings.declared.is_empty(),
        "CLUSTER_NODES belongs on core nodes, not a database-free actions-worker"
    );
    let key = load_or_create(state_dir)?;
    let saved = state_dir.join("cluster-peer.json");
    let mut peers = Vec::new();
    for (public_key, host) in &settings.peers {
        // `tcp://host:port` selects TCP for this core node; any other entry uses iroh.
        let target = host.strip_prefix("tcp://").unwrap_or(host);
        let addresses = match resolve(target).await {
            Ok(addresses) if !addresses.is_empty() => addresses,
            result => {
                tracing::warn!(host, error = ?result.err(), "core address unavailable; trying other peers");
                continue;
            }
        };
        let address = if host.starts_with("tcp://") {
            NodeAddress {
                tcp: addresses,
                ..Default::default()
            }
        } else {
            NodeAddress {
                addrs: addresses,
                ..Default::default()
            }
        };
        peers.push(CorePeer {
            public_key: public_key.clone(),
            address,
        });
    }
    if saved.exists() {
        let peer: CorePeer = serde_json::from_slice(&std::fs::read(&saved)?)?;
        peer.endpoint_addr()?;
        peers.push(peer);
    } else if let Some(ticket) = &settings.join_ticket {
        let membership = super::join(
            ticket,
            &key,
            &settings.relay,
            &settings.name,
            Some(&"actions-worker".parse()?),
        )
        .await?;
        ensure!(
            membership.status != "revoked",
            "this actions-worker was revoked"
        );
        save_core(state_dir, ticket)?;
        peers.push(CorePeer {
            public_key: ticket.node.to_string(),
            address: ticket.address.clone(),
        });
    }
    ensure!(
        !peers.is_empty(),
        "actions-worker needs CLUSTER_PEERS or the core address saved by cluster join"
    );
    // The iroh endpoint opens only when a core node that needs it is dialed.
    Ok((
        NodeContext {
            pool: None,
            iroh: super::IrohDialer::new(settings.relay.clone(), key.clone()),
            key,
            node_id: 0,
            repos_path: state_dir.to_owned(),
            peers: Arc::new(RwLock::new(HashMap::new())),
            unreachable: Default::default(),
        },
        peers,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn one_failed_resolution_keeps_usable_configured_and_saved_cores() {
        let root = tempfile::tempdir().unwrap();
        let key = super::super::NodeKey::generate().public_hex();
        let settings = Settings::from_lookup(&|name| {
            (name == "CLUSTER_PEERS")
                .then(|| format!("{key}@missing-port,{key}@tcp://127.0.0.1:7444"))
        })
        .unwrap()
        .unwrap();
        let saved = CorePeer {
            public_key: key.clone(),
            address: NodeAddress {
                tcp: vec!["127.0.0.1:7445".parse().unwrap()],
                ..Default::default()
            },
        };
        std::fs::write(
            root.path().join("cluster-peer.json"),
            serde_json::to_vec(&saved).unwrap(),
        )
        .unwrap();
        let (_, peers) = worker_context(root.path(), settings).await.unwrap();
        assert_eq!(peers.len(), 2);
        assert_eq!(peers[0].address.tcp[0].port(), 7444);
        assert_eq!(peers[1].address.tcp[0].port(), 7445);
    }
}
