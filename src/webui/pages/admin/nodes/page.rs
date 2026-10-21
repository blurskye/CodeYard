//! Admin list of the cluster's nodes, with approve and revoke.
use crate::webui::{
    components::{count_label, CopyButton},
    pages::admin::{admin_layout, Section},
    pages::fields::notice,
};
use dioxus::prelude::*;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Node {
    pub name: String,
    pub roles: String,
    /// `pending`, `active` or `revoked`.
    pub status: String,
    /// Active, but it has stopped reporting in.
    pub offline: bool,
    pub version: String,
    pub last_seen: String,
    pub disk_free: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Page {
    pub csrf_token: String,
    pub nodes: Vec<Node>,
    pub message: Option<String>,
}

pub fn render(page: Page) -> Element {
    admin_layout(
        Section::Nodes,
        "Cluster nodes",
        &format!(
            "{} connected to this installation.",
            count_label(page.nodes.len(), "node", "nodes")
        ),
        rsx! {
            {notice(page.message.as_deref(), true)}
            section { class: "identity-admin-panel", aria_label: "Cluster nodes",
                if page.nodes.is_empty() {
                    div { class: "identity-admin-box node-setup",
                        h2 { "No nodes have joined." }
                        p { class: "identity-admin-hint", "Connected nodes will appear here. A cluster needs PostgreSQL and this server running in cluster mode (see docs/CLUSTER.md). Then follow these steps." }
                        ol { class: "node-setup-steps",
                            li {
                                p { "On this server, create an invite for a storage node:" }
                                {command_block("codeyard cluster invite --roles storage")}
                            }
                            li {
                                p { "On the new machine, set " code { "CLUSTER_JOIN_TOKEN" } " to the ticket." }
                            }
                            li {
                                p { "Then join the cluster from the new machine:" }
                                {command_block("codeyard cluster join --name NAME")}
                            }
                        }
                    }
                } else {
                    div { class: "identity-table-wrap", table { class: "identity-table",
                        thead { tr {
                            th { "Node" } th { "Roles" } th { "Status" } th { "Version" } th { "Last seen" } th { class: "identity-num", "Free disk" }
                            th { class: "identity-table-action", "Action" }
                        } }
                        tbody { for node in &page.nodes { tr {
                            td { code { "{node.name}" } }
                            td { "{node.roles}" }
                            td {
                                span { class: if node.status == "active" && !node.offline { "identity-status identity-status-success" } else { "identity-status identity-status-danger" },
                                    if node.offline { "Offline" } else { "{node.status}" }
                                }
                            }
                            td { class: "identity-muted", "{node.version}" }
                            td { class: "identity-muted", "{node.last_seen}" }
                            td { class: "identity-num", "{node.disk_free}" }
                            td { class: "identity-table-action",
                                form { method: "post", action: format!("/admin/nodes/{}/{}", node.name, if node.status == "active" { "revoke" } else { "approve" }),
                                    input { r#type: "hidden", name: "csrf_token", value: page.csrf_token.clone() }
                                    if node.status == "active" {
                                        button { r#type: "submit", class: "btn btn-sm btn-danger", "Revoke" }
                                    } else {
                                        button { r#type: "submit", class: "btn btn-sm", "Approve" }
                                    }
                                }
                            }
                        } } }
                    } }
                }
                p { class: "identity-admin-hint", "Revoked nodes lose access on their next connection." }
            }
        },
    )
}

/// A setup command in its own block, with a button that copies it.
fn command_block(command: &str) -> Element {
    rsx! {
        div { class: "node-setup-command",
            code { "{command}" }
            CopyButton { text: command.to_owned(), label: "Copy command".to_owned() }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(name: &str, status: &str, offline: bool) -> Node {
        Node {
            name: name.into(),
            roles: "storage".into(),
            status: status.into(),
            offline,
            version: "0.1.1".into(),
            last_seen: "never".into(),
            disk_free: "10.0 GiB".into(),
        }
    }

    #[test]
    fn each_node_gets_the_action_its_status_allows() {
        let page = Page {
            csrf_token: "csrf-marker".into(),
            nodes: vec![
                node("store-1", "active", true),
                node("store-2", "pending", false),
                node("store-3", "revoked", false),
            ],
            message: None,
        };
        let html = dioxus_ssr::render_element(render(page.clone()));
        assert!(html.contains("/admin/nodes/store-1/revoke"), "{html}");
        assert!(html.contains("/admin/nodes/store-2/approve"));
        assert!(html.contains("/admin/nodes/store-3/approve"));
        assert!(html.contains("Offline"));
        assert_eq!(html.matches("csrf-marker").count(), 3);
        // The model is what the browser hydrates from.
        let json = serde_json::to_string(&page).unwrap();
        assert_eq!(serde_json::from_str::<Page>(&json).unwrap(), page);
    }

    #[test]
    fn the_header_has_the_administration_tabs_and_the_node_count() {
        let html = dioxus_ssr::render_element(render(Page {
            csrf_token: String::new(),
            nodes: vec![],
            message: None,
        }));
        assert!(html.contains("aria-label=\"Administration\""));
        assert!(html.contains("Cluster nodes"));
        assert!(html.contains("0 nodes connected to this installation."));
        assert!(html.contains("href=\"/admin/storage\""));
    }

    #[test]
    fn an_empty_cluster_says_so() {
        let html = dioxus_ssr::render_element(render(Page {
            csrf_token: String::new(),
            nodes: vec![],
            message: None,
        }));
        assert!(html.contains("No nodes have joined."));
        assert!(html.contains("codeyard cluster join --name NAME"));
        // Two numbered steps carry a command each, and each command has its own copy button.
        assert!(html.contains("<ol"), "{html}");
        assert_eq!(
            html.matches("aria-label=\"Copy command\"").count(),
            2,
            "{html}"
        );
    }
}
