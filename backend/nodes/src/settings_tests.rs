use super::settings::*;
use crate::pairing::JoinTicket;
use crate::transport::{NodeAddress, Relay};
use anyhow::Result;
use std::net::SocketAddr;
#[cfg(test)]
mod tests {
    use super::*;
    use codeyard_security::node_identity::NodeKey;

    fn settings(pairs: &[(&str, &str)]) -> Result<Option<Settings>> {
        Settings::from_lookup(&|name| {
            pairs
                .iter()
                .find(|(key, _)| *key == name)
                .map(|(_, value)| (*value).to_owned())
        })
    }

    #[test]
    fn no_cluster_setting_means_a_plain_single_node() {
        assert!(settings(&[]).unwrap().is_none());
        // Names and empty values do not switch anything on by themselves.
        assert!(settings(&[("NODE_NAME", "box")]).unwrap().is_none());
        for name in SWITCHES {
            assert!(
                settings(&[(name, ""), ("NODE_NAME", "box")])
                    .unwrap()
                    .is_none(),
                "{name}"
            );
            assert!(settings(&[(name, "  ")]).unwrap().is_none(), "{name}");
        }
    }

    #[test]
    fn defaults_are_direct_only_on_the_documented_port() {
        let key = NodeKey::generate().public_hex();
        let found = settings(&[
            ("NODE_NAME", "Core-1"),
            ("CLUSTER_NODES", &format!("{key}:web:core-1")),
        ])
        .unwrap()
        .unwrap();
        assert_eq!(found.bind, Some("0.0.0.0:7444".parse().unwrap()));
        assert!(found.relay.is_off());
        assert!(found.advertise.is_empty());
        assert_eq!(found.name, "core-1");
        assert_eq!(found.declared.len(), 1);
        assert!(found.join_ticket.is_none());
    }

    #[test]
    fn each_setting_is_read_and_checked() {
        let found = settings(&[
            ("NODE_NAME", "w1"),
            ("IROH_BIND", "127.0.0.1:9000"),
            ("IROH_RELAY", "n0"),
            ("IROH_ADVERTISE", "node.example.com:7444, 203.0.113.9:7444"),
        ])
        .unwrap()
        .unwrap();
        assert_eq!(found.bind.unwrap().port(), 9000);
        assert_eq!(found.relay, Relay::Public);
        assert_eq!(
            found.advertise,
            ["node.example.com:7444", "203.0.113.9:7444"]
        );

        assert!(settings(&[("NODE_NAME", "w1"), ("IROH_BIND", "not an address")]).is_err());
        assert!(settings(&[("NODE_NAME", "w1"), ("IROH_RELAY", "nonsense")]).is_err());
        assert!(settings(&[("NODE_NAME", "Bad Name!"), ("IROH_BIND", "127.0.0.1:1")]).is_err());
        assert!(settings(&[("NODE_NAME", "w1"), ("CLUSTER_JOIN_TOKEN", "garbage")]).is_err());
        assert!(settings(&[("NODE_NAME", "w1"), ("CLUSTER_NODES", "garbage")]).is_err());
        let both = settings(&[
            ("NODE_NAME", "w1"),
            ("CLUSTER_NODES", ""),
            ("CLUSTER_NODES_FILE", "/nonexistent"),
        ]);
        assert!(both.is_err(), "an unreadable file is an error, not silence");
    }

    #[test]
    fn core_peers_are_ordered_and_checked() {
        let first = NodeKey::generate().public_hex();
        let second = NodeKey::generate().public_hex();
        let found = settings(&[(
            "CLUSTER_PEERS",
            &format!("{first}@core.example:7444,{second}@[::1]:7444"),
        )])
        .unwrap()
        .unwrap();
        assert_eq!(
            found.peers,
            [
                (first, "core.example:7444".into()),
                (second, "[::1]:7444".into())
            ]
        );
        assert!(settings(&[("CLUSTER_PEERS", "bad@host:7444")]).is_err());
        assert!(settings(&[("CLUSTER_PEERS", "missing-at")]).is_err());
    }

