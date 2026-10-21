//! Account pages: sign-in and sign-out, account settings, administration and profiles.
//! One `Page` covers them, and the app renders it through `render`.
use crate::webui::pages::{
    admin::{
        admin_audit_page, admin_page, admin_settings_page, admin_users_page, Admin, AdminAudit,
        AdminSettings, AdminUsers,
    },
    auth::{
        auth_page, logout_page, password_help_page, reset_code_page, reset_password_page,
        two_factor_page, verify_email_page, AuthMode,
    },
    fields::FormValues,
    settings::{settings_page, Settings},
    users::{profile_page, Profile, ProfileActivity, ProfileStars, Repository},
};
use dioxus::prelude::*;

/// Authorized presentation state shared by initial SSR and client navigation.
/// Booleans only describe visible controls; backend POSTs recheck all authority.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum Page {
    Auth {
        mode: AuthMode,
        action: String,
        csrf_token: String,
        values: FormValues,
        private: bool,
        /// Ids of the configured OAuth providers, such as `github`, in display order.
        #[serde(default)]
        providers: Vec<String>,
        redirect: String,
        message: Option<String>,
        is_error: bool,
    },
    Logout {
        csrf_token: String,
    },
    PasswordHelp {
        redirect: String,
        csrf_token: String,
        /// Whether the server can send the reset link.
        mail_available: bool,
        message: Option<String>,
        is_error: bool,
    },
    /// The step after asking for a reset: the mailed code and a new password.
    ResetCode {
        csrf_token: String,
        email: String,
        message: Option<String>,
        is_error: bool,
    },
    ResetPassword {
        csrf_token: String,
        token: String,
        /// Whether the link still works, which shows the form.
        valid: bool,
        message: Option<String>,
        is_error: bool,
    },
    VerifyEmail {
        csrf_token: String,
        token: String,
        valid: bool,
        message: Option<String>,
        is_error: bool,
    },
    TwoFactor {
        action: String,
        csrf_token: String,
        pending: String,
        redirect: String,
        message: Option<String>,
        is_error: bool,
    },
    Settings {
        model: Box<Settings>,
        csrf_token: String,
        message: Option<String>,
        is_error: bool,
    },
    Admin {
        model: Admin,
    },
    AdminSettings {
        model: AdminSettings,
        csrf_token: String,
        message: Option<String>,
        is_error: bool,
    },
    AdminUsers {
        model: AdminUsers,
        csrf_token: String,
        message: Option<String>,
        is_error: bool,
    },
    AdminAudit {
        model: AdminAudit,
    },
    Profile {
        profile: Profile,
        repositories: Vec<Repository>,
        repository_count: usize,
        repositories_newer: Option<String>,
        repositories_older: Option<String>,
        csrf_token: String,
        own: bool,
        authenticated: bool,
        following: bool,
        followers: i64,
        follows: i64,
        message: Option<String>,
        activity: ProfileActivity,
        stars: ProfileStars,
    },
}

