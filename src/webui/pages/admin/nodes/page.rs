//! Admin list of the cluster's nodes, with approve and revoke.
use crate::webui::{
    components::{count_label, CopyButton},
    navigation::NavLink,
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
    rsx! {
        div { class: "page-shell max-w-5xl py-8 pb-14",
            header { class: "admin-header",
                div { class: "admin-header-main",
                    nav { class: "admin-crumbs", aria_label: "Breadcrumb",
                        NavLink { href: "/admin".to_string(), "Administration" }
                        " / Nodes"
                    }
                    div { class: "title-with-count",
                        h1 { "Cluster nodes" }
                        span { class: "count-badge", {count_label(page.nodes.len(), "node", "nodes")} }
                    }
                    p { "Review the machines connected to this installation." }
                }
            }
            if let Some(message) = &page.message {
                p { role: "status", class: "mb-4 rounded-md border border-destructive/30 bg-destructive/10 p-3 text-destructive", "{message}" }
            }
            section { class: "panel overflow-hidden", aria_label: "Cluster nodes",
                if page.nodes.is_empty() {
                    div { class: "node-setup",
                        svg { width: "28", height: "28", view_box: "0 0 24 24", fill: "none", stroke: "currentColor", stroke_width: "1.5", stroke_linecap: "round", stroke_linejoin: "round", "aria-hidden": "true",
                            rect { x: "2", y: "3", width: "20", height: "7", rx: "2" }
                            rect { x: "2", y: "14", width: "20", height: "7", rx: "2" }
                            path { d: "M6 6.5h.01M6 17.5h.01" }
                        }
                        h2 { class: "text-base font-semibold", "No nodes have joined." }
                        p { class: "mt-2 text-sm text-muted-foreground", "Connected nodes will appear here. A cluster needs PostgreSQL and this server running in cluster mode (see docs/CLUSTER.md). Then follow these steps." }
                        ol { class: "node-setup-steps",
                            li {
                                p { "On this server, create an invite for a storage node:" }
                                {command_block("codeyard cluster invite --roles storage")}
                            }
                            li {
                                p { "On the new machine, set " code { class: "font-mono text-foreground", "CLUSTER_JOIN_TOKEN" } " to the ticket." }
                            }
                            li {
                                p { "Then join the cluster from the new machine:" }
                                {command_block("codeyard cluster join --name NAME")}
                            }
                        }
                    }
                } else {
                    div { class: "overflow-x-auto", table { class: "min-w-full text-left text-sm",
                        thead { class: "bg-muted text-xs text-muted-foreground", tr {
                            th { class: "px-4 py-3 font-medium", "Node" }
                            th { class: "px-4 py-3 font-medium", "Roles" }
                            th { class: "px-4 py-3 font-medium", "Status" }
                            th { class: "px-4 py-3 font-medium", "Version" }
                            th { class: "px-4 py-3 font-medium", "Last seen" }
                            th { class: "px-4 py-3 font-medium", "Free disk" }
                            th { class: "px-4 py-3 text-right font-medium", "Action" }
                        } }
                        tbody { for node in &page.nodes { tr { class: "border-t border-border",
                            td { class: "px-4 py-3 font-mono font-medium", "{node.name}" }
                            td { class: "px-4 py-3", "{node.roles}" }
                            td { class: "px-4 py-3",
                                span { class: if node.status == "active" && !node.offline { "rounded-full border border-border px-2 py-0.5 text-xs" } else { "rounded-full border border-destructive/30 bg-destructive/10 px-2 py-0.5 text-xs text-destructive" },
                                    if node.offline { "Offline" } else { "{node.status}" }
                                }
                            }
                            td { class: "px-4 py-3 text-muted-foreground", "{node.version}" }
                            td { class: "px-4 py-3 text-muted-foreground", "{node.last_seen}" }
                            td { class: "px-4 py-3", "{node.disk_free}" }
                            td { class: "px-4 py-3 text-right",
                                form { method: "post", action: format!("/admin/nodes/{}/{}", node.name, if node.status == "active" { "revoke" } else { "approve" }),
                                    input { r#type: "hidden", name: "csrf_token", value: page.csrf_token.clone() }
                                    if node.status == "active" {
                                        button { r#type: "submit", class: "rounded-md px-3 py-2 text-sm text-destructive hover:bg-accent", "Revoke" }
                                    } else {
                                        button { r#type: "submit", class: "rounded-md border border-border bg-background px-3 py-2 text-sm font-medium hover:bg-accent", "Approve" }
                                    }
                                }
                            }
                        } } }
                    } }
                }
            }
            p { class: "mt-3 text-xs text-muted-foreground", "Revoked nodes lose access on their next connection." }
        }
    }
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
    fn the_header_names_the_administration_section() {
        let html = dioxus_ssr::render_element(render(Page {
            csrf_token: String::new(),
            nodes: vec![],
            message: None,
        }));
        assert!(html.contains("Administration</a> / Nodes"));
        assert!(html.contains("class=\"admin-header\""));
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
