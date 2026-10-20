//! Pairing: how a machine becomes a cluster node. Every route ends with the node's
//! public key in `nodes`:
//!
//! - an invite ticket from `cluster invite`, redeemed by `cluster join`;
//! - the same ticket kept as `CLUSTER_JOIN_TOKEN` (reusable, for autoscaling);
//! - a declared list of keys in `CLUSTER_NODES`, applied by core nodes at startup.
use super::{
    identity::{parse_public_key, NodeKey},
    link::{read_to_end_within, Link},
    transport::{self, NodeAddress, Relay, ALPN_PAIR, CLOSE_UNAVAILABLE},
};
use crate::backend::{
    db::{
        self,
        cluster::{Authority, DeclaredMember, JoinRequest, NewJoinToken},
        models::{JoinOutcome, NodeStatusChange},
        DbPool,
    },
    node::NodeRoles,
};
use anyhow::{anyhow, bail, ensure, Context, Result};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use chrono::Utc;
use iroh::{
    endpoint::Connection,
    protocol::{AcceptError, ProtocolHandler},
    PublicKey,
};
use rand::RngCore;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{collections::HashSet, fmt, net::SocketAddr, sync::Arc, time::Duration};
use tokio::{
    io::AsyncWriteExt,
    sync::{OwnedSemaphorePermit, Semaphore},
};
use tracing::{debug, info, warn};

const TICKET_PREFIX: &str = "codeyard-join-v1.";
const MAX_TICKET_LEN: usize = 4096;
/// Mixed into the secret before hashing so the stored hash is only good for this purpose.
const HASH_CONTEXT: &[u8] = b"codeyard join token v1\0";
/// A pairing request or answer is a few hundred bytes; anything bigger is refused.
const MAX_MESSAGE: usize = 4096;
pub(crate) const EXCHANGE_TIMEOUT: Duration = Duration::from_secs(15);
/// After answering, wait this long for the peer to hang up so the answer is delivered.
const HANGUP_WAIT: Duration = Duration::from_secs(3);
/// Pairing is open to keys we do not know yet, so only a few run at a time.
const MAX_PAIRINGS_AT_ONCE: usize = 16;
/// The longest an invite may stay valid.
pub const MAX_INVITE_LIFETIME: Duration = Duration::from_secs(365 * 24 * 60 * 60);

pub const NAME_RULE: &str = "node names are 1-63 characters: lowercase letters, digits, '.', '_' and '-', starting with a letter or digit";
const INVALID_INVITE: &str = "this invite is not valid: it is unknown, expired or already used";

/// Checks a node name and returns it in lowercase.
pub fn normalize_name(raw: &str) -> Result<String> {
    let name = raw.trim().to_ascii_lowercase();
    let allowed = |c: char| c.is_ascii_lowercase() || c.is_ascii_digit();
    let valid = (1..=63).contains(&name.len())
        && name.chars().next().is_some_and(allowed)
        && name
            .chars()
            .all(|c| allowed(c) || matches!(c, '.' | '_' | '-'));
    ensure!(valid, "{NAME_RULE}");
    Ok(name)
}

/// Reads a lifetime such as `90s`, `30m`, `1h`, `7d` or `2w`.
pub fn parse_duration(text: &str) -> Result<Duration> {
    let text = text.trim();
    let split = text
        .find(|c: char| !c.is_ascii_digit())
        .filter(|&at| at > 0)
        .ok_or_else(|| anyhow!("write a lifetime like 30m, 1h, 7d or 2w"))?;
    let (number, unit) = text.split_at(split);
    let number: u64 = number
        .parse()
        .map_err(|_| anyhow!("that number is too large"))?;
    let unit_seconds: u64 = match unit {
        "s" => 1,
        "m" => 60,
        "h" => 3600,
        "d" => 86_400,
        "w" => 604_800,
        _ => bail!("the unit must be s, m, h, d or w"),
    };
    let seconds = number
        .checked_mul(unit_seconds)
        .ok_or_else(|| anyhow!("that lifetime is too long"))?;
    ensure!(seconds > 0, "the lifetime must be more than zero");
    ensure!(
        seconds <= MAX_INVITE_LIFETIME.as_secs(),
        "an invite can be valid for at most 365d"
    );
    Ok(Duration::from_secs(seconds))
}

