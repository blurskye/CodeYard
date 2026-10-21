//! Desired service roles, not enrollment or repository/execution authority.
use anyhow::{ensure, Result};
use std::{collections::BTreeSet, fmt, str::FromStr};

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum NodeRole {
    Web,
    Storage,
    Ssh,
    ActionsWorker,
}

impl NodeRole {
    /// The name used on the command line, in the environment and in `nodes.roles`.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Web => "web",
            Self::Storage => "storage",
            Self::Ssh => "ssh",
            Self::ActionsWorker => "actions-worker",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NodeRoles(BTreeSet<NodeRole>);

impl NodeRoles {
    pub fn contains(&self, role: NodeRole) -> bool {
        self.0.contains(&role)
    }

    /// Role names in a fixed order, as stored in `nodes.roles`.
    pub fn names(&self) -> Vec<String> {
        self.0.iter().map(|role| role.as_str().to_owned()).collect()
    }

    pub fn worker_only(&self) -> bool {
        self.0.len() == 1 && self.contains(NodeRole::ActionsWorker)
    }

    /// Whether users connect to this node. Web and SSH can use local or remote
    /// repository storage; a node without them only serves other nodes.
    pub fn faces_users(&self) -> bool {
        self.contains(NodeRole::Web) || self.contains(NodeRole::Ssh)
    }
}

impl fmt::Display for NodeRoles {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.names().join(","))
    }
}

impl Default for NodeRoles {
    fn default() -> Self {
        // A plain installation runs all services on one machine.
        Self(
            [
                NodeRole::Web,
                NodeRole::Storage,
                NodeRole::Ssh,
                NodeRole::ActionsWorker,
            ]
            .into(),
        )
    }
}

impl FromStr for NodeRoles {
    type Err = anyhow::Error;

    fn from_str(value: &str) -> Result<Self> {
        ensure!(
            !value.is_empty() && value.len() <= 128 && !value.chars().any(char::is_control),
            "Select a nonempty bounded role list without control characters"
        );
        let mut roles = BTreeSet::new();
        for name in value.split(',').map(str::trim) {
            let role = match name {
                "web" => NodeRole::Web,
                "storage" => NodeRole::Storage,
                "ssh" => NodeRole::Ssh,
                "actions-worker" => NodeRole::ActionsWorker,
                _ => {
                    anyhow::bail!("Unknown node role; expected web, storage, ssh or actions-worker")
                }
            };
            ensure!(roles.insert(role), "Duplicate node role");
        }
        Ok(Self(roles))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roles_are_independently_composable_not_authority() {
        for mask in 1..16 {
            let names = ["web", "storage", "ssh", "actions-worker"];
            let selected = names
                .iter()
                .enumerate()
                .filter(|(i, _)| mask & (1 << i) != 0)
                .map(|(_, n)| *n)
                .collect::<Vec<_>>()
                .join(",");
            let roles: NodeRoles = selected.parse().unwrap();
            for (i, role) in [
                NodeRole::Web,
                NodeRole::Storage,
                NodeRole::Ssh,
                NodeRole::ActionsWorker,
            ]
            .into_iter()
            .enumerate()
            {
                assert_eq!(roles.contains(role), mask & (1 << i) != 0);
            }
            assert_eq!(roles.faces_users(), mask & 5 != 0);
        }
    }

    #[test]
    fn role_selection_is_strict_and_bounded() {
        for value in [
            "",
            "all",
            "worker",
            "web,",
            ",web",
            "web,web",
            "web,storage,SSH",
            "web\n,storage",
            "web,storage\0",
        ] {
            assert!(value.parse::<NodeRoles>().is_err(), "accepted {value:?}");
        }
        assert!("web,storage,ssh,actions-worker,"
            .repeat(20)
            .parse::<NodeRoles>()
            .is_err());
        assert_eq!(
            "web, storage, ssh, actions-worker"
                .parse::<NodeRoles>()
                .unwrap(),
            NodeRoles::default()
        );
    }
}
