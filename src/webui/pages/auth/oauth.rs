//! OAuth approval uses native forms before and after hydration.
use crate::webui::pages::fields::{csrf, FIELD, PANEL, SUBMIT};
use dioxus::prelude::*;

pub fn device(
    token: &str,
    code: &str,
    application: Option<(String, String)>,
    message: Option<&str>,
) -> Element {
    rsx! { section { class: "identity-auth",
        header { class: "identity-auth-header", h1 { "Device activation" } }
        if let Some(message) = message { p { role: "status", "{message}" } }
        form { method: "post", action: "/login/device", class: "identity-auth-form",
            {csrf(token)}
            if let Some((name, scopes)) = application {
                h2 { class: "font-semibold break-all", "Authorize {name}" }
                p { "Only approve if this code is shown on a device you are using. Never enter a code sent by someone else." }
                p { class: "break-all", "Requested permissions: {scopes}" }
                p { class: "font-mono", "{code}" }
                input { r#type: "hidden", name: "user_code", value: code.to_owned() }
                button { r#type: "submit", name: "decision", value: "approve", class: SUBMIT, "Authorize device" }
                button { r#type: "submit", name: "decision", value: "deny", class: "btn btn-secondary", "Cancel" }
            } else {
                label { r#for: "user_code", "Enter the code displayed on your device" }
                input { id: "user_code", name: "user_code", required: true, maxlength: "9", autocomplete: "off", class: FIELD }
                button { r#type: "submit", class: SUBMIT, "Continue" }
            }
        }
    } }
}

pub fn authorize(
    token: &str,
    name: &str,
    client_id: &str,
    redirect: &str,
    scopes: &str,
    state: &str,
    challenge: &str,
) -> Element {
    rsx! { section { class: "identity-auth",
        header { class: "identity-auth-header", h1 { "Authorize {name}" } }
        p { class: "break-all", "Requested permissions: {scopes}" }
        p { class: "break-all", "You will return to {redirect}." }
        form { method: "post", action: "/login/oauth/authorize", class: "identity-auth-form",
            {csrf(token)}
            input { r#type: "hidden", name: "client_id", value: client_id.to_owned() }
            input { r#type: "hidden", name: "redirect_uri", value: redirect.to_owned() }
            input { r#type: "hidden", name: "scope", value: scopes.to_owned() }
            input { r#type: "hidden", name: "state", value: state.to_owned() }
            input { r#type: "hidden", name: "code_challenge", value: challenge.to_owned() }
            input { r#type: "hidden", name: "code_challenge_method", value: "S256" }
            button { r#type: "submit", name: "decision", value: "approve", class: SUBMIT, "Authorize application" }
            button { r#type: "submit", name: "decision", value: "deny", class: "btn btn-secondary", "Cancel" }
        }
    } }
}

pub fn register(token: &str, credentials: Option<(String, String)>) -> Element {
    rsx! { section { class: "identity-auth",
        header { class: "identity-auth-header", h1 { "Register OAuth application" } }
        if let Some((id, secret)) = credentials {
            p { "Copy your client secret now. It will not be shown again." }
            dl { class: "min-w-0",
                dt { "Client ID" } dd { class: "break-all font-mono", "{id}" }
                dt { "Client secret" } dd { class: "break-all font-mono", "{secret}" }
            }
            a { href: "/settings/applications", "Your OAuth applications" }
        } else {
            form { method: "post", action: "/settings/applications/new", class: "identity-auth-form",
                {csrf(token)}
                label { r#for: "name", "Application name" }
                input { id: "name", name: "name", required: true, maxlength: "100", class: FIELD }
                label { r#for: "homepage", "Homepage URL" }
                input { id: "homepage", name: "homepage", r#type: "url", required: true, class: FIELD }
                label { r#for: "redirect_uri", "Authorization callback URL" }
                input { id: "redirect_uri", name: "redirect_uri", r#type: "url", required: true, class: FIELD }
                button { r#type: "submit", class: SUBMIT, "Register application" }
            }
        }
    } }
}

pub fn applications(
    applications: Vec<(String, String, String)>,
    previous: Option<String>,
    next: Option<String>,
) -> Element {
    rsx! { section { class: "identity-auth",
        header { class: "identity-auth-header", h1 { "OAuth applications" } }
        a { href: "/settings/applications/new", class: "btn btn-primary", "New OAuth application" }
        for (name, client_id, redirect) in applications {
            article { class: PANEL,
                h2 { class: "font-semibold break-all", "{name}" }
                p { class: "break-all font-mono", "{client_id}" }
                p { class: "break-all", "{redirect}" }
            }
        }
        if previous.is_some() || next.is_some() {
            nav { aria_label: "OAuth application pages", class: "mt-4 flex gap-4",
                if let Some(previous) = previous { a { href: previous, "Previous" } }
                if let Some(next) = next { a { href: next, "Next" } }
            }
        }
    } }
}
