//! Admin email page: the outgoing mail server, with a button that sends a test message.
//! The password is never page data: the page only says whether one is set.
use crate::webui::components::{DraftSelect, SelectOption};
use crate::webui::pages::admin::{admin_layout, Section};
use crate::webui::pages::fields::{csrf, notice, text_field, FIELD, SUBMIT};
use dioxus::prelude::*;
use serde::{Deserialize, Serialize};

/// The mail server as the form shows it. Also the answer of the JSON API.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Settings {
    /// `database`, `environment` or `none`: where the settings in use come from.
    pub source: String,
    pub host: String,
    pub port: String,
    /// `starttls`, `tls` or `none`.
    pub security: String,
    pub username: String,
    pub password_set: bool,
    pub from_name: String,
    pub from_address: String,
    pub reply_to: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Page {
    pub csrf_token: String,
    pub settings: Settings,
    /// Without SECRETS_MASTER_KEY a password cannot be saved.
    pub can_store_password: bool,
    /// The scheme and host of PUBLIC_URL that links in mail start with. Empty when it is unset.
    pub public_url: String,
    pub message: Option<String>,
    pub is_error: bool,
}

fn source_note(source: &str) -> &'static str {
    match source {
        "database" => "These settings are saved in the database.",
        "environment" => "These settings come from the CODEYARD_SMTP_* environment variables. Saving here stores them in the database, which then takes over.",
        _ => "No mail server is set up, so no email is sent.",
    }
}

/// The badge beside the title: where the settings in use come from.
fn source_badge(source: &str) -> Element {
    let (class, label) = match source {
        "database" => ("identity-status identity-status-success", "Set up"),
        "environment" => (
            "identity-status identity-status-success",
            "From environment",
        ),
        _ => ("identity-status", "Not set up"),
    };
    rsx! { span { class, "{label}" } }
}

