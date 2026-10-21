//! The top of every administration page: title, section tabs, and the cluster sub-tabs.
use crate::webui::navigation::NavLink;
use dioxus::prelude::*;

/// The administration page being shown.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Section {
    Overview,
    Users,
    Settings,
    Email,
    Audit,
    Nodes,
    Storage,
}

/// Each tab, and the pages it covers. Nodes and Storage share the Cluster tab.
const TABS: [(&str, &str, &[Section]); 6] = [
    ("/admin", "Overview", &[Section::Overview]),
    ("/admin/users", "Users", &[Section::Users]),
    ("/admin/settings", "Site settings", &[Section::Settings]),
    ("/admin/email", "Email", &[Section::Email]),
    ("/admin/audit", "Audit log", &[Section::Audit]),
    (
        "/admin/nodes",
        "Cluster",
        &[Section::Nodes, Section::Storage],
    ),
];

fn tab_link(href: &str, label: &str, current: bool) -> Element {
    rsx! {
        NavLink { href: href.to_owned(), class: "admin-tab",
            aria_current: current.then(|| "page".to_owned()),
            "{label}"
        }
    }
}

/// The page title with the section tabs under it. The tabs wrap on a narrow screen.
pub fn admin_header(section: Section, title: &str, intro: &str) -> Element {
    let cluster = matches!(section, Section::Nodes | Section::Storage);
    rsx! {
        div { class: "admin-top",
            header { class: "admin-header",
                div { class: "admin-header-main",
                    h1 { "{title}" }
                    p { "{intro}" }
                }
            }
            nav { class: "admin-tabs", aria_label: "Administration",
                for (href, label, sections) in TABS {
                    {tab_link(href, label, sections.contains(&section))}
                }
            }
            if cluster {
                nav { class: "admin-tabs admin-subtabs", aria_label: "Cluster",
                    {tab_link("/admin/nodes", "Nodes", section == Section::Nodes)}
                    {tab_link("/admin/storage", "Storage", section == Section::Storage)}
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn html(section: Section) -> String {
        dioxus_ssr::render_element(admin_header(section, "Title", "Intro"))
    }

    #[test]
    fn every_page_offers_the_same_tabs_and_marks_its_own() {
        for section in [
            Section::Overview,
            Section::Users,
            Section::Settings,
            Section::Audit,
        ] {
            let output = html(section);
            for (href, label, _) in TABS {
                assert!(output.contains(&format!("href=\"{href}\"")), "{output}");
                assert!(output.contains(label), "{output}");
            }
            assert_eq!(output.matches("aria-current=\"page\"").count(), 1);
            assert!(!output.contains("aria-label=\"Cluster\""));
        }
    }

    #[test]
    fn nodes_and_storage_mark_the_cluster_tab_and_their_own_sub_tab() {
        for section in [Section::Nodes, Section::Storage] {
            let output = html(section);
            assert_eq!(
                output.matches("aria-current=\"page\"").count(),
                2,
                "{output}"
            );
            assert!(output.contains("href=\"/admin/nodes\""));
            assert!(output.contains("href=\"/admin/storage\""));
        }
    }
}
