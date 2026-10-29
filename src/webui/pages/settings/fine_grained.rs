//! Fine-grained personal access tokens: the list, the form that creates one, and one token's page.
use super::page::settings_nav;
use crate::webui::components::{compact_date, confirm_dialog, DraftSelect, SelectOption};
use crate::webui::navigation::NavLink;
use crate::webui::pages::fields::{csrf, notice, text_field, FIELD, SUBMIT};
use dioxus::prelude::*;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

pub const BASE: &str = "/settings/personal-access-tokens";

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Model {
    pub csrf_token: String,
    pub view: View,
    pub message: Option<String>,
    pub is_error: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum View {
    List {
        tokens: Vec<Row>,
        next_before: Option<i64>,
    },
    New {
        draft: Draft,
        choices: Choices,
    },
    /// `value` is set only in the response that created or regenerated the token.
    Show {
        token: Detail,
        value: Option<String>,
    },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Row {
    pub id: i64,
    pub name: String,
    pub owner: String,
    /// approved, pending, denied or revoked.
    pub status: String,
    pub expires: String,
    pub last_used: Option<String>,
    pub last_ip: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Detail {
    pub row: Row,
    pub description: String,
    /// public, all or selected.
    pub access: String,
    pub repositories: Vec<String>,
    /// Group label, permission name and level.
    pub permissions: Vec<(String, String, String)>,
    pub reason: Option<String>,
    pub created: String,
    /// The owner allows tokens without expiry, so regeneration offers it.
    pub never_allowed: bool,
}

/// The create form as submitted, kept while the form is refreshed or refused.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Draft {
    pub name: String,
    pub description: String,
    /// `user`, or an organization id.
    pub owner: String,
    pub owners_after: i64,
    /// A number of days, `custom` or `never`.
    pub expiry: String,
    pub expires_at: String,
    pub access: String,
    pub repositories: Vec<i64>,
    pub permissions: BTreeMap<String, String>,
    pub query: String,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Choices {
    /// Value and label: `user` and the username, then each organization.
    pub owners: Vec<(String, String)>,
    pub next_owners: Option<i64>,
    /// The owner's repositories that match the search, and the ones already selected.
    pub repositories: Vec<(i64, String)>,
    pub more_repositories: bool,
    pub permissions: Vec<Choice>,
    pub never_allowed: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Choice {
    pub key: String,
    pub name: String,
    pub group: String,
    pub levels: Vec<String>,
}

fn status_label(status: &str) -> &'static str {
    match status {
        "approved" => "Active",
        "pending" => "Waiting for organization approval",
        "denied" => "Denied by the organization",
        "revoked" => "Revoked by the organization",
        _ => "Unknown",
    }
}

fn access_label(access: &str) -> &'static str {
    match access {
        "all" => "All repositories",
        "selected" => "Only selected repositories",
        _ => "Public repositories (read-only)",
    }
}

fn expiry(expires: &str) -> String {
    if expires == "never" {
        "Never expires".to_owned()
    } else {
        format!("Expires {}", compact_date(expires))
    }
}

/// When a token was last used, and from where, for both token lists.
pub(super) fn last_used(at: Option<&str>, ip: Option<&str>) -> String {
    match (at, ip) {
        (Some(at), Some(ip)) => format!("Last used {} from {ip}", compact_date(at)),
        (Some(at), None) => format!("Last used {}", compact_date(at)),
        _ => "Never used".to_owned(),
    }
}

/// The expiry choices; `never` only when the owner allows tokens without expiry.
fn expiry_fields(choice: &str, date: &str, never_allowed: bool) -> Element {
    let mut options = vec![
        SelectOption::new("7", "7 days"),
        SelectOption::new("30", "30 days"),
        SelectOption::new("60", "60 days"),
        SelectOption::new("90", "90 days"),
        SelectOption::new("custom", "Custom date"),
    ];
    if never_allowed {
        options.push(SelectOption::new("never", "No expiration"));
    }
    let choice = if choice.is_empty() { "30" } else { choice };
    rsx! {
        div { class: "identity-field-row",
            div { class: "space-y-2",
                label { r#for: "expiry", class: "block text-sm font-semibold", "Expiration" }
                DraftSelect { id: Some("expiry".to_owned()), name: "expiry", class: FIELD, initial_value: choice.to_owned(), options }
            }
            div { class: "space-y-2",
                label { r#for: "expires_at", class: "block text-sm font-semibold", "Custom date" }
                input { id: "expires_at", r#type: "date", name: "expires_at", class: FIELD, "value": date.to_owned() }
            }
        }
        p { class: "identity-form-hint", "A token stops working at the end of its last day, in UTC. The custom date is used only with Custom date." }
    }
}

pub fn render(model: Model) -> Element {
    let token = model.csrf_token.clone();
    let body = match &model.view {
        View::List { tokens, next_before } => list(tokens, *next_before),
        View::New { draft, choices } => new(&token, draft, choices),
        View::Show {
            token: detail,
            value,
        } => show(&token, detail, value.as_deref()),
    };
    rsx! {
        div { class: "identity-settings page-shell",
            header { class: "identity-settings-header",
                h1 { "Account settings" }
                p { "Manage your profile, credentials, and account access." }
            }
            {notice(model.message.as_deref(), model.is_error)}
            div { class: "identity-settings-layout",
                {settings_nav("fine-grained-tokens")}
                div { class: "identity-settings-content", {body} }
            }
        }
    }
}

fn list(tokens: &[Row], next_before: Option<i64>) -> Element {
    rsx! {
        section { class: "identity-settings-section", aria_labelledby: "fine-grained-title",
            header { class: "identity-section-header",
                h2 { id: "fine-grained-title", "Fine-grained personal access tokens" }
                p { "Each token reaches one owner's repositories with only the permissions you choose, and never more than your own access." }
                NavLink { href: format!("{BASE}/new"), class: SUBMIT, "Generate new token" }
            }
            div { class: "identity-row-list",
                if tokens.is_empty() {
                    div { class: "identity-empty", h3 { "No fine-grained tokens" } p { "Generate one for scripts, the gh CLI or Git over HTTPS." } }
                }
                if let Some(before) = next_before {
                    NavLink { href: format!("{BASE}?before={before}"), "Older tokens" }
                }
                for row in tokens {
                    article { class: "identity-row", key: "{row.id}",
                        div { class: "identity-row-main",
                            h3 { NavLink { href: format!("{BASE}/{}", row.id), "{row.name}" } }
                            p { "{row.owner} · {status_label(&row.status)} · {expiry(&row.expires)}" }
                            p { "{last_used(row.last_used.as_deref(), row.last_ip.as_deref())}" }
                        }
                    }
                }
            }
        }
    }
}

fn new(token: &str, draft: &Draft, choices: &Choices) -> Element {
    let groups = ["Repository", "Organization", "Account"];
    rsx! {
        section { class: "identity-settings-section", aria_labelledby: "fine-grained-new-title",
            header { class: "identity-section-header",
                h2 { id: "fine-grained-new-title", "New fine-grained personal access token" }
                p { "The token can never do more than your account can. Organization tokens may wait for an owner's approval." }
                NavLink { href: BASE.to_owned(), "Back to tokens" }
            }
            form { method: "post", action: BASE, class: "identity-form",
                {csrf(token)}
                input { r#type: "hidden", name: "owners_after", value: "{draft.owners_after}" }
                {text_field("name", "Token name", draft.name.clone(), "text", true, 100)}
                {text_field("description", "Description", draft.description.clone(), "text", false, 1024)}
                div { class: "space-y-2",
                    label { r#for: "owner", class: "block text-sm font-semibold", "Resource owner" }
                    DraftSelect { id: Some("owner".to_owned()), name: "owner", class: FIELD, initial_value: draft.owner.clone(),
                        options: choices.owners.iter().map(|(value, label)| SelectOption::new(value.clone(), label.clone())).collect::<Vec<_>>() }
                    button { r#type: "submit", name: "action", value: "refresh", formnovalidate: true, class: "identity-button", "Show this owner's repositories and permissions" }
                    if let Some(after) = choices.next_owners {
                        button { r#type: "submit", name: "action", value: "owners:{after}", formnovalidate: true, class: "identity-button", "More organizations" }
                    }
                    if draft.owners_after > 0 {
                        button { r#type: "submit", name: "action", value: "owners:0", formnovalidate: true, class: "identity-button", "First organizations" }
                    }
                }
                {expiry_fields(&draft.expiry, &draft.expires_at, choices.never_allowed)}
                fieldset { class: "identity-fieldset",
                    legend { "Repository access" }
                    for (value, label) in [("public", "Public repositories (read-only)"), ("all", "All repositories"), ("selected", "Only select repositories")] {
                        label { class: "identity-expiry-option",
                            input { r#type: "radio", name: "access", value: value, checked: draft.access == value || (draft.access.is_empty() && value == "public") }
                            span { "{label}" }
                        }
                    }
                    div { class: "identity-field-row",
                        div { class: "space-y-2",
                            label { r#for: "query", class: "block text-sm font-semibold", "Find repositories" }
                            input { id: "query", r#type: "search", name: "query", class: FIELD, maxlength: "100", "value": draft.query.clone() }
                        }
                        button { r#type: "submit", name: "action", value: "refresh", formnovalidate: true, class: "identity-button", "Search" }
                    }
                    if choices.repositories.is_empty() { p { class: "identity-form-hint", "No repositories match." } }
                    for (id, name) in &choices.repositories {
                        label { class: "repo-check-label", key: "{id}",
                            input { r#type: "checkbox", name: "repositories", value: "{id}", checked: draft.repositories.contains(id) }
                            " {name}"
                        }
                    }
                    if choices.more_repositories { p { class: "identity-form-hint", "More repositories match. Search to narrow the list." } }
                    p { class: "identity-form-hint", "Selected repositories are used only with Only select repositories, up to 100." }
                }
                for group in groups {
                    if choices.permissions.iter().any(|choice| choice.group == group) {
                        fieldset { class: "identity-fieldset",
                            legend { "{group} permissions" }
                            if group == "Repository" { p { class: "identity-form-hint", "Used with all or selected repositories. Metadata read is always included." } }
                            for choice in choices.permissions.iter().filter(|choice| choice.group == group) {
                                div { class: "identity-field-row", key: "{choice.key}",
                                    label { r#for: "permission-{choice.key}", class: "text-sm", "{choice.name}" }
                                    DraftSelect { id: Some(format!("permission-{}", choice.key)), name: format!("permission.{}", choice.key), class: FIELD,
                                        aria_label: Some(format!("{group} permission: {}", choice.name)),
                                        initial_value: draft.permissions.get(&choice.key).cloned().unwrap_or_else(|| "none".to_owned()),
                                        options: std::iter::once(SelectOption::new("none", "No access"))
                                            .chain(choice.levels.iter().map(|level| SelectOption::new(level.clone(), level_label(level))))
                                            .collect::<Vec<_>>() }
                                }
                            }
                        }
                    }
                }
                div { class: "identity-form-actions", button { r#type: "submit", name: "action", value: "create", class: SUBMIT, "Generate token" } }
            }
        }
    }
}

fn level_label(level: &str) -> &'static str {
    match level {
        "read" => "Read-only",
        "write" => "Read and write",
        "admin" => "Admin",
        _ => "Unknown",
    }
}

fn show(token: &str, detail: &Detail, value: Option<&str>) -> Element {
    let row = &detail.row;
    let repositories = detail.repositories.join(", ");
    rsx! {
        section { class: "identity-settings-section", aria_labelledby: "fine-grained-show-title",
            header { class: "identity-section-header",
                h2 { id: "fine-grained-show-title", "{row.name}" }
                if !detail.description.is_empty() { p { "{detail.description}" } }
                NavLink { href: BASE.to_owned(), "Back to tokens" }
            }
            if let Some(value) = value {
                div { class: "identity-token-reveal", role: "status",
                    strong { "Copy this token now" }
                    p { "You won't be able to see it again." }
                    code { "{value}" }
                }
            }
            dl { class: "org-facts",
                dt { "Resource owner" } dd { "{row.owner}" }
                dt { "Status" } dd { "{status_label(&row.status)}" if let Some(reason) = &detail.reason { " · {reason}" } }
                dt { "Repository access" } dd { "{access_label(&detail.access)}" }
                if detail.access == "selected" {
                    dt { "Repositories" } dd { "{repositories}" }
                }
                dt { "Permissions" }
                dd {
                    ul {
                        for (group, name, level) in &detail.permissions {
                            li { "{group}: {name} — {level_label(level)}" }
                        }
                    }
                }
                dt { "Created" } dd { "{compact_date(&detail.created)}" }
                dt { "Expiration" } dd { "{expiry(&row.expires)}" }
                dt { "Last used" } dd { "{last_used(row.last_used.as_deref(), row.last_ip.as_deref())}" }
            }
        }
        section { class: "identity-settings-section", aria_labelledby: "fine-grained-regenerate-title",
            header { class: "identity-section-header",
                h2 { id: "fine-grained-regenerate-title", "Regenerate token" }
                p { "A new value replaces the current one at once. Permissions, repositories and approval stay." }
            }
            form { method: "post", action: format!("{BASE}/{}/regenerate", row.id), class: "identity-form",
                {csrf(token)}
                {expiry_fields("30", "", detail.never_allowed)}
                div { class: "identity-form-actions", button { r#type: "submit", class: SUBMIT, "Regenerate token" } }
            }
        }
        section { class: "identity-settings-section", aria_labelledby: "fine-grained-delete-title",
            header { class: "identity-section-header", h2 { id: "fine-grained-delete-title", "Delete token" } }
            details { class: "collaboration-edit native-menu repo-delete-menu",
                summary { class: "btn btn-danger", "Delete" span { class: "site-sr-only", " {row.name}" } }
                {confirm_dialog(None, &format!("Delete token {}", row.name), &format!("{BASE}/{}", row.id), &format!("{BASE}/{}/delete", row.id), rsx! {
                    {csrf(token)}
                    strong { "Delete token {row.name}?" }
                    label { class: "repo-check-label", input { r#type: "checkbox", name: "confirm", value: "yes", required: true } " Revoke this token. Anything that uses it stops working." }
                }, "Delete token")}
            }
        }
    }
}