    #[test]
    fn tcp_listener_runs_without_iroh_unless_an_iroh_setting_is_present() {
        let tcp_only = settings(&[("NODE_NAME", "w1"), ("CLUSTER_TCP_BIND", "0.0.0.0:7445")])
            .unwrap()
            .unwrap();
        assert_eq!(
            tcp_only.bind, None,
            "no UDP socket without an IROH_* setting"
        );
        assert_eq!(tcp_only.tcp_bind, Some("0.0.0.0:7445".parse().unwrap()));
        let both = settings(&[
            ("NODE_NAME", "w1"),
            ("CLUSTER_TCP_BIND", "0.0.0.0:7445"),
            ("IROH_BIND", "127.0.0.1:9000"),
        ])
        .unwrap()
        .unwrap();
        assert_eq!(both.bind.unwrap().port(), 9000);
        assert!(settings(&[("NODE_NAME", "w1"), ("CLUSTER_TCP_BIND", "not an address")]).is_err());
        assert!(
            settings(&[
                ("NODE_NAME", "w1"),
                ("CLUSTER_TCP_ADVERTISE", "core.example:7445"),
            ])
            .is_err(),
            "an advertised TCP address needs a TCP listener"
        );
    }

    #[test]
    fn tcp_peers_are_marked_and_the_advertised_address_defaults_to_the_bound_one() {
        let first = NodeKey::generate().public_hex();
        let second = NodeKey::generate().public_hex();
        let found = settings(&[
            ("NODE_NAME", "w1"),
            ("CLUSTER_TCP_BIND", "10.0.0.5:7445"),
            (
                "CLUSTER_PEERS",
                &format!("{first}@tcp://core.example:7445,{second}@core.example:7444"),
            ),
        ])
        .unwrap()
        .unwrap();
        assert_eq!(
            found.peers,
            [
                (first, "tcp://core.example:7445".into()),
                (second, "core.example:7444".into())
            ]
        );
        // Start fills in the bound address once the listener is up.
        assert!(found.tcp_advertise.is_empty());
        assert!(settings(&[("NODE_NAME", "w1"), ("CLUSTER_PEERS", "bad@tcp://x:1")]).is_err());
    }

    #[test]
    fn an_unspecified_tcp_bind_advertises_nothing_by_default() {
        assert_eq!(
            default_tcp_advertise("10.0.0.5:7445".parse().unwrap()),
            vec!["10.0.0.5:7445".parse::<SocketAddr>().unwrap()]
        );
        assert!(default_tcp_advertise("0.0.0.0:7445".parse().unwrap()).is_empty());
        assert!(default_tcp_advertise("[::]:7445".parse().unwrap()).is_empty());
    }

    #[test]
    fn nodes_file_and_join_ticket_are_read() {
        let directory = tempfile::tempdir().unwrap();
        let key = NodeKey::generate().public_hex();
        let file = directory.path().join("nodes");
        std::fs::write(&file, format!("# members\n{key}:storage:store-1\n")).unwrap();
        let ticket = JoinTicket::new(
            NodeKey::generate().public_key(),
            NodeAddress {
                addrs: vec!["127.0.0.1:7444".parse().unwrap()],
                ..Default::default()
            },
        )
        .unwrap();
        let found = settings(&[
            ("NODE_NAME", "w1"),
            ("CLUSTER_NODES_FILE", file.to_str().unwrap()),
            ("CLUSTER_JOIN_TOKEN", &ticket.encode()),
        ])
        .unwrap()
        .unwrap();
        assert_eq!(found.declared[0].name, "store-1");
        assert_eq!(found.join_ticket.unwrap(), ticket);
        let twice = settings(&[
            ("NODE_NAME", "w1"),
            ("CLUSTER_NODES", &format!("{key}:web:a")),
            ("CLUSTER_NODES_FILE", file.to_str().unwrap()),
        ]);
        assert!(twice.is_err());
    }
}
