//! Account settings presentation.
use crate::webui::components::{
    compact_date, confirm_dialog, current_time, Avatar, DraftSelect, DraftTextarea, SelectOption,
};
use crate::webui::document::Theme;
use crate::webui::navigation::NavLink;
use crate::webui::pages::admin::Signup;
use crate::webui::pages::auth::{password_field, password_field_with_id};
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
    pub sessions: Vec<AccountSession>,
    pub new_token: Option<String>,
    pub password_supported: bool,
    pub must_change_password: bool,
    pub manage_access: bool,
    pub requests: Vec<Signup>,
    pub requests_count: usize,
    pub requests_page: usize,
    pub values: FormValues,
    /// The theme saved on this device, read from the same cookie as the header toggle.
    pub theme: Theme,
    pub two_factor: TwoFactor,
}

/// Two-factor sign-in on this account. `setup` is set while a new secret waits for its confirming
/// code; `recovery_codes` is set only in the one response that shows them.
#[derive(Clone, Debug, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct TwoFactor {
    pub enabled: bool,
    pub setup: Option<TwoFactorSetup>,
    pub recovery_codes: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct TwoFactorSetup {
    /// The base32 secret, typed into an app that cannot scan a code.
    pub secret: String,
    pub uri: String,
    /// Sealed by the server; the confirming form sends it back unchanged.
    pub token: String,
}

/// One signed-in session of the account. `key` is a digest of the session id, which stays secret.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct AccountSession {
    pub key: String,
    pub current: bool,
    pub created: String,
    pub expires: String,
    pub user_agent: String,
    pub ip_address: String,
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
    let mut groups = vec![
        (
            "Account",
            vec![
                (
                    "/settings?tab=profile",
                    "Public profile",
                    "profile",
                    ProfileIcon::User,
                ),
                (
                    "/settings?tab=appearance",
                    "Appearance",
                    "appearance",
                    ProfileIcon::Appearance,
                ),
                (
                    "/settings?tab=email",
                    "Email address",
                    "email",
                    ProfileIcon::User,
                ),
                (
                    "/settings?tab=password",
                    "Password",
                    "password",
                    ProfileIcon::Lock,
                ),
                (
                    "/settings?tab=two-factor",
                    "Two-factor authentication",
                    "two-factor",
                    ProfileIcon::Shield,
                ),
            ],
        ),
        (
            "Access",
            vec![
                (
                    "/settings?tab=keys",
                    "SSH keys",
                    "keys",
                    ProfileIcon::Terminal,
                ),
                (
                    "/settings?tab=tokens",
                    "Access tokens",
                    "tokens",
                    ProfileIcon::Key,
                ),
                (
                    "/settings?tab=sessions",
                    "Sessions",
                    "sessions",
                    ProfileIcon::Layers,
                ),
                (
                    "/settings/organizations",
                    "Organizations",
                    "organizations",
                    ProfileIcon::Building,
                ),
            ],
        ),
    ];
    if manage_access {
        groups.push((
            "Administration",
            vec![(
                "/settings?tab=access",
                "Account access",
                "access",
                ProfileIcon::Shield,
            )],
        ));
    }
    let label = groups
        .iter()
        .flat_map(|(_, tabs)| tabs)
        .find(|tab| tab.2 == current)
        .map_or("Settings", |tab| tab.1);
    rsx! {
        // A phone has no room for the tab column, so the same links are one menu there.
        details { class: "identity-settings-picker",
            summary { aria_label: "Account settings sections",
                span { "{label}" }
                svg { width: "16", height: "16", view_box: "0 0 24 24", fill: "none", stroke: "currentColor", stroke_width: "2", stroke_linecap: "round", stroke_linejoin: "round", "aria-hidden": "true", path { d: "m6 9 6 6 6-6" } }
            }
            div { class: "identity-settings-picker-panel",
                for &(href, text, id, _) in groups.iter().flat_map(|(_, tabs)| tabs) {
                    NavLink {
                        href: href.to_owned(),
                        class: if id == current { "identity-settings-nav-current".to_owned() } else { String::new() },
                        aria_current: (id == current).then(|| "page".to_owned()),
                        "{text}"
                    }
                }
            }
        }
        nav { class: "identity-settings-nav", aria_label: "Account settings",
            for (index, (title, tabs)) in groups.iter().enumerate() {
                p {
                    class: if index == 0 { "identity-settings-nav-title" } else { "identity-settings-nav-title identity-settings-nav-section" },
                    "{title}"
                }
                for &(href, text, id, icon) in tabs {
                    {nav_link(href.to_owned(), text, icon, id == current)}
                }
            }
        }
    }
}

