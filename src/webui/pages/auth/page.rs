//! Authentication, setup and logout presentation.
use crate::webui::pages::fields::{csrf, FormValues, FIELD, PANEL, SUBMIT};
use crate::webui::{
    components::{DraftSelect, SelectOption},
    navigation::NavLink,
};
use dioxus::prelude::*;

pub(in crate::webui::pages) fn password_field(name: &str, label: &str, new: bool) -> Element {
    rsx! { PasswordField { key: "{name}-{new}", name: name.to_owned(), label: label.to_owned(), new_password: new } }
}

pub(super) fn password_strength(password: &str) -> u8 {
    if password.is_empty() {
        return 0;
    }
    u8::from(password.len() >= 8)
        + u8::from(password.chars().any(|c| c.is_ascii_uppercase()))
        + u8::from(password.chars().any(|c| c.is_ascii_digit()))
        + u8::from(password.chars().any(|c| !c.is_ascii_alphanumeric()))
}

/// Hooks live at this component boundary, never in the page render dispatcher.
/// Only a strength score is retained; password text stays in the native input.
#[component]
fn PasswordField(name: String, label: String, new_password: bool) -> Element {
    let mut revealed = use_signal(|| false);
    let mut ready = use_signal(|| false);
    let mut strength = use_signal(|| 0u8);
    let mut has_password = use_signal(|| false);
    use_effect(move || ready.set(true));
    let score = strength();
    let strength_label = match score {
        0 | 1 => "Weak",
        2 => "Fair",
        3 => "Good",
        _ => "Strong",
    };
    rsx! { div { class: "space-y-2",
        label { r#for: name.clone(), class: "block text-sm font-semibold", "{label}" }
        div { class: "flex gap-2 items-center",
            input { id: name.clone(), name: name, r#type: if revealed() { "text" } else { "password" }, required: true,
                maxlength: "4096", autocomplete: if new_password { "new-password" } else { "current-password" }, class: FIELD,
                oninput: move |event| {
                    let value = event.value();
                    has_password.set(!value.is_empty());
                    strength.set(password_strength(&value));
                } }
            button { r#type: "button", disabled: !ready(), class: "btn btn-secondary",
                "aria-label": if revealed() { "Hide password" } else { "Show password" },
                "aria-pressed": revealed().to_string(), onclick: move |_| revealed.toggle(),
                if revealed() { "Hide" } else { "Show" }
            }
        }
        if new_password && has_password() {
            p { role: "status", class: "text-xs text-muted-foreground", "Password strength: {strength_label}" }
            meter { min: "0", max: "4", value: score.to_string(), "aria-label": "Password strength", class: "w-full" }
        }
    } }
}

/// Display name for a provider id. Unknown ids get no sign-in link.
fn provider_label(id: &str) -> Option<&'static str> {
    match id {
        "github" => Some("GitHub"),
        "google" => Some("Google"),
        "gitlab" => Some("GitLab"),
        "discord" => Some("Discord"),
        "bitbucket" => Some("Bitbucket"),
        _ => None,
    }
}

