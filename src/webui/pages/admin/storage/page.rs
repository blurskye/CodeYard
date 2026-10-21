//! Admin storage overview. Storage is configured with environment variables;
//! credentials and filesystem paths are never page data.
use crate::webui::{
    navigation::NavLink,
    pages::admin::{admin_layout, Section},
};
use dioxus::prelude::*;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Page {
    pub database: String,
    pub object_storage: String,
    /// The bucket of S3-compatible storage. Local files have none: the page shows no paths.
    pub location: Option<String>,
}

pub fn render(page: Page) -> Element {
    admin_layout(
        Section::Storage,
        "Storage",
        "Where this installation keeps its data. Storage is set with environment variables.",
        rsx! {
            dl { class: "identity-admin-list",
                div { dt { "Database" } dd { class: "identity-muted", "{page.database}" } }
                div { dt { "Uploaded files" } dd { class: "identity-muted", "{page.object_storage}" } }
                if let Some(bucket) = &page.location {
                    div { dt { "Bucket" } dd { code { "{bucket}" } } }
                }
                div { dt { "Repositories" } dd { class: "identity-muted",
                    "On the node that owns each one. See "
                    NavLink { href: "/admin/nodes".to_string(), class: "identity-account-name", "Nodes" }
                    " for free disk space."
                } }
            }
            p { class: "identity-admin-hint",
                "Change storage settings with environment variables and restart: GITSERV_STATE_DIR for local files, or S3_ENDPOINT, S3_BUCKET, S3_ACCESS_KEY and S3_SECRET_KEY for a bucket."
            }
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_page_has_the_administration_tabs_and_explains_the_settings() {
        let html = dioxus_ssr::render_element(render(Page {
            database: "SQLite".into(),
            object_storage: "Local".into(),
            location: None,
        }));
        assert!(html.contains("aria-label=\"Administration\""));
        assert!(html.contains("href=\"/admin/nodes\""));
        assert!(html.contains("Change storage settings with environment variables and restart"));
        assert!(html.contains("GITSERV_STATE_DIR"));
        assert!(!html.contains(">Bucket<"));
    }
}