/// Hex SHA-256 of an invite secret. Only this is stored in `join_tokens`.
pub fn hash_secret(secret: &[u8; 32]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(HASH_CONTEXT);
    hasher.update(secret);
    hex::encode(hasher.finalize())
}

fn decode_secret(text: &str) -> Option<[u8; 32]> {
    hex::decode(text.trim()).ok()?.try_into().ok()
}

/// What `cluster invite` prints: where to find a core node and the secret that
/// proves the holder was invited.
#[derive(Clone, PartialEq, Eq)]
pub struct JoinTicket {
    pub node: PublicKey,
    pub address: NodeAddress,
    secret: [u8; 32],
}

impl fmt::Debug for JoinTicket {
    // The secret stays out of logs and test output.
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("JoinTicket")
            .field("node", &self.node)
            .field("address", &self.address)
            .finish_non_exhaustive()
    }
}

#[derive(Serialize, Deserialize)]
struct TicketBody {
    node: String,
    addrs: Vec<SocketAddr>,
    relays: Vec<String>,
    /// Absent in tickets written before TCP existed.
    #[serde(default)]
    tcp: Vec<SocketAddr>,
    secret: String,
}

impl JoinTicket {
    /// A ticket for the core node `node`, with a fresh random secret.
    pub fn new(node: PublicKey, address: NodeAddress) -> Self {
        let mut secret = [0u8; 32];
        rand::rngs::OsRng.fill_bytes(&mut secret);
        Self {
            node,
            address,
            secret,
        }
    }

    pub fn secret_hash(&self) -> String {
        hash_secret(&self.secret)
    }

    pub fn encode(&self) -> String {
        let body = TicketBody {
            node: self.node.to_string(),
            addrs: self.address.addrs.clone(),
            relays: self.address.relays.clone(),
            tcp: self.address.tcp.clone(),
            secret: hex::encode(self.secret),
        };
        let json = serde_json::to_vec(&body).expect("a ticket is plain strings and addresses");
        format!("{TICKET_PREFIX}{}", URL_SAFE_NO_PAD.encode(json))
    }

    pub fn decode(text: &str) -> Result<Self> {
        let not_a_ticket = || anyhow!("that is not a Codeyard join ticket");
        let text = text.trim();
        ensure!(
            text.len() <= MAX_TICKET_LEN,
            "that is not a Codeyard join ticket"
        );
        let payload = text.strip_prefix(TICKET_PREFIX).ok_or_else(not_a_ticket)?;
        let json = URL_SAFE_NO_PAD
            .decode(payload)
            .map_err(|_| not_a_ticket())?;
        let body: TicketBody = serde_json::from_slice(&json).map_err(|_| not_a_ticket())?;
        let node = parse_public_key(&body.node).map_err(|_| not_a_ticket())?;
        let secret = decode_secret(&body.secret).ok_or_else(not_a_ticket)?;
        let address = NodeAddress {
            addrs: body.addrs,
            relays: body.relays,
            tcp: body.tcp,
        };
        ensure!(
            !address.is_empty(),
            "that join ticket has no address to connect to"
        );
        // Fail now, not in the middle of connecting, if the relay entries are unusable.
        address.endpoint_addr(node)?;
        Ok(Self {
            node,
            address,
            secret,
        })
    }
}

pub struct InviteOptions {
    pub roles: NodeRoles,
    /// A reusable invite admits any number of nodes until it expires.
    pub reusable: bool,
    /// New nodes wait as `pending` until someone runs `cluster approve`.
    pub requires_approval: bool,
    pub expires_in: Duration,
}

/// Records a new invite and returns the ticket for it. `node` and `address` say
/// where the joining machine should connect: this core node.
pub async fn create_invite(
    pool: &DbPool,
    node: PublicKey,
    address: NodeAddress,
    options: &InviteOptions,
) -> Result<JoinTicket> {
    ensure!(
        !address.is_empty(),
        "this node has no address to put in a ticket"
    );
    ensure!(
        !options.expires_in.is_zero() && options.expires_in <= MAX_INVITE_LIFETIME,
        "an invite can be valid for at most 365d"
    );
    let ticket = JoinTicket::new(node, address);
    ensure!(
        ticket.encode().len() <= MAX_TICKET_LEN,
        "too many advertised addresses for a join ticket (maximum {MAX_TICKET_LEN} bytes)"
    );
    let expires_at = Utc::now() + chrono::Duration::from_std(options.expires_in)?;
    db::cluster::create_join_token(
        pool,
        NewJoinToken {
            token_hash: &ticket.secret_hash(),
            roles: &options.roles.names(),
            reusable: options.reusable,
            requires_approval: options.requires_approval,
            expires_at,
        },
    )
    .await?;
    Ok(ticket)
}

