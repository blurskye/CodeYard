//! The frame of every administration page: the section list beside the page title and body.
use crate::webui::pages::settings::{section_nav, NavGroup};
use crate::webui::pages::users::ProfileIcon;
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

impl Section {
    fn id(self) -> &'static str {
        match self {
            Section::Overview => "overview",
            Section::Users => "users",
            Section::Settings => "settings",
            Section::Email => "email",
            Section::Audit => "audit",
            Section::Nodes => "nodes",
            Section::Storage => "storage",
        }
    }
}

fn groups() -> [NavGroup; 3] {
    [
        (
            "Site",
            vec![
                ("/admin", "Overview", "overview", ProfileIcon::Chart),
                ("/admin/users", "Users", "users", ProfileIcon::People),
                ("/admin/audit", "Audit log", "audit", ProfileIcon::Shield),
            ],
        ),
        (
            "Configuration",
            vec![
                (
                    "/admin/settings",
                    "Site settings",
                    "settings",
                    ProfileIcon::Gear,
                ),
                ("/admin/email", "Email", "email", ProfileIcon::Mail),
            ],
        ),
        (
            "Cluster",
            vec![
                ("/admin/nodes", "Nodes", "nodes", ProfileIcon::Server),
                (
                    "/admin/storage",
                    "Storage",
                    "storage",
                    ProfileIcon::Database,
                ),
            ],
        ),
    ]
}

/// An administration page: the section list on the left, then the title, one line of text
/// and the page's own sections.
pub fn admin_layout(section: Section, title: &str, intro: &str, body: Element) -> Element {
    rsx! {
        div { class: "identity-settings page-shell",
            div { class: "identity-settings-layout",
                {section_nav("Administration", &groups(), section.id())}
                div { class: "identity-settings-content identity-admin",
                    header { class: "admin-header",
                        h1 { "{title}" }
                        p { "{intro}" }
                    }
                    {body}
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_page_lists_every_section_and_marks_its_own() {
        for section in [
            Section::Overview,
            Section::Users,
            Section::Settings,
            Section::Email,
            Section::Audit,
            Section::Nodes,
            Section::Storage,
        ] {
            let output =
                dioxus_ssr::render_element(admin_layout(section, "Title", "Intro", rsx! {}));
            for (_, links) in groups() {
                for (href, label, _, _) in links {
                    assert!(output.contains(&format!("href=\"{href}\"")), "{output}");
                    assert!(output.contains(label), "{output}");
                }
            }
            assert!(output.contains("aria-label=\"Administration\""), "{output}");
            // The sidebar and the phone menu each mark the current page once.
            assert_eq!(
                output.matches("aria-current=\"page\"").count(),
                2,
                "{output}"
            );
            assert_eq!(output.matches("<h1").count(), 1);
        }
    }
}