#[expect(
    clippy::too_many_arguments,
    reason = "Pure auth view receives explicit safe presentation and native-form fields from typed Page dispatch."
)]
pub fn auth_page(
    mode: AuthMode,
    action: &str,
    token: &str,
    values: FormValues,
    private: bool,
    providers: Vec<String>,
    redirect: &str,
    message: Option<&str>,
    is_error: bool,
) -> Element {
    let signup = mode != AuthMode::Login;
    let heading = mode.title();
    let provider_links: Vec<(&str, &str)> = providers
        .iter()
        .filter_map(|id| provider_label(id).map(|label| (id.as_str(), label)))
        .collect();
    let submit = match mode {
        AuthMode::Login => "Sign in",
        AuthMode::Register if private => "Request access",
        AuthMode::Register => "Create account",
        AuthMode::Setup => "Complete setup",
    };
    let redirect_query = crate::webui::encode_component(redirect);
    rsx! {
        // Same trusted static CSS receiver as the profile; no user content enters raw HTML.
        style { dangerous_inner_html: include_str!("../account.css") }
        section { class: "identity-auth", "aria-labelledby": "identity-auth-title",
            header { class: "identity-auth-header",
                img { src: "/assets/logo.webp", alt: "Codeyard logo", width: "512", height: "512", class: "identity-auth-brand" }
                h1 { id: "identity-auth-title", "{heading}" }
                if mode == AuthMode::Setup {
                    p { "Create the initial administrator and choose who can access this node. Setup is only available before any accounts exist." }
                } else if mode == AuthMode::Register && private {
                    p { "This is a private node. An administrator must approve your signup request." }
                }
            }
            if let Some(message) = message {
                p { role: "status", class: if is_error { "identity-auth-notice identity-auth-notice-error" } else { "identity-auth-notice" }, "{message}" }
            }
            form { method: "post", action: action.to_owned(), class: "identity-auth-form",
                {csrf(token)}
                input { r#type: "hidden", name: "redirect", value: redirect.to_owned() }
                input { r#type: "hidden", name: "mode", value: if signup { "signup" } else { "signin" } }
                div { class: "identity-auth-field",
                    label { r#for: "username", if signup { "Username" } else { "Username or email" } }
                    // Raw default attributes preserve text entered before hydration/chrome updates.
                    // The backend allows 255 characters; HTML maxlength counts UTF-16 units.
                    input { id: "username", name: "username", r#type: "text", "value": values.get("username"),
                        autocomplete: "username", autocapitalize: "none", spellcheck: "false", required: true, maxlength: "510" }
                }
                if signup {
                    div { class: "identity-auth-field",
                        label { r#for: "email", "Email (optional)" }
                        input { id: "email", name: "email", r#type: "email", "value": values.get("email"),
                            autocomplete: "email", autocapitalize: "none", spellcheck: "false", maxlength: "320" }
                    }
                }
                div { class: "identity-auth-password",
                    {password_field("password", "Password", signup)}
                    if signup { p { class: "identity-auth-hint", "Use at least 8 characters." } }
                    if !signup {
                        NavLink { href: format!("/forgot-password?redirect={redirect_query}"), class: "identity-auth-link identity-auth-forgot", "Forgot password?" }
                    }
                }
                if signup { {password_field("confirm_password", "Confirm password", true)} }
                if mode == AuthMode::Setup {
                    div { class: "identity-auth-field",
                        label { r#for: "node_mode", "Node access" }
                        DraftSelect { id: Some("node_mode".to_owned()), name: "node_mode",
                            initial_value: String::from(if values.get("node_mode") == "private" { "private" } else { "public" }),
                            options: vec![
                                SelectOption::new("public", "Public"),
                                SelectOption::new("private", "Private — approved accounts only"),
                            ] }
                    }
                    div { class: "identity-auth-field",
                        label { r#for: "admin_secret", "Admin setup secret (if configured)" }
                        input { id: "admin_secret", name: "admin_secret", r#type: "password", autocomplete: "off", maxlength: "4096" }
                    }
                }
                button { r#type: "submit", class: "identity-auth-submit", "{submit}" }
            }
            if mode != AuthMode::Setup {
                if !provider_links.is_empty() {
                    div { class: "identity-auth-providers",
                        p { class: "identity-auth-provider-separator", "or" }
                        div { class: "identity-auth-provider-links",
                            for (id, label) in provider_links {
                                a { href: format!("/api/auth/oauth/{id}"), "Continue with {label}" }
                            }
                        }
                    }
                }
                p { class: "identity-auth-switch",
                    if mode == AuthMode::Login {
                        "New to Codeyard? "
                        NavLink { href: format!("/register?redirect={redirect_query}"), class: "identity-auth-link",
                            if private { "Request access" } else { "Create an account" }
                        }
                    } else {
                        "Already have an account? "
                        NavLink { href: format!("/login?redirect={redirect_query}"), class: "identity-auth-link", "Sign in" }
                    }
                }
            }
        }
    }
}

/// The page behind "Forgot password?". Codeyard sends no reset email and has no reset form, so
/// the page says so. It changes nothing.
pub fn password_help_page(redirect: &str) -> Element {
    let redirect_query = crate::webui::encode_component(redirect);
    rsx! {
        // Same trusted static CSS receiver as the sign-in page; no user content enters raw HTML.
        style { dangerous_inner_html: include_str!("../account.css") }
        section { class: "identity-auth", "aria-labelledby": "identity-auth-title",
            header { class: "identity-auth-header",
                img { src: "/assets/logo.webp", alt: "Codeyard logo", width: "512", height: "512", class: "identity-auth-brand" }
                h1 { id: "identity-auth-title", "Forgot your password?" }
                p { "Codeyard does not email password resets, and this page cannot change a password." }
                p { "Ask the administrator of this Codeyard server to help you get back into your account." }
            }
            p { class: "identity-auth-switch",
                NavLink { href: format!("/login?redirect={redirect_query}"), class: "identity-auth-link", "Back to sign in" }
            }
        }
    }
}

/// The second step of sign-in: a code from the authenticator app, or a recovery code. The sealed
/// `pending` token from the password step rides in a hidden field; no session exists yet.
pub fn two_factor_page(
    action: &str,
    token: &str,
    pending: &str,
    redirect: &str,
    message: Option<&str>,
    is_error: bool,
) -> Element {
    rsx! {
        style { dangerous_inner_html: include_str!("../account.css") }
        section { class: "identity-auth", aria_labelledby: "identity-auth-title",
            header { class: "identity-auth-header",
                img { src: "/assets/logo.webp", alt: "Codeyard logo", width: "512", height: "512", class: "identity-auth-brand" }
                h1 { id: "identity-auth-title", "Two-factor authentication" }
                p { "Enter the six-digit code from your authenticator app, or one of your recovery codes." }
            }
            if let Some(message) = message {
                p { role: if is_error { "alert" } else { "status" }, class: if is_error { "identity-auth-error" } else { "identity-auth-notice" }, "{message}" }
            }
            form { method: "post", action: action.to_owned(), class: "identity-form",
                {csrf(token)}
                input { r#type: "hidden", name: "pending", value: pending.to_owned() }
                input { r#type: "hidden", name: "redirect", value: redirect.to_owned() }
                div { class: "space-y-2",
                    label { r#for: "two-factor-code", class: "block text-sm font-semibold", "Code" }
                    input { id: "two-factor-code", r#type: "text", name: "code", class: FIELD, required: true, maxlength: "32",
                        autocomplete: "one-time-code", inputmode: "text", autofocus: true, spellcheck: "false" }
                    p { class: "identity-form-hint", "Recovery codes look like XXXX-XXXX-XXXX-XXXX and work once." }
                }
                div { class: "identity-form-actions", button { r#type: "submit", class: SUBMIT, "Verify" } }
            }
            p { class: "identity-auth-switch",
                NavLink { href: format!("/login?redirect={}", crate::webui::encode_component(redirect)), class: "identity-auth-link", "Start over" }
            }
        }
    }
}

pub fn logout_page(token: &str) -> Element {
    rsx! { div { class: "page-shell py-8 max-w-lg", section { class: PANEL,
        h1 { class: "text-2xl font-semibold", "Sign out" }
        p { "End your current session?" }
        form { method: "post", action: "/logout", {csrf(token)} button { r#type: "submit", class: SUBMIT, "Sign out" } }
        NavLink { href: "/".to_owned(), "Cancel" }
    } } }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum AuthMode {
    Login,
    Register,
    Setup,
}
impl AuthMode {
    pub fn title(self) -> &'static str {
        match self {
            Self::Login => "Sign in to Codeyard",
            Self::Register => "Create your account",
            Self::Setup => "Set up your node",
        }
    }
}
