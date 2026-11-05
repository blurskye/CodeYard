//! Desired service roles, not enrollment or repository/execution authority.
//!
//! A single machine runs `server,runner`, the default. `server` is `web`, `storage` and
//! `ssh` together; those three are separate roles only in cluster mode, where they can run
//! on different machines. `runner` executes Actions jobs and may run on another machine
//! without database credentials.
use anyhow::{ensure, Result};
use std::{collections::BTreeSet, fmt, str::FromStr};

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum NodeRole {
    Web,
    Storage,
    Ssh,
    Runner,
    Router,
}

impl NodeRole {
    /// The name used on the command line, in the environment and in `nodes.roles`.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Web => "web",
            Self::Storage => "storage",
            Self::Ssh => "ssh",
            Self::Runner => "runner",
            Self::Router => "router",
        }
    }
}

/// The roles `server` stands for.
const SERVER: [NodeRole; 3] = [NodeRole::Web, NodeRole::Storage, NodeRole::Ssh];

/// The refusal for a cluster role outside cluster mode.
pub const SINGLE_NODE_ROLES: &str = "this installation is in single-node mode, which runs only `server` and `runner`; run `codeyard cluster enable` to use the web, storage and ssh roles on separate machines";

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

    pub fn router_only(&self) -> bool {
        self.0.len() == 1 && self.contains(NodeRole::Router)
    }

    pub fn worker_only(&self) -> bool {
        self.0.len() == 1 && self.contains(NodeRole::Runner)
    }

    /// Whether single-node mode allows this selection: the server, a runner, or both.
    pub fn single_node(&self) -> bool {
        let others: Vec<_> = self
            .0
            .iter()
            .filter(|role| **role != NodeRole::Runner)
            .collect();
        others.is_empty() || others == SERVER.iter().collect::<Vec<_>>()
    }

    /// Whether users connect to this node. Web and SSH can use local or remote
    /// repository storage; a node without them only serves other nodes.
    pub fn faces_users(&self) -> bool {
        self.contains(NodeRole::Web)
            || self.contains(NodeRole::Ssh)
            || self.contains(NodeRole::Router)
    }
}

/// Stored role names as people read them: `web`, `storage` and `ssh` together are `server`.
pub fn describe_roles(names: &[String]) -> String {
    match names.join(",").parse::<NodeRoles>() {
        Ok(roles) => roles.to_string(),
        Err(_) => names.join(","),
    }
}

impl fmt::Display for NodeRoles {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let server = SERVER.iter().all(|role| self.contains(*role));
        let mut names = Vec::new();
        if server {
            names.push("server");
        }
        for role in &self.0 {
            if !(server && SERVER.contains(role)) {
                names.push(role.as_str());
            }
        }
        formatter.write_str(&names.join(","))
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
                NodeRole::Runner,
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
        let mut names = BTreeSet::new();
        let mut roles = BTreeSet::new();
        for name in value.split(',').map(str::trim) {
            let selected: &[NodeRole] = match name {
                "server" => &SERVER,
                "web" => &[NodeRole::Web],
                "storage" => &[NodeRole::Storage],
                "ssh" => &[NodeRole::Ssh],
                "runner" => &[NodeRole::Runner],
                "router" => &[NodeRole::Router],
                _ => anyhow::bail!(
                    "Unknown role; expected server, runner or router (web, storage and ssh in cluster mode)"
                ),
            };
            ensure!(names.insert(name), "Duplicate role");
            for role in selected {
                ensure!(roles.insert(*role), "Duplicate role");
            }
        }
        ensure!(
            !roles.contains(&NodeRole::Router) || roles.len() == 1,
            "The router role runs alone without a cell database"
        );
        Ok(Self(roles))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn router_runs_alone_without_cell_services() {
        let roles: NodeRoles = "router".parse().unwrap();
        assert!(roles.router_only());
        assert!(roles.faces_users());
        assert!(!roles.single_node());
        assert!(!roles.worker_only());
        assert_eq!(roles.to_string(), "router");
        for mixed in ["router,server", "router,runner", "router,web", "router,ssh"] {
            assert!(mixed.parse::<NodeRoles>().is_err());
        }
    }

    #[test]
    fn roles_are_independently_composable_not_authority() {
        for mask in 1..16 {
            let names = ["web", "storage", "ssh", "runner"];
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
                NodeRole::Runner,
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
            "actions-worker",
            "web,",
            ",web",
            "web,web",
            "server,web",
            "server,ssh",
            "server,server",
            "web,storage,SSH",
            "web\n,storage",
            "web,storage\0",
        ] {
            assert!(value.parse::<NodeRoles>().is_err(), "accepted {value:?}");
        }
        assert!("web,storage,ssh,runner,"
            .repeat(20)
            .parse::<NodeRoles>()
            .is_err());
        assert_eq!(
            "web, storage, ssh, runner".parse::<NodeRoles>().unwrap(),
            NodeRoles::default()
        );
    }

    #[test]
    fn server_is_web_storage_and_ssh_and_the_default_reads_as_server_runner() {
        assert_eq!(
            "server".parse::<NodeRoles>().unwrap(),
            "web,storage,ssh".parse::<NodeRoles>().unwrap()
        );
        assert_eq!(
            "server,runner".parse::<NodeRoles>().unwrap(),
            NodeRoles::default()
        );
        assert_eq!(NodeRoles::default().to_string(), "server,runner");
        assert_eq!(
            NodeRoles::default().names(),
            ["web", "storage", "ssh", "runner"]
        );
        assert_eq!("runner".parse::<NodeRoles>().unwrap().to_string(), "runner");
        assert!("runner".parse::<NodeRoles>().unwrap().worker_only());
        assert_eq!(
            "web,ssh".parse::<NodeRoles>().unwrap().to_string(),
            "web,ssh"
        );
        assert_eq!(describe_roles(&["storage".into()]), "storage");
        assert_eq!(
            describe_roles(&["web".into(), "storage".into(), "ssh".into()]),
            "server"
        );
        assert_eq!(
            describe_roles(&["web".into(), "storage".into()]),
            "web,storage"
        );
    }

    #[test]
    fn single_node_mode_allows_only_the_server_and_runners() {
        for allowed in ["server", "runner", "server,runner", "web,storage,ssh"] {
            assert!(
                allowed.parse::<NodeRoles>().unwrap().single_node(),
                "{allowed}"
            );
        }
        for refused in [
            "web",
            "storage",
            "ssh",
            "web,ssh",
            "storage,runner",
            "web,storage",
        ] {
            assert!(
                !refused.parse::<NodeRoles>().unwrap().single_node(),
                "{refused}"
            );
        }
    }
}