#[derive(Serialize, Deserialize)]
struct JoinBody {
    secret: String,
    name: String,
    /// Roles the node wants; left out it takes everything the invite allows.
    #[serde(default)]
    roles: Option<Vec<String>>,
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "result", rename_all = "snake_case")]
enum JoinReply {
    Joined {
        name: String,
        roles: Vec<String>,
        status: String,
    },
    AlreadyMember {
        name: String,
        roles: Vec<String>,
        status: String,
    },
    Refused {
        reason: String,
    },
}

/// The standing a node has in the cluster after pairing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Membership {
    pub name: String,
    pub roles: NodeRoles,
    /// `active`, or `pending` until an administrator approves it.
    pub status: String,
    /// False when the key was in the cluster already and nothing was changed.
    pub newly_joined: bool,
}

/// Connects to the core node in `ticket`, proves the secret and registers this node.
/// `roles` limits what the node asks for; `None` takes everything the invite allows.
pub async fn join(
    ticket: &JoinTicket,
    key: &NodeKey,
    relay: &Relay,
    name: &str,
    roles: Option<&NodeRoles>,
) -> Result<Membership> {
    let name = normalize_name(name)?;
    if relay.is_off() && ticket.address.addrs.is_empty() && ticket.address.tcp.is_empty() {
        bail!("this ticket can only be reached through a relay; set IROH_RELAY to a relay URL, or to n0 for the public relays");
    }
    // The dialer opens an iroh endpoint only if a peer needs one, so a ticket with TCP
    // addresses alone opens no socket.
    let iroh = transport::IrohDialer::new(relay.clone(), key.clone());
    let reply = request_join(&iroh, ticket, &name, roles).await;
    iroh.close().await;
    match reply? {
        JoinReply::Joined {
            name,
            roles,
            status,
        } => membership(name, roles, status, true),
        JoinReply::AlreadyMember {
            name,
            roles,
            status,
        } => membership(name, roles, status, false),
        JoinReply::Refused { reason } => bail!("the cluster refused to add this node: {reason}"),
    }
}

fn membership(
    name: String,
    roles: Vec<String>,
    status: String,
    newly_joined: bool,
) -> Result<Membership> {
    Ok(Membership {
        name,
        roles: roles.join(",").parse()?,
        status,
        newly_joined,
    })
}

async fn request_join(
    iroh: &transport::IrohDialer,
    ticket: &JoinTicket,
    name: &str,
    roles: Option<&NodeRoles>,
) -> Result<JoinReply> {
    let request = serde_json::to_vec(&JoinBody {
        secret: hex::encode(ticket.secret),
        name: name.to_owned(),
        roles: roles.map(NodeRoles::names),
    })?;
    // dial_node bounds each attempt itself, so the join needs no timeout of its own.
    let mut link = transport::dial_node(iroh, ticket.node, &ticket.address, ALPN_PAIR)
        .await
        .context("could not reach the cluster node named in the ticket")?;
    let answer = tokio::time::timeout(EXCHANGE_TIMEOUT, async {
        link.writer.write_all(&request).await?;
        link.finish().await?;
        let answer = read_to_end_within(&mut link.reader, MAX_MESSAGE).await?;
        anyhow::Ok(answer)
    })
    .await
    .map_err(|_| anyhow!("timed out waiting for the cluster's answer"))??;
    link.close(0, b"done");
    serde_json::from_slice(&answer).context("the cluster sent an unreadable answer")
}

/// Core-node side of pairing. Open to keys that are not members yet, so it does one
/// thing only: read one request, answer it, and hang up.
#[derive(Clone)]
pub struct PairingHandler {
    pool: DbPool,
    slots: Arc<Semaphore>,
}

impl fmt::Debug for PairingHandler {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PairingHandler")
            .finish_non_exhaustive()
    }
}

impl PairingHandler {
    pub fn new(pool: DbPool) -> Self {
        Self {
            pool,
            slots: Arc::new(Semaphore::new(MAX_PAIRINGS_AT_ONCE)),
        }
    }

