//! The webhooks section of repository and organization settings: the hook list, the form for
//! one hook, and each hook's recent deliveries. Every control is a plain form or link, so the
//! section works without JavaScript.
use crate::webui::components::confirm_dialog;
use dioxus::prelude::*;

/// What the section shows. The backend builds it after it has checked the viewer may manage
/// hooks. It never carries a secret, only whether one is stored.
#[derive(Clone, Debug, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct WebhookPanel {
    /// The section's own URL, which its links and redirects use.
    pub base: String,
    /// The URL the section's forms post to, with an optional query: `/a/b/_forms` or
    /// `/settings/organizations/1/webhooks?tab=webhooks`.
    pub action_base: String,
    pub csrf_token: String,
    /// Hidden on repository forms, which refuse a form for another repository.
    pub repository_id: Option<i64>,
    pub hooks: Vec<HookRow>,
    pub first_href: Option<String>,
    pub next_href: Option<String>,
    pub editing: Option<HookDraft>,
    pub detail: Option<HookDetail>,
    pub notice: Option<String>,
    pub error: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct HookRow {
    pub id: i64,
    pub url: String,
    pub events: String,
    pub active: bool,
    /// The last delivery's status, or "No deliveries yet".
    pub last: String,
    pub open_href: String,
}

/// The form for one hook. `id` is None for a new hook.
#[derive(Clone, Debug, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct HookDraft {
    pub id: Option<i64>,
    pub payload_url: String,
    pub content_type: String,
    /// The checked events, as stored. `*` checks everything.
    pub events: Vec<String>,
    pub active: bool,
    pub ssl_verify: bool,
    pub has_secret: bool,
}

#[derive(Clone, Debug, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct HookDetail {
    pub id: i64,
    pub url: String,
    pub deliveries: Vec<DeliveryRow>,
    pub open: Option<DeliveryDetail>,
}

#[derive(Clone, Debug, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct DeliveryRow {
    pub id: i64,
    /// The event, and the action when it has one: `issues · opened`.
    pub label: String,
    pub status: String,
    pub ok: bool,
    pub duration: String,
    pub delivered: String,
    pub href: String,
}

#[derive(Clone, Debug, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct DeliveryDetail {
    pub id: i64,
    pub guid: String,
    /// The event and status on one line.
    pub summary: String,
    pub status: String,
    pub error: String,
    pub request_headers: String,
    pub request_body: String,
    pub response_headers: String,
    pub response_body: String,
}

/// The events a form offers, with their labels.
pub const EVENT_OPTIONS: [(&str, &str); 11] = [
    ("push", "Pushes"),
    ("create", "Branch or tag created"),
    ("delete", "Branch or tag deleted"),
    ("fork", "Forks"),
    ("star", "Stars"),
    ("release", "Releases"),
    ("issues", "Issues"),
    ("issue_comment", "Issue comments"),
    ("pull_request", "Pull requests"),
    ("pull_request_review", "Pull request reviews"),
    (
        "pull_request_review_comment",
        "Pull request review comments",
    ),
];

impl WebhookPanel {
    /// The form URL for one action, keeping any query the section was given.
    pub fn action(&self, name: &str) -> String {
        match self.action_base.split_once('?') {
            Some((path, query)) => format!("{path}/{name}?{query}"),
            None => format!("{}/{name}", self.action_base),
        }
    }
}

#[component]
pub fn WebhookSection(panel: WebhookPanel) -> Element {
    rsx! {
        section { class: "repo-settings-section panel webhook-section", aria_labelledby: "webhooks-title",
            header { class: "repo-section-heading",
                div {
                    h3 { id: "webhooks-title", "Webhooks" }
                    p { "Send a request to a URL when things happen here. Each request is signed when the hook has a secret." }
                }
            }
            div { class: "webhook-body",
                if let Some(notice) = &panel.notice { div { class: "repo-alert repo-alert-success", role: "status", "{notice}" } }
                if let Some(error) = &panel.error { div { class: "repo-alert repo-alert-error", role: "alert", strong { "Webhook was not saved" } p { "{error}" } } }
                if let Some(detail) = &panel.detail {
                    HookDetailView { panel: panel.clone(), detail: detail.clone() }
                } else if let Some(draft) = &panel.editing {
                    HookEditor { panel: panel.clone(), draft: draft.clone() }
                } else {
                    HookList { panel: panel.clone() }
                }
            }
        }
    }
}

