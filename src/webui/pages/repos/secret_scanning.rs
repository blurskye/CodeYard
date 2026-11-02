//! Security views contain alert metadata, never credential values.
use super::chrome::{chrome, csrf, repository_id, RepoChrome};
use crate::webui::{
    components::{DraftInput, DraftSelect, SelectOption},
    navigation::NavLink,
};
use dioxus::prelude::*;

#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct CustomPattern {
    pub id: i64,
    pub name: String,
    pub state: String,
    pub organization: bool,
}

#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Alert {
    pub number: i64,
    pub name: String,
    pub secret_type: String,
    pub state: String,
    pub resolution: String,
}

#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Location {
    pub path: String,
    pub commit: String,
    pub line: i64,
}

#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum Page {
    Disabled,
    Alerts {
        rows: Vec<Alert>,
        state: String,
        secret_type: String,
        next: Option<String>,
    },
    Alert {
        alert: Alert,
        locations: Vec<Location>,
        next: Option<String>,
    },
    Bypass {
        id: String,
        token_type: String,
        allowed: bool,
    },
}

pub fn render(repo: &RepoChrome, page: Page) -> Element {
    let base = format!("{}/security/secret-scanning", repo.base());
    chrome(
        repo,
        rsx! {
            section { class: "panel repo-settings-section", style:"min-width: 0; overflow-wrap: anywhere;",
                header { class: "repo-section-heading",
                    div { h2 { "Secret scanning" } p { "Review credentials found in this repository." } }
                    NavLink { href: format!("{}/settings?tab=security",repo.base()), "Code security settings" }
                }
                div { class: "repo-form-body",
                    match page {
                        Page::Disabled => rsx! { p { "Secret scanning is disabled. Enable it in Code security settings to scan this repository’s history." } NavLink {class:"btn btn-secondary",href:format!("{}/settings?tab=security",repo.base()),"Enable secret scanning"} },
                        Page::Alerts { rows, state, secret_type, next } => rsx! {
                            form { method: "get", action: base.clone(), class: "repo-form-body",
                                label { class: "repo-field", span { "State" } DraftSelect { name: "state", initial_value: state, options: vec![SelectOption {value:"open".into(),label:"Open".into()},SelectOption {value:"resolved".into(),label:"Closed".into()}] } }
                                label { class: "repo-field", span { "Secret type" } DraftInput { name: "secret_type", initial_value: secret_type } }
                                button { class: "btn btn-secondary", r#type: "submit", "Filter" }
                            }
                            if rows.is_empty() { p { "No alerts match these filters." } }
                            for alert in rows {
                                div { class: "repo-section-heading",
                                    NavLink { href: format!("{base}/{}",alert.number), "{alert.name} #{alert.number}" }
                                    span { "{alert.state}" }
                                }
                            }
                            if let Some(next) = next { NavLink { href: next, "Next" } }
                        },
                        Page::Alert { alert, locations, next } => rsx! {
                            h3 { "{alert.name} #{alert.number}" }
                            p { "{alert.state} {alert.resolution}" }
                            p { "Revoke exposed credentials with their provider before closing this alert." }
                            for location in locations {
                                div { class: "panel", style: "padding: 12px; overflow-wrap: anywhere;",
                                    NavLink { href: format!("{}/blob/{}/{}?view=code&line={}#L{}",repo.base(),location.commit,location.path.split('/').map(crate::webui::encode_component).collect::<Vec<_>>().join("/"),location.line,location.line), "{location.path}:{location.line}" }
                                    p { code { "{location.commit}" } }
                                }
                            }
                            if let Some(next) = next { NavLink { href: next, "More locations" } }
                            form { method: "post", action: format!("{base}/{}",alert.number), class: "repo-form-body",
                                {csrf(&repo.csrf_token)} {repository_id(repo.id)}
                                if alert.state == "open" {
                                    input { r#type: "hidden", name: "state", value: "resolved" }
                                    label { class: "repo-field", span { "Close as" } select { name: "resolution",
                                        option { value: "revoked", "Revoked" }
                                        option { value: "false_positive", "False positive" }
                                        option { value: "used_in_tests", "Used in tests" }
                                        option { value: "wont_fix", "Won't fix" }
                                    } }
                                    button { class: "btn btn-primary", r#type: "submit", "Close alert" }
                                } else {
                                    input { r#type: "hidden", name: "state", value: "open" }
                                    button { class: "btn btn-secondary", r#type: "submit", "Reopen alert" }
                                }
                            }
                        },
                        Page::Bypass { id, token_type, allowed } => rsx! {
                            h3 { "Allow a secret in this push" }
                            p { style: "overflow-wrap: anywhere;", "Secret type: {token_type}" }
                            if allowed {
                                p { role: "status", "Bypass granted for three hours. Retry your push." }
                            } else {
                                p { "Only bypass push protection when you understand the risk." }
                                form { method: "post", action: format!("{base}/unblock-secret/{id}"), class: "repo-form-body",
                                    {csrf(&repo.csrf_token)} {repository_id(repo.id)}
                                    label { class: "repo-field", span { "Reason" } DraftSelect { name: "reason",initial_value:"",required:true,options:vec![
                                        SelectOption::new("","Choose a reason"),SelectOption::new("false_positive","It's a false positive"),SelectOption::new("used_in_tests","It's used in tests"),SelectOption::new("will_fix_later","I'll fix it later")
                                    ] } }
                                    button { class: "btn btn-primary", r#type: "submit", "Allow this secret" }
                                }
                            }
                        },
                    }
                }
            }
        },
    )
}
