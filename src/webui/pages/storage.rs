//! Admin storage overview. Storage is configured with environment variables;
//! credentials and filesystem paths are never page data.
use crate::webui::navigation::NavLink;
use dioxus::prelude::*;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Page {
    pub database: String,
    pub object_storage: String,
    /// Bucket name, or "state directory" for local storage.
    pub location: String,
}

pub fn render(page: Page) -> Element {
    rsx! {
        div { class: "page-shell max-w-5xl py-8 pb-14",
            header { class: "admin-header",
                div { class: "admin-header-main",
                    nav { class: "admin-crumbs", aria_label: "Breadcrumb",
                        NavLink { href: "/admin".to_string(), "Administration" }
                        " / Storage"
                    }
                    h1 { "Storage" }
                    p { "See where application data and uploaded files are stored." }
                }
            }
            div { class: "grid grid-cols-1 gap-4",
                section { class: "panel overflow-hidden", aria_labelledby: "database-storage-title",
                    header { class: "border-b border-border bg-muted/30 px-5 py-4",
                        h2 { id: "database-storage-title", class: "text-base font-semibold", "Application database" }
                        p { class: "mt-1 text-sm text-muted-foreground", "Accounts, repositories, issues, and settings." }
                    }
                    dl { class: "text-sm",
                        div { class: "flex items-center justify-between gap-4 px-5 py-4",
                            dt { class: "text-muted-foreground", "Database engine" }
                            dd { class: "font-medium", "{page.database}" }
                        }
                    }
                }
                section { class: "panel overflow-hidden", aria_labelledby: "file-storage-title",
                    header { class: "border-b border-border bg-muted/30 px-5 py-4",
                        h2 { id: "file-storage-title", class: "text-base font-semibold", "Uploaded files" }
                        p { class: "mt-1 text-sm text-muted-foreground", "Profile pictures and repository attachments." }
                    }
                    dl { class: "text-sm",
                        div { class: "flex items-center justify-between gap-4 px-5 py-4",
                            dt { class: "text-muted-foreground", "Storage provider" }
                            dd { class: "font-medium", "{page.object_storage}" }
                        }
                        div { class: "flex items-start justify-between gap-4 border-t border-border px-5 py-4",
                            dt { class: "text-muted-foreground", "Location" }
                            dd { class: "break-all text-right font-mono", "{page.location}" }
                        }
                    }
                }
            }
            aside { class: "mt-4 rounded-md border border-border bg-muted/30 p-4 text-sm text-muted-foreground",
                "Repository data is stored by the node that owns each repository. Review "
                NavLink { href: "/admin/nodes".to_string(), class: "text-primary hover:underline", "cluster nodes" }
                " to see available disk space."
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_header_names_the_administration_section() {
        let html = dioxus_ssr::render_element(render(Page {
            database: "SQLite".into(),
            object_storage: "Local".into(),
            location: "state directory".into(),
        }));
        assert!(html.contains("Administration</a> / Storage"));
        assert!(html.contains("class=\"admin-header\""));
    }
}