/// A component, so its select runs inside the renderer's runtime.
#[component]
fn EmailForm(page: Page) -> Element {
    let settings = &page.settings;
    rsx! {
        form { method: "post", action: "/admin/email", class: "identity-form identity-email-form",
            {csrf(&page.csrf_token)}
            fieldset { class: "identity-fieldset",
                legend { "Server" }
                div { class: "identity-field-row",
                    {text_field("host", "Host", settings.host.clone(), "text", true, 255)}
                    {text_field("port", "Port", settings.port.clone(), "text", false, 5)}
                }
                div { class: "space-y-2",
                    label { r#for: "security", class: "block text-sm font-semibold", "Encryption" }
                    DraftSelect { id: Some("security".to_owned()), name: "security", class: "border border-border bg-background",
                        initial_value: if settings.security.is_empty() { "starttls".to_owned() } else { settings.security.clone() },
                        options: vec![
                            SelectOption::new("starttls", "STARTTLS (usually port 587)"),
                            SelectOption::new("tls", "TLS (usually port 465)"),
                            SelectOption::new("none", "None (usually port 25)"),
                        ] }
                }
                p { class: "identity-form-hint", "Leave the port blank to use the usual one for the encryption." }
            }
            fieldset { class: "identity-fieldset",
                legend { "Sign-in" }
                p { class: "identity-form-hint", "Leave both blank if the server needs no sign-in." }
                {text_field("username", "Username", settings.username.clone(), "text", false, 255)}
                div { class: "space-y-2",
                    label { r#for: "password", class: "block text-sm font-semibold", "Password" }
                    input { id: "password", name: "password", r#type: "password", autocomplete: "new-password", maxlength: "255",
                        disabled: !page.can_store_password,
                        placeholder: if settings.password_set { "Password set. Leave blank to keep it." } else { "" },
                        class: FIELD }
                    if !page.can_store_password {
                        p { class: "identity-form-hint", "Set SECRETS_MASTER_KEY on the server to save a password here." }
                    }
                }
            }
            fieldset { class: "identity-fieldset",
                legend { "Sender" }
                {text_field("from_name", "From name", settings.from_name.clone(), "text", false, 255)}
                {text_field("from_address", "From address", settings.from_address.clone(), "email", true, 255)}
                {text_field("reply_to", "Reply-To address (optional)", settings.reply_to.clone(), "email", false, 255)}
            }
            div { class: "identity-form-actions", button { r#type: "submit", class: SUBMIT, "Save" } }
        }
    }
}

pub fn render(page: Page) -> Element {
    let configured = page.settings.source != "none";
    admin_layout(
        Section::Email,
        "Email",
        "Send notifications and account notices by email through your own mail server.",
        rsx! {
            {notice(page.message.as_deref(), page.is_error)}
            section { class: "identity-admin-panel", aria_labelledby: "email-server-title",
                header { class: "identity-admin-panel-header",
                    div {
                        h2 { id: "email-server-title", "Mail server " {source_badge(&page.settings.source)} }
                        p { {source_note(&page.settings.source)} }
                        if page.public_url.is_empty() {
                            p { "Without PUBLIC_URL, emails contain codes but no links." }
                        } else {
                            p { class: "break-all", "Links in emails start with " code { "{page.public_url}" } ", from PUBLIC_URL." }
                        }
                    }
                }
                EmailForm { page: page.clone() }
            }
            if configured {
                section { class: "identity-admin-panel", aria_labelledby: "email-test-title",
                    header { class: "identity-admin-panel-header",
                        div {
                            h2 { id: "email-test-title", "Send a test email" }
                            p { "Sends a message to the address of your own account, so you see the server's answer." }
                        }
                    }
                    form { method: "post", action: "/admin/email/test",
                        {csrf(&page.csrf_token)}
                        button { r#type: "submit", class: "btn", "Send test email" }
                    }
                }
            }
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn page(source: &str, password_set: bool) -> Page {
        Page {
            csrf_token: "csrf-marker".into(),
            settings: Settings {
                source: source.into(),
                host: "mail.example.com".into(),
                port: "587".into(),
                security: "starttls".into(),
                username: "mailer".into(),
                password_set,
                from_name: "Codeyard".into(),
                from_address: "noreply@example.com".into(),
                reply_to: String::new(),
            },
            can_store_password: true,
            public_url: "https://git.example.com".into(),
            message: None,
            is_error: false,
        }
    }

    #[test]
    fn the_page_shows_the_link_address_or_says_mail_has_no_links() {
        let html = dioxus_ssr::render_element(render(page("database", false)));
        assert!(html.contains("https://git.example.com"), "{html}");
        assert!(!html.contains("no links"), "{html}");
        let mut unset = page("database", false);
        unset.public_url = String::new();
        let html = dioxus_ssr::render_element(render(unset));
        assert!(
            html.contains("Without PUBLIC_URL, emails contain codes but no links."),
            "{html}"
        );
    }

    #[test]
    fn the_form_shows_the_saved_values_and_both_actions() {
        let html = dioxus_ssr::render_element(render(page("database", true)));
        assert!(html.contains("aria-label=\"Administration\""), "{html}");
        assert!(html.contains("action=\"/admin/email\""), "{html}");
        assert!(html.contains("action=\"/admin/email/test\""), "{html}");
        assert!(html.contains("mail.example.com"), "{html}");
        assert!(html.contains("saved in the database"), "{html}");
        assert!(
            html.contains("Password set. Leave blank to keep it."),
            "{html}"
        );
        assert_eq!(html.matches("csrf-marker").count(), 2);
    }

    #[test]
    fn the_page_says_where_the_settings_come_from() {
        assert!(
            dioxus_ssr::render_element(render(page("environment", false)))
                .contains("CODEYARD_SMTP_*")
        );
        let none = dioxus_ssr::render_element(render(page("none", false)));
        assert!(none.contains("No mail server is set up"));
        assert!(none.contains("Not set up"));
        // Without a server there is nothing to test.
        assert!(!none.contains("action=\"/admin/email/test\""));
    }

    #[test]
    fn the_model_has_no_password_and_survives_hydration() {
        let model = page("database", true);
        let json = serde_json::to_string(&model).unwrap();
        assert!(!json.contains("\"password\""), "{json}");
        assert_eq!(serde_json::from_str::<Page>(&json).unwrap(), model);
    }
}