#[component]
fn HookList(panel: WebhookPanel) -> Element {
    rsx! {
        div { class: "webhook-toolbar",
            a { class: "btn btn-primary", href: format!("{}&hook=new", panel.base), "Add webhook" }
        }
        if panel.hooks.is_empty() {
            p { class: "webhook-empty", "No webhooks yet. A webhook sends a POST request to the URL you choose when the events you pick happen." }
        }
        ul { class: "webhook-list",
            for hook in panel.hooks.iter() {
                li { class: "webhook-row", key: "{hook.id}",
                    div { class: "webhook-row-main",
                        a { class: "webhook-url", href: hook.open_href.clone(), "{hook.url}" }
                        p { class: "webhook-meta",
                            span { class: if hook.active { "webhook-badge webhook-badge-on" } else { "webhook-badge" }, if hook.active { "Active" } else { "Disabled" } }
                            span { " · {hook.events}" }
                            span { " · {hook.last}" }
                        }
                    }
                    div { class: "webhook-row-actions",
                        a { class: "btn btn-secondary btn-sm", href: format!("{}&hook={}&edit=1", panel.base, hook.id), title: "Edit this webhook", "Edit" }
                        form { method: "post", action: panel.action("webhook-ping"), class: "webhook-inline-form",
                            input { r#type: "hidden", name: "csrf_token", value: panel.csrf_token.clone() }
                            if let Some(id) = panel.repository_id { input { r#type: "hidden", name: "repository_id", value: id.to_string() } }
                            input { r#type: "hidden", name: "hook_id", value: hook.id.to_string() }
                            button { class: "btn btn-secondary btn-sm", r#type: "submit", title: "Send a ping to this URL", "Ping" }
                        }
                        a { class: "btn btn-danger btn-sm", href: format!("#webhook-delete-{}", hook.id), "Delete" span { class: "site-sr-only", " webhook {hook.url}" } }
                        {confirm_dialog(Some(&format!("webhook-delete-{}", hook.id)), &format!("Delete webhook {}", hook.url), &panel.base, &panel.action("webhook-delete"), rsx! {
                                input { r#type: "hidden", name: "csrf_token", value: panel.csrf_token.clone() }
                                if let Some(id) = panel.repository_id { input { r#type: "hidden", name: "repository_id", value: id.to_string() } }
                                input { r#type: "hidden", name: "hook_id", value: hook.id.to_string() }
                                strong { "Delete this webhook?" }
                                label { class: "repo-check-label", input { r#type: "checkbox", required: true, name: "confirm", value: "yes" } " Deliveries stop, and their history is removed." }
                            }, "Delete webhook")}
                    }
                }
            }
        }
        nav { aria_label: "Webhook pages",
            if let Some(href) = panel.first_href { a { class: "btn btn-secondary", href, "First page" } }
            if let Some(href) = panel.next_href { a { class: "btn btn-secondary", href, "Next page" } }
        }
    }
}

#[component]
fn HookEditor(panel: WebhookPanel, draft: HookDraft) -> Element {
    let everything = draft.events.iter().any(|event| event == "*");
    let title = if draft.id.is_some() {
        "Edit webhook"
    } else {
        "Add webhook"
    };
    rsx! {
        form { class: "repo-form-body webhook-form", method: "post", action: panel.action("webhook-save"),
            input { r#type: "hidden", name: "csrf_token", value: panel.csrf_token.clone() }
            if let Some(id) = panel.repository_id { input { r#type: "hidden", name: "repository_id", value: id.to_string() } }
            if let Some(id) = draft.id { input { r#type: "hidden", name: "hook_id", value: id.to_string() } }
            h4 { "{title}" }
            label { class: "repo-field",
                span { "Payload URL" }
                input { r#type: "url", name: "payload_url", required: true, maxlength: "2048", value: draft.payload_url.clone(), placeholder: "https://example.com/hooks" }
                small { "Requests are sent as POST to this address. Private and loopback addresses are refused." }
            }
            label { class: "repo-field",
                span { "Content type" }
                select { name: "content_type",
                    option { value: "application/json", selected: draft.content_type != "application/x-www-form-urlencoded", "application/json" }
                    option { value: "application/x-www-form-urlencoded", selected: draft.content_type == "application/x-www-form-urlencoded", "application/x-www-form-urlencoded" }
                }
            }
            label { class: "repo-field",
                span { "Secret" }
                input { r#type: "password", name: "secret", autocomplete: "new-password", maxlength: "1024", placeholder: if draft.has_secret { "Leave blank to keep the saved secret" } else { "Optional" } }
                small { if draft.has_secret { "A secret is saved and never shown again. Enter a new one to replace it." } else { "Used to sign each request with X-Hub-Signature-256." } }
            }
            fieldset { class: "collaboration-option-group",
                legend { "Which events" }
                label { class: "repo-check-label",
                    input { r#type: "checkbox", name: "events", value: "*", checked: everything }
                    " Send me everything"
                }
                for (name, label) in EVENT_OPTIONS {
                    label { class: "repo-check-label", key: "{name}",
                        input { r#type: "checkbox", name: "events", value: name, checked: draft.events.iter().any(|event| event == name) || everything }
                        " {label}"
                    }
                }
            }
            label { class: "repo-check-label",
                input { r#type: "checkbox", name: "active", value: "yes", checked: draft.active }
                " Active"
            }
            label { class: "repo-check-label",
                input { r#type: "checkbox", name: "ssl_verify", value: "yes", checked: draft.ssl_verify }
                " Enable SSL verification"
            }
            div { class: "repo-form-actions",
                a { class: "btn btn-secondary", href: panel.base.clone(), "Cancel" }
                button { class: "btn btn-primary", r#type: "submit", "Save webhook" }
            }
        }
    }
}

#[component]
fn HookDetailView(panel: WebhookPanel, detail: HookDetail) -> Element {
    rsx! {
        div { class: "webhook-toolbar",
            a { class: "btn btn-secondary btn-sm", href: panel.base.clone(), "Back to webhooks" }
            a { class: "btn btn-secondary btn-sm", href: format!("{}&hook={}&edit=1", panel.base, detail.id), "Edit" }
        }
        h4 { class: "webhook-detail-title", "Recent deliveries" }
        p { class: "webhook-meta", "{detail.url}" }
        if detail.deliveries.is_empty() {
            p { class: "webhook-empty", "No deliveries yet." }
        }
        ul { class: "webhook-list",
            for row in detail.deliveries.iter() {
                li { class: "webhook-row", key: "{row.id}",
                    div { class: "webhook-row-main",
                        a { class: "webhook-url", href: row.href.clone(), "{row.label}" }
                        p { class: "webhook-meta",
                            span { class: if row.ok { "webhook-badge webhook-badge-on" } else { "webhook-badge webhook-badge-off" }, "{row.status}" }
                            span { " · {row.duration}" }
                            span { " · {row.delivered}" }
                        }
                    }
                }
            }
        }
        if let Some(open) = &detail.open {
            DeliveryView { panel: panel.clone(), hook_id: detail.id, open: open.clone() }
        }
    }
}

#[component]
fn DeliveryView(panel: WebhookPanel, hook_id: i64, open: DeliveryDetail) -> Element {
    rsx! {
        section { class: "webhook-delivery", aria_label: "Delivery details",
            header { class: "webhook-delivery-head",
                h4 { "Delivery {open.guid}" }
                form { method: "post", action: panel.action("webhook-redeliver"), class: "webhook-inline-form",
                    input { r#type: "hidden", name: "csrf_token", value: panel.csrf_token.clone() }
                    if let Some(id) = panel.repository_id { input { r#type: "hidden", name: "repository_id", value: id.to_string() } }
                    input { r#type: "hidden", name: "hook_id", value: hook_id.to_string() }
                    input { r#type: "hidden", name: "delivery_id", value: open.id.to_string() }
                    button { class: "btn btn-secondary btn-sm", r#type: "submit", title: "Send this request again to the current URL", "Redeliver" }
                }
            }
            p { class: "webhook-meta", "{open.summary}" }
            if !open.error.is_empty() { p { class: "repo-alert repo-alert-error", "{open.error}" } }
            h5 { "Request headers" }
            pre { class: "webhook-pre", "{open.request_headers}" }
            h5 { "Request body" }
            pre { class: "webhook-pre", "{open.request_body}" }
            h5 { "Response headers" }
            pre { class: "webhook-pre", "{open.response_headers}" }
            h5 { "Response body" }
            pre { class: "webhook-pre", "{open.response_body}" }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn panel() -> WebhookPanel {
        WebhookPanel {
            base: "/a/b/settings?tab=webhooks".into(),
            action_base: "/a/b/_forms".into(),
            csrf_token: "token-1".into(),
            repository_id: Some(3),
            hooks: vec![HookRow {
                id: 7,
                url: "https://hooks.example/<script>".into(),
                events: "push, issues".into(),
                active: true,
                last: "No deliveries yet".into(),
                open_href: "/a/b/settings?tab=webhooks&hook=7".into(),
            }],
            ..WebhookPanel::default()
        }
    }

    #[test]
    fn list_posts_native_forms_and_escapes_the_url() {
        let html = crate::webui::tests::in_scope(|| {
            dioxus_ssr::render_element(rsx! { WebhookSection { panel: panel() } })
        });
        assert!(html.contains("action=\"/a/b/_forms/webhook-ping\""));
        assert!(html.contains("action=\"/a/b/_forms/webhook-delete\""));
        assert!(html.contains("name=\"repository_id\" value=\"3\""));
        assert!(html.contains("name=\"hook_id\" value=\"7\""));
        assert!(html.contains("hooks.example/"));
        assert!(!html.contains("<script>"));
    }

    #[test]
    fn editor_never_renders_the_saved_secret() {
        let mut panel = panel();
        panel.editing = Some(HookDraft {
            id: Some(7),
            payload_url: "https://hooks.example/a".into(),
            content_type: "application/json".into(),
            events: vec!["*".into()],
            active: true,
            ssl_verify: false,
            has_secret: true,
        });
        let html = crate::webui::tests::in_scope(|| {
            dioxus_ssr::render_element(rsx! { WebhookSection { panel: panel.clone() } })
        });
        assert!(html.contains("Leave blank to keep the saved secret"));
        assert!(html.contains("name=\"secret\""));
        assert!(!html.contains("name=\"secret\" value="));
        assert!(html.contains("value=\"*\""));
        assert!(html.contains("name=\"ssl_verify\""));
    }
}