    /// Reads the request on `link` and writes the answer.
    async fn serve_one(&self, link: &mut Link) -> Result<()> {
        let request = read_to_end_within(&mut link.reader, MAX_MESSAGE).await?;
        let reply = self.decide(link.peer, &request).await;
        link.writer.write_all(&serde_json::to_vec(&reply)?).await?;
        link.finish().await?;
        Ok(())
    }

    /// A slot for one pairing exchange, or `None` when all of them are in use.
    pub(crate) fn slot(&self) -> Option<OwnedSemaphorePermit> {
        self.slots.clone().try_acquire_owned().ok()
    }

    /// Serves the pairing request on `link`. The exchange must finish by `deadline`, which
    /// the caller set before it waited for the stream. `_slot` is held until the exchange
    /// and the wait for the peer to hang up are over.
    pub(crate) async fn serve_link(
        &self,
        mut link: Link,
        deadline: tokio::time::Instant,
        _slot: OwnedSemaphorePermit,
    ) {
        match tokio::time::timeout_at(deadline, self.serve_one(&mut link)).await {
            Ok(Ok(())) => {
                // Dropping the connection now could lose the answer in flight.
                let _ = tokio::time::timeout(HANGUP_WAIT, link.wait_for_peer_close()).await;
            }
            Ok(Err(error)) => {
                debug!(%error, "pairing exchange failed");
                link.close(1, b"bad pairing request");
            }
            Err(_) => link.close(1, b"pairing timed out"),
        }
    }

    /// Turns a request into an answer. The answer for a bad secret never says whether
    /// the invite exists, expired, or was used.
    async fn decide(&self, peer: PublicKey, request: &[u8]) -> JoinReply {
        let refuse = |reason: &str| JoinReply::Refused {
            reason: reason.to_owned(),
        };
        let Ok(body) = serde_json::from_slice::<JoinBody>(request) else {
            return refuse("the request could not be read");
        };
        let Ok(name) = normalize_name(&body.name) else {
            return refuse(NAME_RULE);
        };
        let roles = match body.roles {
            None => None,
            Some(list) => match list.join(",").parse::<NodeRoles>() {
                Ok(roles) => Some(roles.names()),
                Err(_) => return refuse("unknown role requested"),
            },
        };
        let Some(secret) = decode_secret(&body.secret) else {
            return refuse(INVALID_INVITE);
        };
        let public_key = peer.to_string();
        let outcome = db::cluster::join_with_token(
            &self.pool,
            JoinRequest {
                token_hash: &hash_secret(&secret),
                public_key: &public_key,
                name: &name,
                roles: roles.as_deref(),
                version: env!("CARGO_PKG_VERSION"),
            },
        )
        .await;
        match outcome {
            Ok(JoinOutcome::Joined(node)) => {
                info!(name = %node.name, status = %node.status, key = %peer.fmt_short(), "node joined the cluster");
                JoinReply::Joined {
                    name: node.name,
                    roles: node.roles,
                    status: node.status,
                }
            }
            Ok(JoinOutcome::AlreadyMember(node)) => JoinReply::AlreadyMember {
                name: node.name,
                roles: node.roles,
                status: node.status,
            },
            Ok(JoinOutcome::InvalidToken) => {
                warn!(key = %peer.fmt_short(), "pairing refused: invite not valid");
                refuse(INVALID_INVITE)
            }
            Ok(JoinOutcome::RolesNotAllowed { allowed }) => refuse(&format!(
                "this invite allows only these roles: {}",
                allowed.join(",")
            )),
            Ok(JoinOutcome::NameTaken) => refuse(&format!(
                "a node named '{name}' already exists; choose another name (NODE_NAME)"
            )),
            Err(error) => {
                warn!(%error, "could not record a joining node");
                refuse("the cluster could not record this node; try again later")
            }
        }
    }
}

impl ProtocolHandler for PairingHandler {
    async fn accept(&self, connection: Connection) -> Result<(), AcceptError> {
        // The slot is taken before the request stream is awaited, as it always was: a peer
        // that never opens a stream holds a slot, and overload is refused at once.
        let Some(slot) = self.slot() else {
            connection.close(CLOSE_UNAVAILABLE.into(), b"too many pairing requests");
            return Ok(());
        };
        // One request stream per connection. Waiting for that stream counts against the
        // exchange deadline, as it always has.
        let deadline = tokio::time::Instant::now() + EXCHANGE_TIMEOUT;
        match tokio::time::timeout_at(deadline, transport::accept_stream(&connection)).await {
            Ok(Some(link)) => self.serve_link(link, deadline, slot).await,
            Ok(None) => connection.close(1u32.into(), b"bad pairing request"),
            Err(_) => connection.close(1u32.into(), b"pairing timed out"),
        }
        Ok(())
    }
}

