//! Account settings presentation.
use crate::webui::components::{
    compact_date, current_time, Avatar, DraftSelect, DraftTextarea, SelectOption,
};
use crate::webui::document::Theme;
use crate::webui::navigation::NavLink;
use crate::webui::pages::admin::Signup;
use crate::webui::pages::auth::password_field;
use crate::webui::pages::fields::{
    access_forms, csrf, notice, text_field, FormValues, FIELD, SUBMIT,
};
use crate::webui::pages::users::{profile_icon, Profile, ProfileIcon};
use dioxus::prelude::*;

/// The appearance choices, in the order the tab lists them: value, label, one-line description.
const APPEARANCE_CHOICES: [(Theme, &str, &str); 3] = [
    (
        Theme::System,
        "System",
        "Follow the light or dark setting of this device.",
    ),
    (Theme::Light, "Light", "Always use the light colour scheme."),
    (Theme::Dark, "Dark", "Always use the dark colour scheme."),
];

#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Settings {
    pub profile: Profile,
    pub email: String,
    pub tab: String,
    pub keys: Vec<Key>,
    pub tokens: Vec<Token>,
    pub new_token: Option<String>,
    pub password_supported: bool,
    pub must_change_password: bool,
    pub manage_access: bool,
    pub requests: Vec<Signup>,
    pub values: FormValues,
    /// The theme saved on this device, read from the same cookie as the header toggle.
    pub theme: Theme,
}

#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Token {
    pub id: i64,
    pub name: String,
    pub scope: String,
    pub expires: String,
    pub created: String,
}

/// One link in the account settings navigation. The current item carries `aria-current` and the
/// current-item colour; its label says so for screen readers.
fn nav_link(href: String, label: &str, icon: ProfileIcon, current: bool) -> Element {
    let class = if current {
        "identity-settings-nav-link identity-settings-nav-current"
    } else {
        "identity-settings-nav-link"
    };
    rsx! {
        NavLink {
            href,
            class,
            aria_label: current.then(|| format!("{label}, current page")),
            aria_current: current.then(|| "page".to_owned()),
            {profile_icon(icon)}
            span { "{label}" }
        }
    }
}

/// The day a new token expires on when the form does not name one yet: 30 days from today, in UTC.
fn token_expiry_day(submitted: String) -> String {
    if submitted.is_empty() {
        (current_time() + chrono::Duration::days(30))
            .format("%Y-%m-%d")
            .to_string()
    } else {
        submitted
    }
}

/// The account settings navigation, in three groups. Organization pages share it; `current` is
/// the tab or page being shown.
pub fn settings_nav(current: &str, manage_access: bool) -> Element {
    rsx! {
        nav { class: "identity-settings-nav", aria_label: "Account settings",
            p { class: "identity-settings-nav-title", "Account" }
            {nav_link("/settings?tab=profile".to_owned(), "Public profile", ProfileIcon::User, current == "profile")}
            {nav_link("/settings?tab=appearance".to_owned(), "Appearance", ProfileIcon::Appearance, current == "appearance")}
            {nav_link("/settings?tab=password".to_owned(), "Password", ProfileIcon::Lock, current == "password")}
            p { class: "identity-settings-nav-title identity-settings-nav-section", "Access" }
            {nav_link("/settings?tab=keys".to_owned(), "SSH keys", ProfileIcon::Terminal, current == "keys")}
            {nav_link("/settings?tab=tokens".to_owned(), "Access tokens", ProfileIcon::Key, current == "tokens")}
            {nav_link("/settings/organizations".to_owned(), "Organizations", ProfileIcon::Building, current == "organizations")}
            if manage_access {
                p { class: "identity-settings-nav-title identity-settings-nav-section", "Administration" }
                {nav_link("/settings?tab=access".to_owned(), "Account access", ProfileIcon::Shield, current == "access")}
            }
        }
    }
}