pub fn settings_page(model: Settings, token: &str, message: Option<&str>, error: bool) -> Element {
    let values = &model.values;
    // The header toggle changes the theme without reloading the settings model.
    let current_theme = try_consume_context::<crate::webui::app::CurrentPage>()
        .map_or(model.theme, |page| page.0.read().chrome.theme);
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
                                {text_field("display_name", "Name", values.display_name.clone(), "text", false, 100)}
                                div { class: "space-y-2",
                                    label { r#for: "bio", class: "block text-sm font-semibold", "Bio" }
                                    DraftTextarea { id: Some("bio".to_owned()), name: "bio", initial_value: values.bio.clone(), rows: 4, maxlength: Some(4000), class: FIELD }
                                    p { class: "identity-form-hint", "A short introduction shown on your profile." }
                                }
                                {text_field("location", "Location", values.location.clone(), "text", false, 200)}
                                {text_field("website", "Website", values.website.clone(), "url", false, 255)}
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
                                        details { class: "native-menu repo-delete-menu",
                                            summary { class: "btn btn-danger btn-sm", "Delete" span { class: "site-sr-only", " {key.name}" } }
                                            {confirm_dialog(None, &format!("Delete SSH key {}", key.name), "/settings?tab=keys", &format!("/settings/keys/{}/delete", key.id), rsx! {
                                                {csrf(token)}
                                                strong { "Delete SSH key {key.name}?" }
                                                label { class: "repo-check-label", input { r#type: "checkbox", required: true, name: "confirm", value: "yes" } " Git over SSH with this key stops working." }
                                            }, "Delete key")}
                                        }
                                    }
                                }
                            }
                        }
                        if model.keys.len() == 100 {
                            if let Some(last) = model.keys.last() {
                                NavLink { href: format!("/settings?tab=keys&keys_after={}",last.id), class: "identity-button", "Next SSH keys" }
                            }
                        }
                        section { class: "identity-settings-section", aria_labelledby: "settings-add-key-title",
                            header { class: "identity-section-header", h2 { id: "settings-add-key-title", "Add SSH key" } }
                            form { method: "post", action: "/settings/keys", class: "identity-form",
                                {csrf(token)}
                                {text_field("name", "Title", values.name.clone(), "text", true, 100)}
                                div { class: "space-y-2",
                                    label { r#for: "public_key", class: "block text-sm font-semibold", "Public key" }
                                    DraftTextarea { id: Some("public_key".to_owned()), name: "public_key", initial_value: values.public_key.clone(), required: true, rows: 5, maxlength: Some(16384), class: FIELD }
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
                                        details { class: "collaboration-edit native-menu repo-delete-menu",
                                            summary { class: "btn btn-danger", "Delete" span { class: "site-sr-only", " {access.name}" } }
                                            {confirm_dialog(None, &format!("Delete token {}", access.name), "/settings?tab=tokens", &format!("/settings/tokens/{}/delete", access.id), rsx! {
                                                {csrf(token)}
                                                strong { "Delete token {access.name}?" }
                                                label { class: "repo-check-label", input { r#type: "checkbox", name: "confirm", value: "yes", required: true } " Revoke this token. Anything that uses it stops working." }
                                            }, "Delete token")}
                                        }
                                    }
                                }
                            }
                        }
                        section { class: "identity-settings-section", aria_labelledby: "settings-create-token-title",
                            header { class: "identity-section-header", h2 { id: "settings-create-token-title", "Create token" } }
                            form { method: "post", action: "/settings/tokens", class: "identity-form",
                                {csrf(token)}
                                {text_field("name", "Token name", values.name.clone(), "text", true, 100)}
                                div { class: "space-y-2",
                                    label { r#for: "scope", class: "block text-sm font-semibold", "Repository access" }
                                    DraftSelect { id: Some("scope".to_owned()), name: "scope", class: FIELD,
                                        initial_value: String::from(if values.scope == "write" { "write" } else { "read" }),
                                        options: vec![
                                            SelectOption::new("read", "Read — clone and fetch"),
                                            SelectOption::new("write", "Write — clone, fetch, and push"),
                                        ] }
                                }
                                fieldset { class: "identity-expiry",
                                    legend { class: "block text-sm font-semibold", "Expiration" }
                                    label { class: "identity-expiry-option",
                                        input { r#type: "radio", name: "expiry", value: "never", checked: values.expiry == "never" }
                                        span { "Never expires" }
                                    }
                                    label { class: "identity-expiry-option",
                                        input { r#type: "radio", name: "expiry", value: "date", checked: values.expiry != "never" }
                                        span { "Expires on" }
                                        input { r#type: "date", name: "expires_at", class: FIELD, value: token_expiry_day(values.expires_at.clone()) }
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
                                        if choice == current_theme {
                                            span { class: "count-badge", "Current" }
                                        }
                                        button { r#type: "submit", class: "identity-button", "aria-current": if choice == current_theme { "true" } else { "false" }, "Use {label}" }
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
                    } else if model.tab == "email" {
                        section { class: "identity-settings-section", aria_labelledby: "settings-email-title",
                            header { class: "identity-section-header",
                                h2 { id: "settings-email-title", "Email address" }
                                p { "Your sign-in and account notices go to this address. Changing it needs your current password." }
                            }
                            if model.password_supported {
                                form { method: "post", action: "/settings/email", class: "identity-form",
                                    {csrf(token)}
                                    {text_field("email", "New email address", if values.email.is_empty() { model.email.clone() } else { values.email.clone() }, "email", true, 255)}
                                    {password_field("current_password", "Current password", false)}
                                    div { class: "identity-form-actions", button { r#type: "submit", class: SUBMIT, "Update email" } }
                                }
                            } else {
                                div { class: "identity-empty", h3 { "Email changes aren't available" } p { "This account uses OAuth and does not support email changes." } }
                            }
                        }
                    } else if model.tab == "sessions" {
                        section { class: "identity-settings-section", aria_labelledby: "settings-sessions-title",
                            header { class: "identity-section-header",
                                h2 { id: "settings-sessions-title", "Signed-in sessions" }
                                p { "Each browser or device you are signed in on. Revoking one signs it out at once." }
                            }
                            div { class: "identity-row-list",
                                for session in &model.sessions {
                                    article { class: "identity-row", key: "{session.key}",
                                        div { class: "identity-row-main",
                                            h3 {
                                                if session.user_agent.is_empty() { "Unknown browser" } else { "{session.user_agent}" }
                                                if session.current { span { class: "identity-status identity-status-success", " This session" } }
                                            }
                                            p { "Signed in {session.created} · Expires {session.expires} · " if session.ip_address.is_empty() { "IP unknown" } else { "IP {session.ip_address}" } }
                                        }
                                        details { class: "collaboration-edit native-menu repo-delete-menu",
                                            summary { class: "btn", "Revoke" }
                                            {confirm_dialog(None, "Revoke this session", "/settings?tab=sessions", "/settings/sessions/revoke", rsx! {
                                                {csrf(token)}
                                                input { r#type: "hidden", name: "session", value: session.key.clone() }
                                                strong { if session.current { "Revoke this session? You will be signed out here." } else { "Revoke this session?" } }
                                                p { class: "text-sm", "The browser or device using it is signed out immediately." }
                                            }, "Revoke session")}
                                        }
                                    }
                                }
                            }
                            if model.sessions.iter().any(|session| !session.current) {
                                details { class: "collaboration-edit native-menu repo-delete-menu identity-sessions-everywhere",
                                    summary { class: "btn btn-danger", "Sign out everywhere else" }
                                    {confirm_dialog(None, "Sign out everywhere else", "/settings?tab=sessions", "/settings/sessions/revoke-others", rsx! {
                                        {csrf(token)}
                                        strong { "Sign out every other session?" }
                                        p { class: "text-sm", "Other browsers and devices are signed out. This one stays signed in." }
                                    }, "Sign out everywhere else")}
                                }
                            }
                        }
                    } else if model.tab == "two-factor" {
                        {two_factor_section(&model.two_factor, model.password_supported, token)}
                    } else if model.tab == "access" && model.manage_access {
                        section { class: "identity-settings-section identity-access-section", {access_forms(&model.requests, token, values, "/settings/access")}
                            {crate::webui::pages::admin::signup_pagination(model.requests_page, model.requests_count, "/settings?tab=access")} }
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
        Some("two-factor") => "two-factor",
        Some("email") => "email",
        Some("sessions") => "sessions",
        Some("access") => "access",
        _ => "profile",
    }
}

/// Two-factor sign-in: the state, the one-time recovery codes, and the forms that change it. Each
/// change asks for the current password; setup also asks for a code.
fn two_factor_section(two_factor: &TwoFactor, password_supported: bool, token: &str) -> Element {
    rsx! {
        section { class: "identity-settings-section", aria_labelledby: "settings-two-factor-title",
            header { class: "identity-section-header",
                h2 { id: "settings-two-factor-title", "Two-factor authentication" }
                p { "Also ask for a code from an authenticator app when you sign in with your password." }
            }
            if !two_factor.recovery_codes.is_empty() {
                div { class: "identity-settings-alert", role: "status",
                    div {
                        strong { "Save these recovery codes" }
                        p { "Each code signs you in once if you lose your device. They are not shown again." }
                    }
                    ul { class: "identity-recovery-codes",
                        for code in &two_factor.recovery_codes {
                            li { key: "{code}", code { "{code}" } }
                        }
                    }
                }
            }
            if !password_supported {
                p { "Two-factor enrollment requires an account password. Manage sign-in security with your identity provider." }
            } else if two_factor.enabled {
                p { class: "identity-status identity-status-success", "On. Sign-in asks for a code from your app." }
                form { method: "post", action: "/settings/two-factor/recovery-codes", class: "identity-form",
                    {csrf(token)}
                    {password_field("current_password", "Current password", false)}
                    div { class: "identity-form-actions",
                        button { r#type: "submit", class: SUBMIT, "Regenerate recovery codes" }
                    }
                }
                details { class: "collaboration-edit native-menu repo-delete-menu",
                    summary { class: "btn btn-danger", "Turn off two-factor authentication" }
                    {confirm_dialog(None, "Turn off two-factor authentication", "/settings?tab=two-factor", "/settings/two-factor/disable", rsx! {
                        {csrf(token)}
                        strong { "Turn off two-factor authentication?" }
                        p { class: "text-sm", "Your password alone will sign you in, and every recovery code stops working." }
                        {password_field_with_id("disable-two-factor-password", "current_password", "Current password")}
                    }, "Turn off")}
                }
            } else if let Some(setup) = &two_factor.setup {
                p { "In your authenticator app, add an account with this key, or open the setup link on a phone that has the app." }
                p { class: "identity-form-hint", "Key" }
                div { class: "identity-copy-box", code { "{setup.secret}" } }
                p { class: "identity-form-hint", "Setup link" }
                div { class: "identity-copy-box", code { "{setup.uri}" } }
                form { method: "post", action: "/settings/two-factor/enable", class: "identity-form",
                    {csrf(token)}
                    input { r#type: "hidden", name: "setup", value: setup.token.clone() }
                    {password_field("current_password", "Current password", false)}
                    {text_field("code", "Code from your app", String::new(), "text", true, 16)}
                    div { class: "identity-form-actions", button { r#type: "submit", class: SUBMIT, "Turn on" } }
                }
            } else {
                form { method: "post", action: "/settings/two-factor/setup", class: "identity-form",
                    {csrf(token)}
                    p { "You need an authenticator app, such as one that supports TOTP codes." }
                    {password_field("current_password", "Current password", false)}
                    div { class: "identity-form-actions",
                        button { r#type: "submit", class: SUBMIT, "Set up two-factor authentication" }
                    }
                }
            }
        }
    }
}