/// One entry of `CLUSTER_NODES`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeclaredNode {
    pub public_key: String,
    pub roles: NodeRoles,
    pub name: String,
}

/// Reads `<public key>:<roles>:<name>` entries separated by commas or new lines.
/// Roles are joined with `+` (for example `web+ssh`) because commas separate entries.
/// `#` starts a comment.
pub fn parse_declared_nodes(text: &str) -> Result<Vec<DeclaredNode>> {
    let mut nodes: Vec<DeclaredNode> = Vec::new();
    for line in text.lines() {
        let line = line.split('#').next().unwrap_or_default();
        for entry in line
            .split(',')
            .map(str::trim)
            .filter(|entry| !entry.is_empty())
        {
            let parts: Vec<&str> = entry.split(':').collect();
            let [key, roles, name] = parts[..] else {
                bail!("`{entry}` should look like <public key>:<roles>:<name>, with roles joined by + (for example web+ssh)");
            };
            let node = DeclaredNode {
                public_key: parse_public_key(key)
                    .with_context(|| format!("entry `{entry}`"))?
                    .to_string(),
                roles: roles
                    .replace('+', ",")
                    .parse()
                    .with_context(|| format!("roles in entry `{entry}`"))?,
                name: normalize_name(name).with_context(|| format!("name in entry `{entry}`"))?,
            };
            ensure!(
                nodes
                    .iter()
                    .all(|other| other.public_key != node.public_key),
                "the public key of `{}` is listed twice",
                node.name
            );
            ensure!(
                nodes.iter().all(|other| other.name != node.name),
                "the name `{}` is listed twice",
                node.name
            );
            nodes.push(node);
        }
    }
    Ok(nodes)
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct DeclaredSummary {
    /// Entries now active with the declared name and roles.
    pub applied: usize,
    /// Declared nodes that stay revoked: revoking wins until someone approves again.
    pub kept_revoked: Vec<String>,
}

/// Makes every declared node an active member with the declared name and roles.
/// Nodes missing from the list are left alone; use `cluster revoke` to remove one.
/// The list is applied in one transaction, so a failing entry applies none of it.
pub async fn apply_declared_nodes(
    pool: &DbPool,
    nodes: &[DeclaredNode],
) -> Result<DeclaredSummary> {
    let mut seen = HashSet::new();
    for declared in nodes {
        ensure!(seen.insert(&declared.public_key), "a key is declared twice");
    }
    let mut members: Vec<DeclaredMember> = nodes
        .iter()
        .map(|declared| DeclaredMember {
            public_key: declared.public_key.clone(),
            name: declared.name.clone(),
            roles: declared.roles.names(),
        })
        .collect();
    // Two core nodes that start together lock the rows in the same order, so they cannot deadlock.
    members.sort_by(|a, b| a.public_key.cmp(&b.public_key));
    let rows = db::cluster::upsert_declared_nodes(pool, &members)
        .await
        .context("cannot apply the declared node list")?;
    let mut summary = DeclaredSummary::default();
    for node in rows {
        if node.status == "revoked" {
            summary.kept_revoked.push(node.name);
        } else {
            summary.applied += 1;
        }
    }
    Ok(summary)
}

/// Makes a pending or revoked node active. `Authority::Administrator` must still be an
/// active administrator when the change commits.
pub async fn approve(
    pool: &DbPool,
    authority: Authority<'_>,
    name: &str,
) -> Result<NodeStatusChange> {
    db::cluster::set_node_status(pool, authority, &normalize_name(name)?, "active").await
}

/// Stops a node from connecting. Its next connection attempt is refused.
pub async fn revoke(
    pool: &DbPool,
    authority: Authority<'_>,
    name: &str,
) -> Result<NodeStatusChange> {
    db::cluster::set_node_status(pool, authority, &normalize_name(name)?, "revoked").await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn address() -> NodeAddress {
        NodeAddress {
            addrs: vec!["203.0.113.7:7444".parse().unwrap()],
            relays: vec!["https://relay.example.com/".into()],
            tcp: vec!["203.0.113.7:7445".parse().unwrap()],
        }
    }

    #[test]
    fn tickets_written_before_tcp_still_decode() {
        // The body as the release before TCP wrote it: there is no `tcp` key.
        let body = serde_json::json!({
            "node": NodeKey::generate().public_hex(),
            "addrs": ["203.0.113.7:7444"],
            "relays": [],
            "secret": hex::encode([7_u8; 32]),
        });
        let text = format!(
            "{TICKET_PREFIX}{}",
            URL_SAFE_NO_PAD.encode(serde_json::to_vec(&body).unwrap())
        );
        let ticket = JoinTicket::decode(&text).unwrap();
        assert!(ticket.address.tcp.is_empty());
        assert_eq!(
            ticket.address.addrs,
            vec!["203.0.113.7:7444".parse::<SocketAddr>().unwrap()]
        );
        let again = JoinTicket::decode(&ticket.encode()).unwrap();
        assert_eq!(again.address, ticket.address);
        assert_eq!(again.node, ticket.node);
    }

    #[test]
    fn ticket_round_trips_with_address_and_secret() {
        let node = NodeKey::generate().public_key();
        let ticket = JoinTicket::new(node, address());
        let text = ticket.encode();
        assert!(text.starts_with("codeyard-join-v1."));
        assert!(!text.contains(['+', '/', '=', ' ', '\n']));
        let back = JoinTicket::decode(&text).unwrap();
        assert_eq!(back, ticket);
        assert_eq!(back.node, node);
        assert_eq!(back.address, address());
        assert_eq!(back.secret_hash(), ticket.secret_hash());
        // Pasting from a terminal often adds whitespace.
        assert_eq!(JoinTicket::decode(&format!("  {text}\n")).unwrap(), ticket);
    }

    #[test]
    fn every_ticket_has_its_own_secret() {
        let node = NodeKey::generate().public_key();
        let first = JoinTicket::new(node, address());
        let second = JoinTicket::new(node, address());
        assert_ne!(first.encode(), second.encode());
        assert_ne!(first.secret_hash(), second.secret_hash());
    }

    #[test]
    fn damaged_or_foreign_tickets_are_rejected() {
        let node = NodeKey::generate().public_key();
        let good = JoinTicket::new(node, address()).encode();
        let payload = good.strip_prefix(TICKET_PREFIX).unwrap();
        let encode = |json: String| format!("{TICKET_PREFIX}{}", URL_SAFE_NO_PAD.encode(json));
        let key = node.to_string();
        let secret = "ab".repeat(32);
        let cases = vec![
            String::new(),
            "hello".into(),
            format!("2.{payload}"),
            payload.to_owned(),
            format!("{TICKET_PREFIX}!!!not-base64!!!"),
            format!("{TICKET_PREFIX}{}", &payload[..payload.len() / 2]),
            "x".repeat(MAX_TICKET_LEN + 1),
            encode("{}".into()),
            encode(format!(
                r#"{{"node":"{key}","addrs":[],"relays":[],"secret":"{secret}"}}"#
            )),
            encode(format!(
                r#"{{"node":"{key}","addrs":["1.2.3.4:5"],"relays":[],"secret":"abcd"}}"#
            )),
            encode(format!(
                r#"{{"node":"short","addrs":["1.2.3.4:5"],"relays":[],"secret":"{secret}"}}"#
            )),
            encode(format!(
                r#"{{"node":"{key}","addrs":[],"relays":["not a url"],"secret":"{secret}"}}"#
            )),
        ];
        for case in cases {
            assert!(JoinTicket::decode(&case).is_err(), "accepted {case:?}");
        }
    }

    #[test]
    fn debug_output_hides_the_secret() {
        let ticket = JoinTicket::new(NodeKey::generate().public_key(), address());
        let shown = format!("{ticket:?}");
        assert!(!shown.contains(&hex::encode(ticket.secret)));
        assert!(!shown.contains("secret"));
    }

    #[test]
    fn token_hash_is_stable_hex_and_depends_on_every_byte() {
        let secret = [7u8; 32];
        let hash = hash_secret(&secret);
        assert_eq!(hash, hash_secret(&secret));
        assert_eq!(hash.len(), 64);
        assert!(hash
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)));
        let mut other = secret;
        other[31] ^= 1;
        assert_ne!(hash, hash_secret(&other));
        // The stored value is not the plain SHA-256 of the secret.
        assert_ne!(hash, hex::encode(Sha256::digest(secret)));
        // Known answer, so the hashing cannot change unnoticed: existing invites
        // would stop working.
        assert_eq!(
            hash_secret(&[0u8; 32]),
            "0115d9a01c49e2a9f5faa7d893363f5d4dc6e0a0134d37131f3cf7dee4879abb"
        );
    }

    #[test]
    fn durations_have_units_and_limits() {
        assert_eq!(parse_duration("90s").unwrap(), Duration::from_secs(90));
        assert_eq!(parse_duration("30m").unwrap(), Duration::from_secs(1800));
        assert_eq!(parse_duration("1h").unwrap(), Duration::from_secs(3600));
        assert_eq!(
            parse_duration("30d").unwrap(),
            Duration::from_secs(30 * 86_400)
        );
        assert_eq!(
            parse_duration("2w").unwrap(),
            Duration::from_secs(14 * 86_400)
        );
        assert_eq!(parse_duration("365d").unwrap().as_secs(), 365 * 86_400);
        for bad in [
            "",
            "d",
            "30",
            "0s",
            "-1h",
            "1.5h",
            "1 h",
            "30x",
            "366d",
            "99999999999999999999d",
            "18446744073709551615w",
        ] {
            assert!(parse_duration(bad).is_err(), "accepted {bad:?}");
        }
    }

    #[test]
    fn node_names_are_lowercased_and_restricted() {
        assert_eq!(
            normalize_name(" Store-1.example ").unwrap(),
            "store-1.example"
        );
        assert!(normalize_name("a").is_ok());
        assert!(normalize_name(&"a".repeat(63)).is_ok());
        for bad in [
            "",
            " ",
            "-a",
            ".a",
            "_a",
            "a b",
            "a:b",
            "a,b",
            "a/b",
            "ä",
            &"a".repeat(64),
        ] {
            assert!(normalize_name(bad).is_err(), "accepted {bad:?}");
        }
    }

    #[test]
    fn declared_nodes_parse_from_env_style_and_file_style_text() {
        let first = NodeKey::generate().public_hex();
        let second = NodeKey::generate().public_hex();
        let third = NodeKey::generate().public_hex();
        let text = format!(
            "{first}:web+ssh:Core-1,{second}:storage:store-1\n# the build farm\n\n{third}:actions-worker:ci-1 # trailing comment\n"
        );
        let nodes = parse_declared_nodes(&text).unwrap();
        assert_eq!(nodes.len(), 3);
        assert_eq!(nodes[0].public_key, first);
        assert_eq!(nodes[0].name, "core-1");
        assert_eq!(nodes[0].roles.names(), ["web", "ssh"]);
        assert_eq!(nodes[1].roles.to_string(), "storage");
        assert_eq!(nodes[2].name, "ci-1");
        assert!(parse_declared_nodes("").unwrap().is_empty());
        assert!(parse_declared_nodes("  \n# nothing\n").unwrap().is_empty());
        // Upper-case hex is accepted and stored in the one canonical form.
        let upper = format!("{}:web:a", first.to_uppercase());
        assert_eq!(parse_declared_nodes(&upper).unwrap()[0].public_key, first);
    }

    #[test]
    fn declared_nodes_reject_mistakes_with_a_pointer_to_the_entry() {
        let key = NodeKey::generate().public_hex();
        let other = NodeKey::generate().public_hex();
        for bad in [
            format!("{key}:web"),
            format!("{key}:web:a:extra"),
            // The comma form of roles reads as two entries and fails clearly.
            format!("{key}:web,ssh:a"),
            format!("{key}:cook:a"),
            format!("{key}:web+web:a"),
            format!("{key}::a"),
            format!("{key}:web:"),
            format!("{key}:web:Bad Name"),
            format!("{}:web:a", &key[..40]),
            format!("{key}:web:a,{key}:ssh:b"),
            format!("{key}:web:a,{other}:ssh:a"),
        ] {
            assert!(parse_declared_nodes(&bad).is_err(), "accepted {bad:?}");
        }
        let error = parse_declared_nodes(&format!("{key}:web,ssh:a"))
            .unwrap_err()
            .to_string();
        assert!(error.contains('+'), "{error}");
    }
}
