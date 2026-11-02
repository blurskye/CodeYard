use anyhow::{bail, Context};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::FromRow;
use std::{fmt, str::FromStr};

/// A machine that is, or is waiting to become, part of the cluster. Roles are plain
/// names here; the `cluster` module turns them into `NodeRoles`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ClusterNode {
    pub id: i64,
    /// Lowercase hex of the node's ed25519 public key, which is also its iroh endpoint id.
    pub public_key: String,
    pub name: String,
    pub roles: Vec<String>,
    /// `pending`, `active` or `revoked`.
    pub status: String,
    /// JSON with the addresses the node last reported.
    pub addr_hint: Option<String>,
    pub version: Option<String>,
    pub disk_free: Option<i64>,
    pub last_seen: Option<DateTime<Utc>>,
    /// Database time when this row was read, on the heartbeat's clock.
    #[serde(default)]
    pub observed_at: DateTime<Utc>,
    pub created_at: DateTime<Utc>,
}

impl ClusterNode {
    /// Active and reporting in when read from the database.
    pub fn is_online(&self) -> bool {
        self.status == "active"
            && self.last_seen.is_some_and(|seen| {
                let age = self.observed_at - seen;
                age >= chrono::Duration::zero() && age <= chrono::Duration::seconds(60)
            })
    }
}

/// An invite as an operator sees it. The secret and its hash are never listed.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct JoinInvite {
    pub id: i64,
    pub roles: Vec<String>,
    pub reusable: bool,
    pub requires_approval: bool,
    pub expires_at: DateTime<Utc>,
    pub uses: i32,
    pub created_at: DateTime<Utc>,
}

/// What redeeming a join token did. Only `Joined` used up a token.
#[derive(Debug, Clone, PartialEq)]
pub enum JoinOutcome {
    /// The node was added, `active` or `pending` depending on the invite.
    Joined(ClusterNode),
    /// This key was already registered; it gets its current standing back.
    AlreadyMember(ClusterNode),
    /// The token is unknown, expired, or a one-time token that was used already.
    InvalidToken,
    /// The node asked for roles the invite does not allow.
    RolesNotAllowed { allowed: Vec<String> },
    /// Another node already has this name.
    NameTaken,
}

/// A copy of a repository that this node holds, with the primary's version.
#[derive(Debug, Clone, PartialEq, Eq, FromRow)]
pub struct HeldReplica {
    pub repository_id: i64,
    pub disk_path: String,
    /// The node that holds the primary; `None` for a repository that has no node yet.
    pub primary_node_id: Option<i64>,
    /// The primary's version this copy has been brought up to; -1 before the first copy.
    pub applied_version: i64,
    pub write_version: i64,
}

/// How many repositories a node is primary for, and how many copies it holds, of which
/// `current` match their primary's version.
#[derive(Debug, Clone, Copy, PartialEq, Eq, FromRow)]
pub struct NodeReplicaCounts {
    pub node_id: i64,
    pub primaries: i64,
    pub replicas: i64,
    pub current: i64,
}

/// The replica setting: how many copies of each repository the installation keeps.
/// Stored as text in `node_configuration.replicas`: a count from 1 to 999 or `all`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReplicaSetting {
    /// That many copies in all, the primary included. `1` means no replicas.
    Copies(u16),
    /// A copy on every storage node.
    All,
}

impl FromStr for ReplicaSetting {
    type Err = anyhow::Error;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        if text == "all" {
            return Ok(Self::All);
        }
        let valid = (1..=3).contains(&text.len()) && text.bytes().all(|b| b.is_ascii_digit());
        if !valid || text.starts_with('0') {
            bail!("replicas must be a count from 1 to 999, or all");
        }
        Ok(Self::Copies(text.parse().context("replica count")?))
    }
}

impl fmt::Display for ReplicaSetting {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Copies(count) => write!(f, "{count}"),
            Self::All => f.write_str("all"),
        }
    }
}

/// What a membership status change did.
#[derive(Debug, Clone, PartialEq)]
pub enum NodeStatusChange {
    /// The node has its new status now; `before` is the status it had.
    Changed {
        before: String,
        node: Box<ClusterNode>,
    },
    /// No node has that name.
    Missing,
    /// The acting account is not an active administrator, so nothing changed.
    Denied,
}
