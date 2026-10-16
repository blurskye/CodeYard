use super::administration::{activity_totals, columns, AdminTimeframe};
use super::auth::password_strength;
use super::profile::profile_repository_rows;
use super::*;

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
                    values: values.clone(),
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
                ai_crawlers: "block".into(),
                accounts: vec![Account {
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
                values,
            },
            csrf_token: "csrf-marker".into(),
            message: Some("<b>Validation error</b>".into()),
            is_error: true,
        },
        "All accounts",
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
            csrf_token: "csrf-marker".into(),
            own: false,
            authenticated: true,
            following: true,
            followers: 7,
            follows: 2,
            message: Some("<b>Validation error</b>".into()),
            activity: Default::default(),
        },
        "Repositories (1)",
    ));
    pages
}

mod administration;
mod authentication;
mod profile;
mod schema;
mod settings;
