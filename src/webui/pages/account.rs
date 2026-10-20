//! Account pages: sign-in and sign-out, account settings, administration and profiles.
//! One `Page` covers them, and the app renders it through `render`.
use crate::webui::pages::{
    admin::{admin_page, admin_users_page, Admin, AdminUsers},
    auth::{auth_page, logout_page, password_help_page, two_factor_page, AuthMode},
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
        Page::PasswordHelp { redirect } => password_help_page(&redirect),
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
        Page::Admin {
            model,
            csrf_token,
            message,
            is_error,
        } => admin_page(model, &csrf_token, message.as_deref(), is_error),
        Page::AdminUsers {
            model,
            csrf_token,
            message,
            is_error,
        } => admin_users_page(model, &csrf_token, message.as_deref(), is_error),
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
        admin::{Account, Activity, AdminUsers, Signup},
        fields::preserved_values,
        settings::Key,
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
        for (tab, label) in [
            ("profile", "Save profile"),
            ("keys", "Add SSH key"),
            ("tokens", "Create token"),
            ("password", "Confirm new password"),
            ("access", "Create managed account"),
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
                        must_change_password: true,
                        manage_access: true,
                        requests: vec![Signup {
                            id: 8,
                            username: "bob".into(),
                            email: "bob@example.test".into(),
                            created: "2026-10-02".into(),
                        }],
                        requests_count: 1,
                        requests_page: 1,
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
                    current_user_id: 1,
                    totals: (1, 0, 1024),
                    ai_crawlers: "block".into(),
                    accounts: vec![Account {
                        two_factor: false,
                        id: 2,
                        username: "bob".into(),
                        email: "bob@example.test".into(),
                        admin: false,
                        banned: false,
                        must_change_password: true,
                        files: 3,
                        bytes: 1024,
                        commits: 4,
                        pushes: 2,
                        contributions: 6,
                        created: "2026-10-02".into(),
                    }],
                    activity: vec![Activity {
                        date: "2026-10-02".into(),
                        contributions: 6,
                        cumulative_contributions: 6,
                        bytes: 1024,
                        cumulative_bytes: 1024,
                    }],
                    days: 7,
                    manage_access: true,
                    requests: vec![],
                    pending_requests: 0,
                    requests_count: 0,
                    requests_page: 1,
                    values,
                },
                csrf_token: "csrf-marker".into(),
                message: Some("<b>Validation error</b>".into()),
                is_error: true,
            },
            "Manage users",
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
                },
                csrf_token: "csrf-marker".into(),
                message: Some("<b>Validation error</b>".into()),
                is_error: true,
            },
            "Users",
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
                assert!(output.contains("csrf-marker"));
                assert!(!output.contains("<script>"));
                assert!(!output.contains("<b>Validation error</b>"));
                assert!(!output.contains("secret-"));
            }
        });
    }
}
