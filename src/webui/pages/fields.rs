//! Shared native form controls, and the form values that survive a refused submission.
use crate::webui::components::compact_date;
use crate::webui::pages::{admin::Signup, auth::password_field};
use dioxus::prelude::*;
use std::collections::HashMap;

pub(in crate::webui::pages) const FIELD: &str = "w-full bg-background border border-border text-sm text-foreground focus:border-primary focus:outline-none";
pub(in crate::webui::pages) const SUBMIT: &str = "btn btn-primary";
pub(in crate::webui::pages) const PANEL: &str =
    "panel rounded-md border border-border bg-card p-6 space-y-5";

/// Explicit recovery fields prevent a generic form map from serializing secrets.
#[derive(Clone, Debug, Default, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FormValues {
    pub username: String,
    pub email: String,
    pub node_mode: String,
    pub display_name: String,
    pub bio: String,
    pub location: String,
    pub website: String,
    pub name: String,
    pub public_key: String,
    pub scope: String,
    pub expiry: String,
    pub expires_at: String,
    pub redirect: String,
    pub mode: String,
    pub days: String,
}
impl FormValues {
    pub fn get(&self, key: &str) -> String {
        match key {
            "username" => &self.username,
            "email" => &self.email,
            "node_mode" => &self.node_mode,
            "display_name" => &self.display_name,
            "bio" => &self.bio,
            "location" => &self.location,
            "website" => &self.website,
            "name" => &self.name,
            "public_key" => &self.public_key,
            "scope" => &self.scope,
            "expiry" => &self.expiry,
            "expires_at" => &self.expires_at,
            "redirect" => &self.redirect,
            "mode" => &self.mode,
            "days" => &self.days,
            _ => return String::new(),
        }
        .clone()
    }
}

/// Only explicitly non-secret values may survive a failed submission.
pub fn preserved_values(form: &HashMap<String, String>) -> FormValues {
    let get = |name: &str| form.get(name).cloned().unwrap_or_default();
    let public_key = get("public_key");
    FormValues {
        username: get("username"),
        email: get("email"),
        node_mode: get("node_mode"),
        display_name: get("display_name"),
        bio: get("bio"),
        location: get("location"),
        website: get("website"),
        name: get("name"),
        scope: get("scope"),
        expiry: get("expiry"),
        expires_at: get("expires_at"),
        public_key: if public_key.contains("PRIVATE KEY") {
            String::new()
        } else {
            public_key
        },
        redirect: get("redirect"),
        mode: get("mode"),
        days: get("days"),
    }
}

pub(in crate::webui::pages) fn csrf(token: &str) -> Element {
    rsx! { input { r#type: "hidden", name: "csrf_token", value: token.to_owned() } }
}
pub(in crate::webui::pages) fn notice(message: Option<&str>, error: bool) -> Element {
    let class = if error {
        "rounded-md border border-destructive/30 bg-destructive/10 p-3 text-destructive"
    } else {
        "rounded-md border border-border bg-muted/30 p-3 identity-notice-saved"
    };
    rsx! { if let Some(message) = message { p { role: "status", class, "{message}" } } }
}
pub(in crate::webui::pages) fn text_field(
    name: &str,
    label: &str,
    value: String,
    kind: &str,
    required: bool,
    max: usize,
) -> Element {
    rsx! { div { class: "space-y-2",
        label { r#for: name.to_owned(), class: "block text-sm font-semibold", "{label}" }
        // Native defaults: unrelated theme/chrome rerenders must not erase drafts.
        input { id: name.to_owned(), name: name.to_owned(), r#type: kind.to_owned(), "value": value,
            required, maxlength: max.to_string(), class: FIELD }
    } }
}
/// The form that creates an account with a temporary password. `with_admin` adds the
/// administrator checkbox; the new account is then made an administrator right after it is created.
pub(in crate::webui::pages) fn managed_account_form(
    token: &str,
    values: &FormValues,
    action: &str,
    with_admin: bool,
) -> Element {
    rsx! {
        form { method: "post", action: action.to_owned(), class: "identity-form",
            {csrf(token)}
            {text_field("username", "Username", values.get("username"), "text", true, 255)}
            {text_field("email", "Email (optional)", values.get("email"), "email", false, 320)}
            {password_field("password", "Temporary password", true)}
            if with_admin {
                label { class: "identity-check",
                    input { r#type: "checkbox", name: "is_admin" }
                    " Make administrator"
                }
            }
            p { class: "identity-form-hint", "The new user will be asked to change this password when they sign in." }
            div { class: "identity-form-actions", button { r#type: "submit", class: SUBMIT, "Create account" } }
        }
    }
}

pub(in crate::webui::pages) fn access_forms(
    requests: &[Signup],
    token: &str,
    values: &FormValues,
    prefix: &str,
) -> Element {
    rsx! {
        header { class: "identity-section-header",
            h2 { "Account access" }
            p { "Create accounts and review requests to join this private node." }
        }
        div { class: "identity-access-grid",
            section { class: "identity-access-form", aria_labelledby: "managed-account-title",
                h3 { id: "managed-account-title", "Create managed account" }
                {managed_account_form(token, values, &format!("{prefix}/users"), false)}
            }
            section { class: "identity-access-requests", aria_labelledby: "signup-requests-title",
                h3 { id: "signup-requests-title", "Pending signup requests" }
                div { class: "identity-row-list",
                    if requests.is_empty() {
                        div { class: "identity-empty", h4 { "No pending requests" } p { "New requests will appear here." } }
                    }
                    for request in requests {
                        article { class: "identity-row",
                            div { class: "identity-row-main",
                                h4 { "{request.username}" }
                                p { "{request.email} · Requested {compact_date(&request.created)}" }
                            }
                            form { method: "post", action: format!("{prefix}/requests/{}/resolve", request.id), class: "identity-row-actions",
                                {csrf(token)}
                                if prefix.starts_with("/admin/") { input { r#type: "hidden", name: "days", value: values.get("days") } }
                                button { r#type: "submit", name: "action", value: "approve", class: SUBMIT, "Approve" }
                                button { r#type: "submit", name: "action", value: "reject", class: "identity-button", "Reject" }
                            }
                        }
                    }
                }
            }
        }
    }
}
