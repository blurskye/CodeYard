//! Site settings: who can sign up (set during setup) and the AI crawler policy.
use super::nav::{admin_header, Section};
use crate::webui::components::{DraftSelect, SelectOption};
use crate::webui::pages::fields::{csrf, notice};
use dioxus::prelude::*;

#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct AdminSettings {
    /// A private node reviews each signup request. A public one lets anyone register.
    pub private: bool,
    /// `block` or `allow`.
    pub ai_crawlers: String,
}

/// The AI crawler setting. A component, so its select runs inside the renderer's runtime.
#[component]
fn AiCrawlersPanel(token: String, value: String) -> Element {
    rsx! {
        section { class: "identity-admin-panel", aria_labelledby: "admin-crawlers-title",
            header { class: "identity-admin-panel-header",
                div { h2 { id: "admin-crawlers-title", "AI crawlers" } p { "Block AI training crawlers in robots.txt and ai.txt, and ask crawlers not to use HTML pages." } }
            }
            form { method: "post", action: "/admin/settings", class: "identity-admin-crawlers",
                {csrf(&token)}
                div { class: "identity-admin-crawlers-field",
                    label { r#for: "ai_crawlers", "AI crawler policy" }
                    DraftSelect { id: Some("ai_crawlers".to_owned()), name: "ai_crawlers", class: "border border-border bg-background",
                        initial_value: value,
                        options: vec![
                            SelectOption::new("block", "Block (default)"),
                            SelectOption::new("allow", "Allow"),
                        ] }
                    p { class: "identity-admin-hint", "Block disallows AI training crawlers in robots.txt and ai.txt. Allow lets every crawler in." }
                }
                button { r#type: "submit", class: "btn", "Save" }
            }
        }
    }
}

pub fn admin_settings_page(
    model: AdminSettings,
    token: &str,
    message: Option<&str>,
    error: bool,
) -> Element {
    rsx! {
        style { dangerous_inner_html: include_str!("../account.css") }
        div { class: "identity-settings identity-admin page-shell",
            {admin_header(Section::Settings, "Site settings", "Who can sign up, and how crawlers treat this site.")}
            {notice(message, error)}
            section { class: "identity-admin-panel", aria_labelledby: "admin-signup-title",
                header { class: "identity-admin-panel-header",
                    div { h2 { id: "admin-signup-title", "Sign-up" } p { "Chosen when the node was set up." } }
                }
                div { class: "identity-admin-crawlers",
                    p { class: "identity-admin-hint",
                        if model.private {
                            "Private: people request an account and an administrator approves each request on the Users tab."
                        } else {
                            "Public: anyone can register an account."
                        }
                    }
                }
            }
            AiCrawlersPanel { token: token.to_owned(), value: model.ai_crawlers }
        }
    }
}
