//! The deploy keys section of repository settings: the keys the repository has, and a form to add
//! one. Every control is a plain form or link, so the section works without JavaScript.
use crate::webui::components::{compact_date, confirm_dialog, DraftTextarea};
use dioxus::prelude::*;

/// What the section shows. The backend builds it after it has checked the viewer may manage keys.
#[derive(Clone, Debug, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct DeployKeyPanel {
    /// The section's own URL, which its redirects and the delete dialogs' cancel links use.
    pub base: String,
    /// The repository form endpoint, `/owner/repo/_forms`.
    pub action_base: String,
    pub csrf_token: String,
    pub repository_id: i64,
    pub keys: Vec<DeployKeyRow>,
    /// The add form as it was typed, so a refused add comes back with its values.
    pub title: String,
    pub public_key: String,
    pub can_write: bool,
    pub notice: Option<String>,
    pub error: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct DeployKeyRow {
    pub id: i64,
    pub title: String,
    pub fingerprint: String,
    pub key_type: String,
    pub can_write: bool,
    /// RFC 3339.
    pub added: String,
    /// RFC 3339, or None before the first use.
    pub last_used: Option<String>,
}

impl DeployKeyPanel {
    /// The form URL for one action.
    pub fn action(&self, name: &str) -> String {
        format!("{}/{name}", self.action_base)
    }
}

#[component]
pub fn DeployKeySection(panel: DeployKeyPanel) -> Element {
    rsx! {
        section { class: "repo-settings-section panel deploy-key-section", aria_labelledby: "deploy-keys-title",
            header { class: "repo-section-heading",
                div {
                    h3 { id: "deploy-keys-title", "Deploy keys" }
                    p { "SSH keys that give one server access to this repository. They are read-only unless write access is allowed." }
                }
            }
            div { class: "deploy-key-body",
                if let Some(notice) = &panel.notice { div { class: "repo-alert repo-alert-success", role: "status", "{notice}" } }
                if let Some(error) = &panel.error { div { class: "repo-alert repo-alert-error", role: "alert", "{error}" } }
                if panel.keys.is_empty() {
                    p { class: "deploy-key-empty", "No deploy keys yet." }
                } else {
                    ul { class: "deploy-key-list",
                        for key in panel.keys.iter() {
                            DeployKeyItem { base: panel.base.clone(), action: panel.action("deploy-key-delete"), csrf_token: panel.csrf_token.clone(), repository_id: panel.repository_id, entry: key.clone() }
                        }
                    }
                }
                form { class: "deploy-key-form", method: "post", action: panel.action("deploy-key-add"),
                    input { r#type: "hidden", name: "csrf_token", value: panel.csrf_token.clone() }
                    input { r#type: "hidden", name: "repository_id", value: panel.repository_id.to_string() }
                    h4 { "Add deploy key" }
                    label { class: "repo-field", span { "Title" } input { r#type: "text", name: "title", required: true, maxlength: 255, value: panel.title.clone(), placeholder: "Build server" } }
                    label { class: "repo-field", span { "Public key" } DraftTextarea { name: "key", initial_value: panel.public_key.clone(), rows: 3, required: true } small { "Paste the contents of the .pub file, such as ssh-ed25519 AAAA…" } }
                    label { class: "repo-check-label", input { r#type: "checkbox", name: "can_write", checked: panel.can_write } " Allow write access" }
                    div { class: "deploy-key-actions",
                        button { class: "btn btn-primary", r#type: "submit", "Add deploy key" }
                    }
                }
            }
        }
    }
}

#[component]
fn DeployKeyItem(
    base: String,
    action: String,
    csrf_token: String,
    repository_id: i64,
    entry: DeployKeyRow,
) -> Element {
    let used = match &entry.last_used {
        Some(used) => format!("Last used {}", compact_date(used)),
        None => "Never used".to_owned(),
    };
    let access = if entry.can_write {
        "Read and write"
    } else {
        "Read only"
    };
    rsx! {
        li { class: "deploy-key-row",
            div { class: "deploy-key-main",
                strong { class: "deploy-key-name", "{entry.title}" }
                span { class: "deploy-key-meta", code { "{entry.fingerprint}" } " · {entry.key_type}" }
                span { class: "deploy-key-meta", "Added {compact_date(&entry.added)} · {used}" }
            }
            span { class: "deploy-key-badge", "{access}" }
            a { class: "btn btn-danger btn-sm", href: format!("#deploy-key-delete-{}", entry.id), "Delete" span { class: "site-sr-only", " deploy key {entry.title}" } }
            {confirm_dialog(Some(&format!("deploy-key-delete-{}", entry.id)), &format!("Delete deploy key {}", entry.title), &base, &action, rsx! {
                input { r#type: "hidden", name: "csrf_token", value: csrf_token }
                input { r#type: "hidden", name: "repository_id", value: repository_id.to_string() }
                input { r#type: "hidden", name: "key_id", value: entry.id.to_string() }
                strong { "Delete this deploy key?" }
                label { class: "repo-check-label", input { r#type: "checkbox", required: true, name: "confirm", value: "yes" } " The server loses access to this repository." }
            }, "Delete deploy key")}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn one_key_row_needs_no_copy_of_the_key_catalogue() {
        crate::webui::tests::in_scope(|| {
            let html = dioxus_ssr::render_element(rsx! { DeployKeyItem {
                base: "/u/r/settings/keys", action: "/u/r/_forms/deploy-key-delete", csrf_token: "token", repository_id: 12,
                entry: DeployKeyRow { id: 9, title: "build".into(), ..Default::default() }
            } });
            assert!(html.contains("value=\"12\""));
            assert!(html.contains("name=\"key_id\" value=\"9\""));
            assert!(html.contains("Delete deploy key build"));
        });
    }
}