pub fn render(page: Page) -> Element {
    match page {
        Page::Auth {
            mode,
            action,
            csrf_token,
            values,
            private,
            providers,
            redirect,
            message,
            is_error,
        } => auth_page(
            mode,
            &action,
            &csrf_token,
            values,
            private,
            providers,
            &redirect,
            message.as_deref(),
            is_error,
        ),
        Page::Logout { csrf_token } => logout_page(&csrf_token),
        Page::PasswordHelp {
            redirect,
            csrf_token,
            mail_available,
            message,
            is_error,
        } => password_help_page(
            &redirect,
            &csrf_token,
            mail_available,
            message.as_deref(),
            is_error,
        ),
        Page::ResetCode {
            csrf_token,
            email,
            message,
            is_error,
        } => reset_code_page(&csrf_token, &email, message.as_deref(), is_error),
        Page::ResetPassword {
            csrf_token,
            token,
            valid,
            message,
            is_error,
        } => reset_password_page(&csrf_token, &token, valid, message.as_deref(), is_error),
        Page::VerifyEmail {
            csrf_token,
            token,
            valid,
            message,
            is_error,
        } => verify_email_page(&csrf_token, &token, valid, message.as_deref(), is_error),
        Page::TwoFactor {
            action,
            csrf_token,
            pending,
            redirect,
            message,
            is_error,
        } => two_factor_page(
            &action,
            &csrf_token,
            &pending,
            &redirect,
            message.as_deref(),
            is_error,
        ),
        Page::Settings {
            model,
            csrf_token,
            message,
            is_error,
        } => settings_page(*model, &csrf_token, message.as_deref(), is_error),
        Page::Admin { model } => admin_page(model),
        Page::AdminSettings {
            model,
            csrf_token,
            message,
            is_error,
        } => admin_settings_page(model, &csrf_token, message.as_deref(), is_error),
        Page::AdminUsers {
            model,
            csrf_token,
            message,
            is_error,
        } => admin_users_page(model, &csrf_token, message.as_deref(), is_error),
        Page::AdminAudit { model } => admin_audit_page(model),
        Page::Profile {
            profile,
            repositories,
            repository_count,
            repositories_newer,
            repositories_older,
            csrf_token,
            own,
            authenticated,
            following,
            followers,
            follows,
            message,
            activity,
            stars,
        } => profile_page(
            profile,
            repositories,
            repository_count,
            activity,
            stars,
            &csrf_token,
            own,
            authenticated,
            following,
            followers,
            follows,
            message.as_deref(),
            repositories_newer,
            repositories_older,
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::webui::pages::{
        admin::{
            Account, Activity, AdminAudit, AdminSettings, AdminUsers, AuditEntry, Signup,
            SystemStatus,
        },
        fields::preserved_values,
        settings::{EmailVerification, Key},
    };
    use dioxus::prelude::Element;
    use std::collections::HashMap;

    fn html(element: Element) -> String {
        dioxus_ssr::render_element(element)
    }

    fn safe_pages() -> Vec<(Page, &'static str)> {
        let fields = HashMap::from([
            ("username".into(), "alice".into()),
            ("bio".into(), "<script>untrusted biography</script>".into()),
            ("password".into(), "secret-password-marker".into()),
            ("current_password".into(), "secret-current-marker".into()),
            ("new_password".into(), "secret-new-marker".into()),
            ("confirm_password".into(), "secret-confirm-marker".into()),
            ("admin_secret".into(), "secret-admin-marker".into()),
            (
                "public_key".into(),
                "-----BEGIN OPENSSH PRIVATE KEY-----secret-key-marker".into(),
            ),
        ]);
        let values = preserved_values(&fields);
        let profile = Profile {
            username: "alice".into(),
            bio: values.bio.clone(),
            ..Profile::default()
        };
        let mut pages = vec![];
        for (mode, action) in [
            (AuthMode::Login, "/login"),
            (AuthMode::Register, "/register"),
            (AuthMode::Setup, "/setup"),
        ] {
            pages.push((
                Page::Auth {
                    mode,
                    action: action.into(),
                    csrf_token: "csrf-marker".into(),
                    values: values.clone(),
                    private: true,
                    providers: vec!["github".into(), "gitlab".into()],
                    redirect: "/alice/project".into(),
                    message: Some("<b>Validation error</b>".into()),
                    is_error: true,
                },
                mode.title(),
            ));
        }
        pages.push((
            Page::Logout {
                csrf_token: "csrf-marker".into(),
            },
            "Sign out",
        ));
        pages.push((
            Page::ResetCode {
                csrf_token: "csrf-marker".into(),
                email: "alice@example.test".into(),
                message: Some("<b>Validation error</b>".into()),
                is_error: true,
            },
            "Check your email",
        ));
        for (tab, label) in [
            ("profile", "Save profile"),
            ("keys", "Add SSH key"),
            ("tokens", "Create token"),
            ("password", "Confirm new password"),
        ] {
            pages.push((
                Page::Settings {
                    model: Box::new(Settings {
                        theme: crate::webui::document::Theme::System,
                        profile: profile.clone(),
                        email: "alice@example.test".into(),
                        tab: tab.into(),
                        keys: vec![Key {
                            id: 7,
                            name: "Laptop".into(),
                            fingerprint: "SHA256:public".into(),
                            kind: "ed25519".into(),
                            created: "2026-10-02".into(),
                        }],
                        tokens: vec![],
                        sessions: vec![],
                        new_token: None,
                        password_supported: true,
                        email_notifications: true,
                        email_verification: EmailVerification::Verified,
                        must_change_password: true,
                        values: values.clone(),
                        two_factor: Default::default(),
                    }),
                    csrf_token: "csrf-marker".into(),
                    message: Some("<b>Validation error</b>".into()),
                    is_error: true,
                },
                label,
            ));
        }
        pages.push((
            Page::Admin {
                model: Admin {
                    accounts: vec![Account {
                        id: 2,
                        username: "bob".into(),
                        created: "2026-10-02".into(),
                    }],
                    users: 1,
                    banned: 0,
                    repositories: 2,
                    organizations: 1,
                    pending_requests: Some(1),
                    storage_bytes: 1024,
                    activity: vec![Activity {
                        date: "2026-10-02".into(),
                        contributions: 6,
                        cumulative_contributions: 6,
                        bytes: 1024,
                        cumulative_bytes: 1024,
                    }],
                    days: 7,
                    status: SystemStatus {
                        version: "0.1.2".into(),
                        database: "SQLite".into(),
                        private: true,
                        nodes: None,
                        queued_runs: 0,
                    },
                },
            },
            "System status",
        ));
        pages.push((
            Page::AdminSettings {
                model: AdminSettings {
                    private: true,
                    ai_crawlers: "block".into(),
                },
                csrf_token: "csrf-marker".into(),
                message: Some("<b>Validation error</b>".into()),
                is_error: true,
            },
            "AI crawler policy",
        ));
        pages.push((
            Page::AdminUsers {
                model: AdminUsers {
                    managed_accounts: true,
                    current_user_id: 1,
                    accounts: vec![crate::webui::pages::admin::AccountMetadata {
                        two_factor: false,
                        id: 2,
                        username: "bob".into(),
                        email: "bob@example.test".into(),
                        admin: false,
                        banned: true,
                        must_change_password: true,
                        created: "2026-10-02".into(),
                    }],
                    search: "bo".into(),
                    page: 1,
                    pages: 2,
                    total: 30,
                    values: FormValues::default(),
                    requests: vec![Signup {
                        id: 8,
                        username: "carol".into(),
                        email: "carol@example.test".into(),
                        created: "2026-10-02".into(),
                    }],
                    requests_count: 1,
                    requests_page: 1,
                },
                csrf_token: "csrf-marker".into(),
                message: Some("<b>Validation error</b>".into()),
                is_error: true,
            },
            "Users",
        ));
        pages.push((
            Page::AdminAudit {
                model: AdminAudit {
                    entries: vec![AuditEntry {
                        id: 1,
                        actor: "alice".into(),
                        action: "user.ban".into(),
                        target: "user 2".into(),
                        details: "{}".into(),
                        time: "2026-10-02 09:30 UTC".into(),
                    }],
                    action: String::new(),
                    actor: String::new(),
                    page: 1,
                    pages: 1,
                    total: 1,
                },
            },
            "Audit log",
        ));
        pages.push((
            Page::Profile {
                profile,
                repositories: vec![Repository {
                    name: "project".into(),
                    description: "<script>description</script>".into(),
                    private: false,
                    archived: false,
                    stars: 3,
                    forks: 2,
                    updated: "2026-10-02".into(),
                }],
                repository_count: 1,
                repositories_newer: None,
                repositories_older: None,
                csrf_token: "csrf-marker".into(),
                own: false,
                authenticated: true,
                following: true,
                followers: 7,
                follows: 2,
                message: Some("<b>Validation error</b>".into()),
                activity: Default::default(),
                stars: Default::default(),
            },
            "Repositories (1)",
        ));
        pages
    }

    #[test]
    fn every_page_variant_roundtrips_safe_owned_json_without_secrets() {
        crate::webui::tests::in_scope(|| {
            for (page, _) in safe_pages() {
                let json = serde_json::to_string(&page).expect("safe identity page serializes");
                assert!(!json.contains("secret-"), "secret survived in {json}");
                for key in [
                    "\"password\":",
                    "\"password_hash\":",
                    "\"current_password\":",
                    "\"new_password\":",
                    "\"confirm_password\":",
                    "\"admin_secret\":",
                ] {
                    assert!(!json.contains(key), "unexpected secret field {key}");
                }
                let decoded: Page =
                    serde_json::from_str(&json).expect("identity page deserializes");
                assert_eq!(page, decoded);
                assert_eq!(html(render(page)), html(render(decoded)));
            }
        });
    }

    #[test]
    fn recovery_schema_rejects_accidental_password_fields() {
        let mut value = serde_json::to_value(FormValues::default()).unwrap();
        value.as_object_mut().unwrap().insert(
            "password".into(),
            serde_json::json!("secret-password-marker"),
        );
        assert!(serde_json::from_value::<FormValues>(value).is_err());
    }

    #[test]
    fn dispatch_covers_auth_settings_admin_logout_and_profile_with_escaped_recovery() {
        crate::webui::tests::in_scope(|| {
            for (page, label) in safe_pages() {
                let output = html(render(page));
                assert!(output.contains(label), "missing {label} in {output}");
                // The admin overview and the audit log only read, so they have no forms.
                assert!(
                    output.contains("csrf-marker")
                        || label == "System status"
                        || label == "Audit log"
                );
                assert!(!output.contains("<script>"));
                assert!(!output.contains("<b>Validation error</b>"));
                assert!(!output.contains("secret-"));
            }
        });
    }
}
