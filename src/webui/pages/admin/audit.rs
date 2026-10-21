//! Administrator audit log: who did what, newest first, with a filter on the action and the actor.
use super::nav::{admin_header, Section};
use crate::webui::components::count_label;
use crate::webui::navigation::NavLink;
use dioxus::prelude::*;

/// Rows shown per page.
pub const AUDIT_ENTRIES_PER_PAGE: usize = 50;

#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct AuditEntry {
    pub id: i64,
    /// The acting account, or empty when it no longer exists.
    pub actor: String,
    pub action: String,
    /// What the action changed, such as "user 7". Empty when the row names no target.
    pub target: String,
    /// The row's details as JSON text. Empty when there are none.
    pub details: String,
    pub time: String,
}

#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct AdminAudit {
    /// The rows on this page.
    pub entries: Vec<AuditEntry>,
    /// The action filter, trimmed. Empty matches every action.
    pub action: String,
    /// The actor filter, trimmed. Empty matches every actor.
    pub actor: String,
    pub page: usize,
    pub pages: usize,
    /// Rows that match the filters, on every page.
    pub total: usize,
}

/// The audit log address for the filters and a page. Pages after the first keep their number.
fn audit_url(action: &str, actor: &str, page: usize) -> String {
    let mut query = url::form_urlencoded::Serializer::new(String::new());
    if !action.is_empty() {
        query.append_pair("action", action);
    }
    if !actor.is_empty() {
        query.append_pair("actor", actor);
    }
    if page > 1 {
        query.append_pair("page", &page.to_string());
    }
    let query = query.finish();
    if query.is_empty() {
        "/admin/audit".to_owned()
    } else {
        format!("/admin/audit?{query}")
    }
}

pub fn admin_audit_page(model: AdminAudit) -> Element {
    let page = model.page;
    let previous = (page > 1).then(|| audit_url(&model.action, &model.actor, page - 1));
    let next = (page < model.pages).then(|| audit_url(&model.action, &model.actor, page + 1));
    rsx! {
        div { class: "identity-settings identity-admin page-shell",
            {admin_header(Section::Audit, "Audit log", "Administrative changes to accounts and site settings, newest first.")}
            section { class: "identity-admin-panel", aria_labelledby: "admin-audit-title",
                header { class: "identity-admin-panel-header",
                    div { h2 { id: "admin-audit-title", "Events" } p { {count_label(model.total, "event", "events")} } }
                }
                form { method: "get", action: "/admin/audit", role: "search", class: "identity-admin-search",
                    label { r#for: "admin-audit-action", class: "identity-sr-only", "Filter by action" }
                    input { id: "admin-audit-action", name: "action", r#type: "search", value: model.action.clone(), placeholder: "Action, such as user.ban", class: "identity-admin-search-input" }
                    label { r#for: "admin-audit-actor", class: "identity-sr-only", "Filter by actor" }
                    input { id: "admin-audit-actor", name: "actor", r#type: "search", value: model.actor.clone(), placeholder: "Actor username", class: "identity-admin-search-input" }
                    button { r#type: "submit", class: "btn", "Filter" }
                }
                if model.entries.is_empty() {
                    div { class: "identity-empty", h3 { "No events" } p { "Nothing matches these filters." } }
                } else {
                    div { class: "identity-table-wrap", table { class: "identity-table",
                        thead { tr { th { "Time" } th { "Actor" } th { "Action" } th { "Target" } th { "Details" } } }
                        tbody { for entry in model.entries.iter() { tr { key: "{entry.id}",
                            td { "data-label": "Time", "{entry.time}" }
                            td { "data-label": "Actor", if entry.actor.is_empty() { span { class: "identity-muted", "Removed account" } } else { "{entry.actor}" } }
                            td { "data-label": "Action", code { "{entry.action}" } }
                            td { "data-label": "Target", "{entry.target}" }
                            td { "data-label": "Details", if !entry.details.is_empty() { code { "{entry.details}" } } }
                        } } }
                    } }
                }
                if model.pages > 1 {
                    nav { class: "identity-admin-pager", aria_label: "Audit log pages",
                        if let Some(href) = previous { NavLink { href, class: "btn", "Previous" } }
                        span { class: "identity-muted", "Page {page} of {model.pages}" }
                        if let Some(href) = next { NavLink { href, class: "btn", "Next" } }
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::audit_url;

    #[test]
    fn addresses_keep_the_filters_and_leave_out_the_first_page() {
        assert_eq!(audit_url("", "", 1), "/admin/audit");
        assert_eq!(
            audit_url("user.", "ali ce", 3),
            "/admin/audit?action=user.&actor=ali+ce&page=3"
        );
    }
}