pub fn settings_page(model: Settings, token: &str, message: Option<&str>, error: bool) -> Element {
    let values = &model.values;
    rsx! {
        style { dangerous_inner_html: include_str!("../account.css") }
        div { class: "identity-settings page-shell",
            header { class: "identity-settings-header",
                h1 { "Account settings" }
                p { "Manage your profile, credentials, and account access." }
            }
            {notice(message, error)}
            if model.must_change_password {
                div { class: "identity-settings-alert", role: "status",
                    div { strong { "Change your temporary password" } p { "Choose a permanent password before continuing." } }
                    NavLink { href: "/settings?tab=password".to_owned(), class: "identity-button", "Change password" }
                }
            }
            div { class: "identity-settings-layout",
                {settings_nav(&model.tab, model.manage_access)}
                div { class: "identity-settings-content",
                    if model.tab == "profile" {
                        section { class: "identity-settings-section", aria_labelledby: "settings-profile-title",
                            header { class: "identity-section-header",
                                h2 { id: "settings-profile-title", "Public profile" }
                                p { "This information appears on your profile and repositories." }
                            }
                            form { method: "post", action: "/settings/profile", class: "identity-form",
                                {csrf(token)}
                                {text_field("display_name", "Name", values.get("display_name"), "text", false, 100)}
                                div { class: "space-y-2",
                                    label { r#for: "bio", class: "block text-sm font-semibold", "Bio" }
                                    DraftTextarea { id: Some("bio".to_owned()), name: "bio", initial_value: values.get("bio"), rows: 4, maxlength: Some(4000), class: FIELD }
                                    p { class: "identity-form-hint", "A short introduction shown on your profile." }
                                }
                                {text_field("location", "Location", values.get("location"), "text", false, 200)}
                                {text_field("website", "Website", values.get("website"), "url", false, 255)}
                                div { class: "identity-form-actions", button { r#type: "submit", class: SUBMIT, "Save profile" } }
                            }
                            p { class: "identity-account-meta", "Signed in as @{model.profile.username} · {model.email}" }
                        }
                        section { class: "identity-settings-section", aria_labelledby: "settings-avatar-title",
                            header { class: "identity-section-header",
                                h2 { id: "settings-avatar-title", "Profile picture" }
                                p { "Use a square image for the best result." }
                            }
                            div { class: "identity-avatar-editor",
                                // An uploaded picture is linked by its own address, so a new upload shows at once;
                                // /avatars/NAME is cached for a few minutes and would show the old one.
                                if let Some(avatar) = model.profile.avatar_url.as_deref() {
                                    img { src: avatar, alt: "Current profile picture", class: "identity-settings-avatar", width: "72", height: "72" }
                                } else {
                                    Avatar { name: model.profile.username.clone(), size: 72 }
                                }
                                div { class: "identity-avatar-actions",
                                    form { id: "profile-picture-upload", method: "post", action: "/settings/avatar", enctype: "multipart/form-data", class: "identity-form identity-avatar-form",
                                        {csrf(token)}
                                        label { r#for: "picture", class: "block text-sm font-semibold", "Upload a new picture" }
                                        input { id: "picture", r#type: "file", name: "picture", accept: "image/png,image/jpeg,image/gif,image/webp", required: true, class: FIELD }
                                        p { class: "identity-form-hint", "PNG, JPEG, GIF, or WebP. Maximum 2 MB." }
                                    }
                                    div { class: "identity-avatar-buttons",
                                        button { r#type: "submit", form: "profile-picture-upload", class: "btn btn-primary", "Upload picture" }
                                        if model.profile.avatar_url.is_some() {
                                            form { method: "post", action: "/settings/avatar/remove",
                                                {csrf(token)} button { r#type: "submit", class: "btn btn-danger", "Remove picture" }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    } else if model.tab == "keys" {
                        section { class: "identity-settings-section", aria_labelledby: "settings-keys-title",
                            header { class: "identity-section-header",
                                h2 { id: "settings-keys-title", "SSH keys" }
                                p { "Keys let you clone and push over SSH." }
                            }
                            div { class: "identity-row-list",
                                if model.keys.is_empty() {
                                    div { class: "identity-empty", h3 { "No SSH keys" } p { "Add a key to use Git over SSH." } }
                                }
                                for key in &model.keys {
                                    article { class: "identity-row",
                                        div { class: "identity-row-main",
                                            h3 { "{key.name}" }
                                            code { "{key.fingerprint}" }
                                            p { "{key.kind} · Added {compact_date(&key.created)}" }
                                        }
                                        form { method: "post", action: format!("/settings/keys/{}/delete", key.id),
                                            {csrf(token)} button { r#type: "submit", class: "btn btn-danger btn-sm", "Delete" }
                                        }
                                    }
                                }
                            }
                        }
                        section { class: "identity-settings-section", aria_labelledby: "settings-add-key-title",
                            header { class: "identity-section-header", h2 { id: "settings-add-key-title", "Add SSH key" } }
                            form { method: "post", action: "/settings/keys", class: "identity-form",
                                {csrf(token)}
                                {text_field("name", "Title", values.get("name"), "text", true, 100)}
                                div { class: "space-y-2",
                                    label { r#for: "public_key", class: "block text-sm font-semibold", "Public key" }
                                    DraftTextarea { id: Some("public_key".to_owned()), name: "public_key", initial_value: values.get("public_key"), required: true, rows: 5, maxlength: Some(16384), class: FIELD }
                                    p { class: "identity-form-hint", "Paste the contents of your public key. Never share a private key." }
                                }
                                div { class: "identity-form-actions", button { r#type: "submit", class: SUBMIT, "Add SSH key" } }
                            }
                        }
                    } else if model.tab == "tokens" {
                        section { class: "identity-settings-section", aria_labelledby: "settings-tokens-title",
                            header { class: "identity-section-header",
                                h2 { id: "settings-tokens-title", "Personal access tokens" }
                                p { "Use a token as your password for Git over HTTPS." }
                            }
                            if let Some(value) = &model.new_token {
                                div { class: "identity-token-reveal", role: "status",
                                    strong { "Copy this token now" }
                                    p { "You won't be able to see it again." }
                                    code { "{value}" }
                                }
                            }
                            div { class: "identity-row-list",
                                if model.tokens.is_empty() {
                                    div { class: "identity-empty", h3 { "No access tokens" } p { "Create a token to use Git over HTTPS." } }
                                }
                                for access in &model.tokens {
                                    article { class: "identity-row",
                                        div { class: "identity-row-main",
                                            h3 { "{access.name}" }
                                            p { "{access.scope} access · Added {compact_date(&access.created)} · Expires {compact_date(&access.expires)}" }
                                        }
                                        details { class: "collaboration-edit",
                                            summary { class: "btn btn-danger", "Delete" span { class: "repo-sr-only", " {access.name}" } }
                                            form { method: "post", action: format!("/settings/tokens/{}/delete", access.id), class: "collaboration-form-fields",
                                                {csrf(token)}
                                                label { class: "collaboration-checkbox", input { r#type: "checkbox", name: "confirm", value: "yes", required: true } " Revoke this token. Anything that uses it stops working." }
                                                div { class: "collaboration-actions", button { r#type: "submit", class: "btn btn-danger", "Delete token" } }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                        section { class: "identity-settings-section", aria_labelledby: "settings-create-token-title",
                            header { class: "identity-section-header", h2 { id: "settings-create-token-title", "Create token" } }
                            form { method: "post", action: "/settings/tokens", class: "identity-form",
                                {csrf(token)}
                                {text_field("name", "Token name", values.get("name"), "text", true, 100)}
                                div { class: "space-y-2",
                                    label { r#for: "scope", class: "block text-sm font-semibold", "Repository access" }
                                    DraftSelect { id: Some("scope".to_owned()), name: "scope", class: FIELD,
                                        initial_value: String::from(if values.get("scope") == "write" { "write" } else { "read" }),
                                        options: vec![
                                            SelectOption::new("read", "Read — clone and fetch"),
                                            SelectOption::new("write", "Write — clone, fetch, and push"),
                                        ] }
                                }
                                fieldset { class: "identity-expiry",
                                    legend { class: "block text-sm font-semibold", "Expiration" }
                                    label { class: "identity-expiry-option",
                                        input { r#type: "radio", name: "expiry", value: "never", checked: values.get("expiry") == "never" }
                                        span { "Never expires" }
                                    }
                                    label { class: "identity-expiry-option",
                                        input { r#type: "radio", name: "expiry", value: "date", checked: values.get("expiry") != "never" }
                                        span { "Expires on" }
                                        input { r#type: "date", name: "expires_at", class: FIELD, value: token_expiry_day(values.get("expires_at")) }
                                    }
                                    p { class: "identity-form-hint", "A token that expires stops working at the end of the chosen day, in UTC." }
                                }
                                div { class: "identity-form-actions", button { r#type: "submit", class: SUBMIT, "Create token" } }
                            }
                        }
                    } else if model.tab == "appearance" {
                        section { class: "identity-settings-section", aria_labelledby: "settings-appearance-title",
                            header { class: "identity-section-header",
                                h2 { id: "settings-appearance-title", "Appearance" }
                                p { "Choose how Codeyard looks on this device." }
                            }
                            div { class: "identity-appearance-list",
                                for (choice, label, detail) in APPEARANCE_CHOICES {
                                    form { method: "post", action: "/_forms/theme", class: "identity-appearance-row",
                                        {csrf(token)}
                                        input { r#type: "hidden", name: "return_to", value: "/settings?tab=appearance" }
                                        input { r#type: "hidden", name: "theme", value: choice.as_str() }
                                        div { class: "identity-appearance-text",
                                            strong { "{label}" }
                                            p { "{detail}" }
                                        }
                                        if choice == model.theme {
                                            span { class: "count-badge", "Current" }
                                        }
                                        button { r#type: "submit", class: "identity-button", "aria-current": if choice == model.theme { "true" } else { "false" }, "Use {label}" }
                                    }
                                }
                            }
                        }
                    } else if model.tab == "password" {
                        section { class: "identity-settings-section", aria_labelledby: "settings-password-title",
                            header { class: "identity-section-header",
                                h2 { id: "settings-password-title", "Change password" }
                                p { "Use a strong password you don't use elsewhere." }
                            }
                            if model.password_supported {
                                form { method: "post", action: "/settings/password", class: "identity-form",
                                    {csrf(token)}
                                    {password_field("current_password", "Current password", false)}
                                    {password_field("new_password", "New password", true)}
                                    {password_field("confirm_password", "Confirm new password", true)}
                                    p { class: "identity-form-hint", "Use at least 8 characters." }
                                    div { class: "identity-form-actions", button { r#type: "submit", class: SUBMIT, "Update password" } }
                                }
                            } else {
                                div { class: "identity-empty", h3 { "Password changes aren't available" } p { "This account uses OAuth and does not support password changes." } }
                            }
                        }
                    } else if model.tab == "access" && model.manage_access {
                        section { class: "identity-settings-section identity-access-section", {access_forms(&model.requests, token, values, "/settings/access")} }
                    }
                }
            }
        }
    }
}

#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Key {
    pub id: i64,
    pub name: String,
    pub fingerprint: String,
    pub kind: String,
    pub created: String,
}

pub fn settings_tab(raw: Option<&str>) -> &'static str {
    match raw {
        Some("appearance") => "appearance",
        Some("keys") => "keys",
        Some("tokens") => "tokens",
        Some("password") => "password",
        Some("access") => "access",
        _ => "profile",
    }
}
