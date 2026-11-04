//! Core addresses and the dialing-only context for a worker with no database.
use crate::cluster::context::NodeContext;
use codeyard_cluster::transport::NodeAddress;
use codeyard_cluster::settings::Settings;
use anyhow::ensure;
use anyhow::Context;
use anyhow::Result;
use codeyard_support::node_identity::load_or_create;
use codeyard_support::node_identity::parse_public_key;
use serde::Deserialize;
use serde::Serialize;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;
use std::sync::RwLock;

pub fn save_core(state_dir: &Path, ticket: &codeyard_cluster::pairing::JoinTicket) -> Result<()> {
    codeyard_support::fs::private_dir(state_dir)?;
    let peer = CorePeer {
        public_key: ticket.node.to_string(),
        address: ticket.address.clone(),
        target: None,
    };
    use std::io::Write;
    let mut file = tempfile::NamedTempFile::new_in(state_dir)?;
    file.write_all(&serde_json::to_vec(&peer)?)?;
    file.as_file().sync_all()?;
    file.persist(state_dir.join("cluster-peer.json"))?;
    // The rename is durable only once the directory is synced.
    std::fs::File::open(state_dir)?.sync_all()?;
    Ok(())
}

pub async fn worker_context(
    state_dir: &Path,
    settings: Settings,
) -> Result<(NodeContext, Vec<CorePeer>)> {
    ensure!(
        settings.declared.is_empty(),
        "CLUSTER_NODES belongs on core nodes, not a database-free runner"
    );
    let key = load_or_create(state_dir)?;
    let saved = state_dir.join("cluster-peer.json");
    let mut peers = Vec::new();
    for (public_key, host) in &settings.peers {
        // `tcp://host:port` selects TCP for this core node; any other entry uses iroh.
        let target = host.strip_prefix("tcp://").unwrap_or(host);
        if !target.rsplit_once(':').is_some_and(|(host, port)| {
            !host.is_empty() && port.parse::<u16>().is_ok_and(|port| port > 0)
        }) {
            tracing::warn!(host, "invalid configured core address; expected host:port");
            continue;
        }
        let addresses = match tokio::time::timeout(
            std::time::Duration::from_secs(5),
            resolve(target),
        )
        .await
        {
            Ok(Ok(addresses)) if !addresses.is_empty() => addresses,
            result => {
                tracing::warn!(host, error = ?result.err(), "core address unavailable; trying other peers");
                Vec::new()
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
            target: Some(host.clone()),
        });
    }
    let saved_peer = (|| -> Result<CorePeer> {
        let peer: CorePeer = serde_json::from_slice(&std::fs::read(&saved)?)?;
        ensure!(!peer.address.is_empty(), "saved core has no address");
        peer.endpoint_addr()?;
        Ok(peer)
    })();
    // A join token joins every time: the saved address says a core was found, not that
    // this key is enrolled, and joining again with an enrolled key changes nothing.
    if let Some(ticket) = &settings.join_ticket {
        let joined = codeyard_cluster::pairing::join(
            ticket,
            &key,
            &settings.relay,
            &settings.name,
            Some(&"runner".parse()?),
        )
        .await;
        match (joined, saved_peer) {
            (Ok(membership), _) => {
                ensure!(membership.status != "revoked", "this runner was revoked");
                save_core(state_dir, ticket)?;
                peers.push(CorePeer {
                    public_key: ticket.node.to_string(),
                    address: ticket.address.clone(),
                    target: None,
                });
            }
            (Err(error), Ok(peer)) => {
                tracing::warn!(%error, "cluster join failed; using the saved core address");
                peers.push(peer);
            }
            (Err(error), Err(_)) if !peers.is_empty() => {
                tracing::warn!(%error, "cluster join failed; trying configured cores");
            }
            (Err(error), Err(_)) => return Err(error),
        }
    } else if let Ok(peer) = saved_peer {
        peers.push(peer);
    }
    ensure!(
        !peers.is_empty(),
        "runner needs CLUSTER_PEERS or the core address saved by cluster join"
    );
    // The iroh endpoint opens only when a core node that needs it is dialed.
    Ok((
        NodeContext {
            pool: None,
            iroh: codeyard_cluster::transport::IrohDialer::new(settings.relay.clone(), key.clone()),
            key,
            node_id: 0,
            repos_path: state_dir.to_owned(),
            peers: Arc::new(RwLock::new(HashMap::new())),
            unreachable: Default::default(),
            ci_replicas: Default::default(),
        },
        peers,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn malformed_saved_core_does_not_override_configured_peers() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("cluster-peer.json"), b"{broken").unwrap();
        let key = codeyard_support::node_identity::NodeKey::generate().public_hex();
        let settings = Settings::from_lookup(&|name| {
            (name == "CLUSTER_PEERS").then(|| format!("{key}@tcp://127.0.0.1:7444"))
        })
        .unwrap()
        .unwrap();
        let (_, peers) = worker_context(root.path(), settings).await.unwrap();
        assert_eq!(peers.len(), 1);
        let ticket = codeyard_cluster::pairing::JoinTicket::new(
            peers[0].node().unwrap(),
            peers[0].address.clone(),
        )
        .unwrap();
        save_core(root.path(), &ticket).unwrap();
        let peer: CorePeer =
            serde_json::from_slice(&std::fs::read(root.path().join("cluster-peer.json")).unwrap())
                .unwrap();
        assert_eq!(peer.address, ticket.address);
        // A replacement does not truncate a file that another reader has open.
        let held = std::fs::File::open(root.path().join("cluster-peer.json")).unwrap();
        let replacement = codeyard_cluster::pairing::JoinTicket::new(
            codeyard_support::node_identity::NodeKey::generate().public_key(),
            ticket.address.clone(),
        )
        .unwrap();
        save_core(root.path(), &replacement).unwrap();
        use std::io::Read;
        let mut previous = Vec::new();
        (&held).read_to_end(&mut previous).unwrap();
        let previous: CorePeer = serde_json::from_slice(&previous).unwrap();
        assert_eq!(previous.public_key, ticket.node.to_string());
        let current: CorePeer =
            serde_json::from_slice(&std::fs::read(root.path().join("cluster-peer.json")).unwrap())
                .unwrap();
        assert_eq!(current.public_key, replacement.node.to_string());
    }

    #[tokio::test]
    async fn one_failed_resolution_keeps_usable_configured_and_saved_cores() {
        let root = tempfile::tempdir().unwrap();
        let key = codeyard_support::node_identity::NodeKey::generate().public_hex();
        let settings = Settings::from_lookup(&|name| {
            (name == "CLUSTER_PEERS")
                .then(|| format!("{key}@missing-port,{key}@tcp://127.0.0.1:7444"))
        })
        .unwrap()
        .unwrap();
        let saved = CorePeer {
            target: None,
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

use codeyard_cluster::worker::{CorePeer, resolve};
