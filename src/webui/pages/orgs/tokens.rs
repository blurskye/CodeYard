//! The owners' token policy for an organization, its pending fine-grained token requests and
//! the tokens it has approved.
use super::{fields, Page};
use crate::webui::components::{compact_date, DraftCheckbox, DraftInput};
use crate::webui::navigation::NavLink;
use dioxus::prelude::*;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TokenPanel {
    pub fine_grained_allowed: bool,
    pub fine_grained_approval: bool,
    pub classic_allowed: bool,
    /// Empty for no maximum, which also allows tokens without expiry.
    pub max_lifetime_days: String,
    pub pending: Vec<TokenGrant>,
    pub active: Vec<TokenGrant>,
    pub pending_next: Option<String>,
    pub active_next: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TokenGrant {
    pub id: i64,
    pub user: String,
    pub name: String,
    pub description: String,
    pub access: String,
    pub repositories: Vec<String>,
    /// Permission name and level, such as `Issues: write`.
    pub permissions: Vec<String>,
    pub requested: String,
    pub expires: String,
    pub last_used: Option<String>,
}

fn checkbox(page: &Page, key: &str, label: &str, current: bool) -> Element {
    let checked = super::fields::draft_checked(page, "token-policy", key, current);
    rsx! { label { class: "org-confirm", DraftCheckbox { name: key.to_owned(), submission_value: "true", initial_checked: checked } span { "{label}" } } }
}

fn summary(grant: &TokenGrant) -> String {
    let access = match grant.access.as_str() {
        "all" => "all repositories".to_owned(),
        "selected" => grant.repositories.join(", "),
        _ => "public repositories".to_owned(),
    };
    let expires = if grant.expires == "never" {
        "never expires".to_owned()
    } else {
        format!("expires {}", compact_date(&grant.expires))
    };
    format!("{} · {access} · {expires}", grant.user)
}

pub(super) fn panel(page: &Page) -> Element {
    if !page.is_owner {
        return rsx! {};
    }
    let (Some(org), Some(tokens)) = (&page.selected, &page.tokens) else {
        return rsx! {};
    };
    let target = format!("/settings/organizations/{}/personal-access-tokens", org.id);
    let days = if page
        .draft
        .get("action")
        .is_some_and(|value| value == "token-policy")
    {
        page.draft
            .get("max_lifetime_days")
            .cloned()
            .unwrap_or_default()
    } else {
        tokens.max_lifetime_days.clone()
    };
    rsx! {
        section { class: "org-section", aria_labelledby: "organization-tokens-title",
            h2 { id: "organization-tokens-title", "Personal access tokens" }
            p { class: "org-note", "Fine-grained tokens reach this organization only with an owner's approval when approval is on. A token never does more than its user can, and stops working when the user leaves." }
            section { class: "org-panel", header { class: "org-panel-header", h3 { "Token policy" } }
                div { class: "org-panel-body",
                    form { key: "{org.id}-token-policy", class: "org-form", method: "post", action: target.clone(),
                        {fields::hidden_controls(page, "token-policy")}
                        {checkbox(page, "fine_grained_allowed", "Allow fine-grained personal access tokens", tokens.fine_grained_allowed)}
                        {checkbox(page, "fine_grained_approval", "Require owner approval for fine-grained tokens", tokens.fine_grained_approval)}
                        {checkbox(page, "classic_allowed", "Allow personal access tokens (classic)", tokens.classic_allowed)}
                        label { r#for: "organization-token-lifetime", "Maximum lifetime in days (1-366, empty for no maximum)" }
                        DraftInput { id: Some("organization-token-lifetime".to_owned()), name: "max_lifetime_days".to_owned(), initial_value: days, maxlength: Some(3) }
                        p { class: "org-field-help", "Without a maximum, members may create tokens that never expire. Lowering it stops longer-lived tokens until they are regenerated." }
                        button { class: "btn btn-primary", r#type: "submit", "Save token policy" }
                    }
                }
            }
            section { class: "org-panel", header { class: "org-panel-header", h3 { "Pending requests" } }
                ul { class: "org-list",
                    if tokens.pending.is_empty() { li { class: "org-empty", "No pending requests." } }
                    for grant in &tokens.pending {
                        li { class: "org-row", key: "{grant.id}",
                            div { class: "org-row-main",
                                h3 { "{grant.name}" }
                                p { "{summary(grant)} · requested {compact_date(&grant.requested)}" }
                                if !grant.description.is_empty() { p { "{grant.description}" } }
                                p { {grant.permissions.join(", ")} }
                            }
                            form { class: "org-row-actions", method: "post", action: target.clone(),
                                {fields::hidden_controls(page, "token-review")}
                                input { r#type: "hidden", name: "token_id", value: grant.id.to_string() }
                                DraftInput { name: "reason".to_owned(), initial_value: String::new(), maxlength: Some(1024), aria_label: Some(format!("Reason for {}", grant.name)) }
                                button { class: "btn btn-primary", r#type: "submit", name: "decision", value: "approve", "Approve" }
                                button { class: "btn btn-danger", r#type: "submit", name: "decision", value: "deny", "Deny" }
                            }
                        }
                    }
                }
                if let Some(next) = &tokens.pending_next { NavLink { class: "org-pagination", href: next.clone(), "Next pending requests" } }
            }
            section { class: "org-panel", header { class: "org-panel-header", h3 { "Active tokens" } }
                ul { class: "org-list",
                    if tokens.active.is_empty() { li { class: "org-empty", "No fine-grained tokens can reach this organization." } }
                    for grant in &tokens.active {
                        li { class: "org-row", key: "{grant.id}",
                            div { class: "org-row-main",
                                h3 { "{grant.name}" }
                                p { "{summary(grant)}" }
                                p { {grant.last_used.as_deref().map_or_else(|| "Never used".to_owned(), |at| format!("Last used {}", compact_date(at)))} }
                            }
                            form { class: "org-row-actions", method: "post", action: target.clone(),
                                {fields::hidden_controls(page, "token-revoke")}
                                input { r#type: "hidden", name: "token_id", value: grant.id.to_string() }
                                button { class: "btn btn-danger", r#type: "submit", "Revoke" span { class: "site-sr-only", " {grant.name}" } }
                            }
                        }
                    }
                }
                if let Some(next) = &tokens.active_next { NavLink { class: "org-pagination", href: next.clone(), "Next active tokens" } }
            }
        }
    }
}
