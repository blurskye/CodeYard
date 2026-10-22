//! Administrator audit log: who did what, newest first, as sentences, with a filter on the
//! kind of change and on the actor.
use super::nav::{admin_layout, Section};
use crate::webui::components::{count_label, DraftSelect, LiveTime, SelectOption};
use crate::webui::navigation::NavLink;
use dioxus::prelude::*;
use serde_json::Value;

/// Rows shown per page.
pub const AUDIT_ENTRIES_PER_PAGE: usize = 50;

#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct AuditEntry {
    pub id: i64,
    /// The acting account, or empty when it no longer exists.
    pub actor: String,
    pub action: String,
    /// The name of what the action changed, such as an account. Empty when the row names none.
    pub target: String,
    /// The organization the row belongs to. Empty for site changes.
    pub org: String,
    /// The row's details as JSON text. Empty when there are none.
    pub details: String,
    /// An RFC 3339 time.
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

/// The kinds of change the filter offers. A value ending in a dot matches every action in
/// that group.
const ACTIONS: [(&str, &str); 8] = [
    ("", "All changes"),
    ("user.create", "Accounts created"),
    ("user.ban", "Bans"),
    ("user.admin_role", "Administrator role"),
    ("user.two_factor_reset", "Two-factor resets"),
    ("signup.", "Sign-up requests"),
    ("node.", "Site settings and nodes"),
    ("org.", "Organizations"),
];

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

fn profile(name: &str) -> Element {
    rsx! { NavLink { href: format!("/{}", crate::webui::encode_component(name)), class: "identity-account-name", "{name}" } }
}

/// What the actor did, as the rest of a sentence: "banned bob".
fn change(entry: &AuditEntry) -> Element {
    let details: Value = serde_json::from_str(&entry.details).unwrap_or_default();
    let flag = |name: &str| details.get(name).and_then(Value::as_bool).unwrap_or(false);
    let text = |name: &str| {
        details
            .get(name)
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned()
    };
    let username = text("username");
    let status = text("status");
    let target = || {
        if entry.target.is_empty() {
            rsx! { "a removed account" }
        } else {
            profile(&entry.target)
        }
    };
    match entry.action.as_str() {
        "user.create" => {
            rsx! { "created the account " {target()} if flag("is_admin") { " as an administrator" } }
        }
        "user.ban" if flag("banned") => rsx! { "banned " {target()} },
        "user.ban" => rsx! { "unbanned " {target()} },
        "user.admin_role" if flag("is_admin") => rsx! { "made " {target()} " an administrator" },
        "user.admin_role" => rsx! { "removed the administrator role from " {target()} },
        "user.two_factor_reset" => rsx! { "turned off two-factor for " {target()} },
        "signup.approve" => rsx! { "approved the sign-up request from " {target()} },
        "signup.reject" => rsx! { "rejected the sign-up request from " "{username}" },
        "node.node_mode" if text("node_mode") == "private" => {
            rsx! { "made new accounts need approval" }
        }
        "node.node_mode" => rsx! { "opened sign-up to anyone" },
        "node.ai_crawlers" if text("ai_crawlers") == "allow" => rsx! { "allowed AI crawlers" },
        "node.ai_crawlers" => rsx! { "blocked AI crawlers" },
        "node.email_settings" if flag("password_changed") => {
            rsx! { "changed the mail server and its password" }
        }
        "node.email_settings" => rsx! { "changed the mail server" },
        "node.actions_settings" if flag("enabled") => rsx! { "changed the Actions settings" },
        "node.actions_settings" => rsx! { "turned off Actions" },
        "node.status" => rsx! { "set node " strong { "{entry.target}" } " to " "{status}" },
        "org.governance.recover_owner" => {
            rsx! { "made " {target()} " an owner of " strong { "{entry.org}" } ", which had no owner left" }
        }
        action => {
            rsx! { code { "{action}" } if !entry.target.is_empty() { " on " strong { "{entry.target}" } } }
        }
    }
}

/// A component, so its select runs inside the renderer's runtime.
#[component]
fn AuditFilters(action: String, actor: String) -> Element {
    let options = ACTIONS
        .iter()
        .map(|&(value, label)| SelectOption::new(value, label))
        .collect::<Vec<_>>();
    rsx! {
        form { method: "get", action: "/admin/audit", role: "search", class: "identity-admin-search identity-admin-audit-filters",
            label { r#for: "admin-audit-action", class: "identity-sr-only", "Kind of change" }
            DraftSelect { id: Some("admin-audit-action".to_owned()), name: "action", class: "border border-border bg-background",
                initial_value: action, options }
            label { r#for: "admin-audit-actor", class: "identity-sr-only", "Done by" }
            input { id: "admin-audit-actor", name: "actor", r#type: "search", value: actor, placeholder: "Done by (username)", class: "identity-admin-search-input" }
            button { r#type: "submit", class: "btn", "Filter" }
        }
    }
}

pub fn admin_audit_page(model: AdminAudit) -> Element {
    let page = model.page;
    let previous = (page > 1).then(|| audit_url(&model.action, &model.actor, page - 1));
    let next = (page < model.pages).then(|| audit_url(&model.action, &model.actor, page + 1));
    let filtered = !model.action.is_empty() || !model.actor.is_empty();
    admin_layout(
        Section::Audit,
        "Audit log",
        "Changes administrators made to accounts, sign-up and site settings, newest first.",
        rsx! {
            section { class: "identity-admin-panel", aria_label: "Changes",
                AuditFilters { action: model.action.clone(), actor: model.actor.clone() }
                p { class: "identity-muted",
                    {count_label(model.total, "change", "changes")}
                    if filtered { " match. " NavLink { href: "/admin/audit", "Clear filters" } }
                }
                if model.entries.is_empty() {
                    div { class: "identity-empty", h3 { "No changes" }
                        p { if filtered { "Nothing matches these filters." } else { "Changes to accounts and settings will appear here." } }
                    }
                } else {
                    ol { class: "identity-admin-list identity-admin-audit",
                        for entry in model.entries.iter() {
                            li { key: "{entry.id}",
                                span {
                                    if entry.actor.is_empty() { "A removed account" } else { {profile(&entry.actor)} }
                                    " "
                                    {change(entry)}
                                }
                                span { class: "identity-muted", LiveTime { value: entry.time.clone() } }
                            }
                        }
                    }
                }
                if model.pages > 1 {
                    nav { class: "identity-admin-pager", aria_label: "Audit log pages",
                        if let Some(href) = previous { NavLink { href, class: "btn btn-sm", "Previous" } }
                        span { class: "identity-muted", "Page {page} of {model.pages}" }
                        if let Some(href) = next { NavLink { href, class: "btn btn-sm", "Next" } }
                    }
                }
            }
        },
    )
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
