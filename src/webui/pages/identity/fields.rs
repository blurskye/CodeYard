//! Shared native identity form controls.
use super::*;

pub(super) fn csrf(token: &str) -> Element {
    rsx! { input { r#type: "hidden", name: "csrf_token", value: token.to_owned() } }
}
pub(super) fn notice(message: Option<&str>, error: bool) -> Element {
    let class = if error {
        "rounded-md border border-destructive/30 bg-destructive/10 p-3 text-destructive"
    } else {
        "rounded-md border border-border bg-muted/30 p-3 identity-notice-saved"
    };
    rsx! { if let Some(message) = message { p { role: "status", class, "{message}" } } }
}
pub(super) fn text_field(
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
pub(super) fn access_forms(
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
                form { method: "post", action: format!("{prefix}/users"), class: "identity-form",
                    {csrf(token)}
                    if prefix.starts_with("/admin/") { input { r#type: "hidden", name: "days", value: values.get("days") } }
                    {text_field("username", "Username", values.get("username"), "text", true, 255)}
                    {text_field("email", "Email (optional)", values.get("email"), "email", false, 320)}
                    {password_field("password", "Temporary password", true)}
                    p { class: "identity-form-hint", "The new user will be asked to change this password when they sign in." }
                    div { class: "identity-form-actions", button { r#type: "submit", class: SUBMIT, "Create account" } }
                }
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
