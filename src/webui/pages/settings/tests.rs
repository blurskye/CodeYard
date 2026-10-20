//! Account settings contracts: a rejected form keeps its non-secret fields, and never its secrets.
use super::*;
use crate::webui::pages::fields::{preserved_values, FormValues};
use crate::webui::pages::users::Profile;
use dioxus::prelude::Element;
use std::collections::HashMap;

fn html(element: Element) -> String {
    dioxus_ssr::render_element(element)
}

#[test]
fn rejected_settings_preserve_nonsecret_fields_but_not_passwords_or_private_keys() {
    crate::webui::tests::in_scope(|| {
        let form = HashMap::from([
            ("display_name".into(), "Alice & Bob".into()),
            ("bio".into(), "</textarea><script>evil</script>".into()),
            ("website".into(), "https://example.org".into()),
            ("new_password".into(), "secret-new-marker".into()),
            (
                "public_key".into(),
                "-----BEGIN OPENSSH PRIVATE KEY-----secret-key-marker".into(),
            ),
        ]);
        let values = preserved_values(&form);
        assert!(values.get("public_key").is_empty());
        assert!(values.get("new_password").is_empty());
        let output = html(settings_page(
            Settings {
                two_factor: Default::default(),
                theme: crate::webui::document::Theme::System,
                profile: Profile::default(),
                email: String::new(),
                tab: "profile".into(),
                keys: vec![],
                tokens: vec![],
                sessions: vec![],
                new_token: None,
                password_supported: true,
                must_change_password: false,
                manage_access: false,
                requests: vec![],
                requests_count: 0,
                requests_page: 1,
                values,
            },
            "token",
            Some("Please correct your profile."),
            true,
        ));
        assert!(output.contains("Alice &#38; Bob"));
        assert!(output.contains("&lt;/textarea&gt;"));
        assert!(output.contains("https://example.org"));
        assert!(!output.contains("<script>"));
        assert!(!output.contains("secret-new-marker"));
        assert!(!output.contains("secret-key-marker"));
        assert!(!output.contains("Private node access"));
    });
}

#[test]
fn settings_primary_actions_share_one_class_and_deletes_are_outlined() {
    crate::webui::tests::in_scope(|| {
        let settings_for = |tab: &str, keys: Vec<Key>| Settings {
            two_factor: Default::default(),
            theme: crate::webui::document::Theme::System,
            profile: Profile {
                username: "alice".into(),
                ..Profile::default()
            },
            email: "alice@example.test".into(),
            tab: tab.into(),
            keys,
            tokens: vec![],
            sessions: vec![],
            new_token: None,
            password_supported: true,
            must_change_password: false,
            manage_access: false,
            requests: vec![],
            requests_count: 0,
            requests_page: 1,
            values: preserved_values(&HashMap::new()),
        };
        let profile = html(settings_page(
            settings_for("profile", vec![]),
            "token",
            None,
            false,
        ));
        assert!(profile.contains("btn-primary"));
        assert!(!profile.contains("bg-primary"));
        let keys = html(settings_page(
            settings_for(
                "keys",
                vec![Key {
                    id: 7,
                    name: "Laptop".into(),
                    fingerprint: "SHA256:public".into(),
                    kind: "ed25519".into(),
                    created: "2026-10-02".into(),
                }],
            ),
            "token",
            None,
            false,
        ));
        assert!(keys.contains("btn btn-danger btn-sm"));
        assert!(!keys.contains("identity-danger-link"));
    });
}

#[test]
fn ssh_key_and_token_deletion_ask_in_a_centred_dialog() {
    crate::webui::tests::in_scope(|| {
        let settings_for = |tab: &str| Settings {
            two_factor: Default::default(),
            theme: crate::webui::document::Theme::System,
            profile: Profile {
                username: "alice".into(),
                ..Profile::default()
            },
            email: "alice@example.test".into(),
            tab: tab.into(),
            keys: vec![Key {
                id: 7,
                name: "Laptop".into(),
                fingerprint: "SHA256:public".into(),
                kind: "ed25519".into(),
                created: "2026-10-02".into(),
            }],
            tokens: vec![Token {
                id: 4,
                name: "ci".into(),
                scope: "read".into(),
                expires: "2027-01-01".into(),
                created: "2026-10-02".into(),
            }],
            sessions: vec![],
            new_token: None,
            password_supported: true,
            must_change_password: false,
            manage_access: false,
            requests: vec![],
            requests_count: 0,
            requests_page: 1,
            values: preserved_values(&HashMap::new()),
        };
        let keys = html(settings_page(settings_for("keys"), "token", None, false));
        assert!(
            keys.contains("aria-label=\"Delete SSH key Laptop\""),
            "{keys}"
        );
        assert!(
            keys.contains("action=\"/settings/keys/7/delete\""),
            "{keys}"
        );
        assert!(keys.contains("href=\"/settings?tab=keys\""), "{keys}");
        assert!(keys.contains(">Cancel</a>"), "{keys}");
        assert!(!keys.contains("class=\"collaboration-checkbox\""), "{keys}");
        let tokens = html(settings_page(settings_for("tokens"), "token", None, false));
        assert!(
            tokens.contains("aria-label=\"Delete token ci\""),
            "{tokens}"
        );
        assert!(
            tokens.contains("action=\"/settings/tokens/4/delete\""),
            "{tokens}"
        );
        assert!(tokens.contains("href=\"/settings?tab=tokens\""), "{tokens}");
        assert!(
            !tokens.contains("class=\"collaboration-checkbox\""),
            "{tokens}"
        );
    });
}

#[test]
fn settings_use_a_current_left_nav_and_keep_native_credential_forms() {
    crate::webui::tests::in_scope(|| {
        let output = html(settings_page(
            Settings {
                two_factor: Default::default(),
                theme: crate::webui::document::Theme::System,
                profile: Profile {
                    username: "alice".into(),
                    ..Profile::default()
                },
                email: "alice@example.test".into(),
                tab: "keys".into(),
                keys: vec![],
                tokens: vec![],
                sessions: vec![],
                new_token: None,
                password_supported: true,
                must_change_password: false,
                manage_access: true,
                requests: vec![],
                requests_count: 0,
                requests_page: 1,
                values: FormValues::default(),
            },
            "settings-csrf",
            None,
            false,
        ));
        assert!(output.contains("class=\"identity-settings-layout\""));
        assert!(output.contains("identity-settings-nav-current"));
        assert!(output.contains("SSH keys, current page"));
        assert!(output.contains("action=\"/settings/keys\""));
        assert!(output.contains("name=\"public_key\""));
        assert!(output.contains("value=\"settings-csrf\""));
    });
}

#[test]
fn prerendered_bio_and_public_key_keep_native_escaped_text() {
    crate::webui::tests::in_scope(|| {
        let source = "\n</textarea><script>bad</script>&";
        for (tab, name) in [("profile", "bio"), ("keys", "public_key")] {
            let model = Settings {
                two_factor: Default::default(),
                theme: crate::webui::document::Theme::System,
                profile: Profile::default(),
                email: String::new(),
                tab: tab.into(),
                keys: vec![],
                tokens: vec![],
                sessions: vec![],
                new_token: None,
                password_supported: false,
                must_change_password: false,
                manage_access: false,
                requests: vec![],
                requests_count: 0,
                requests_page: 1,
                values: FormValues {
                    bio: source.into(),
                    public_key: source.into(),
                    ..Default::default()
                },
            };
            let mut renderer = dioxus_ssr::Renderer::new();
            renderer.pre_render = true;
            let html = renderer.render_element(settings_page(model, "csrf-fixture", None, false));
            let textarea = html.split_once("<textarea").unwrap().1;
            assert!(textarea
                .split_once('>')
                .unwrap()
                .0
                .contains(&format!("name=\"{name}\"")));
            let text = textarea
                .split_once('>')
                .unwrap()
                .1
                .split_once("</textarea>")
                .unwrap()
                .0;
            assert_eq!(
                text,
                "\n\n&lt;/textarea&gt;&lt;script&gt;bad&lt;/script&gt;&amp;"
            );
            assert!(!text.contains("<!--"));
            assert!(html.contains("name=\"csrf_token\""));
        }
    });
}

#[test]
fn token_expiry_defaults_to_thirty_days_and_takes_a_calendar_day() {
    fn radio<'a>(output: &'a str, value: &str) -> &'a str {
        let start = output
            .find(&format!("value=\"{value}\""))
            .expect("radio is rendered");
        let rest = &output[start..];
        &rest[..rest.find('>').expect("tag closes")]
    }
    crate::webui::tests::in_scope(|| {
        let tokens_page = |values: FormValues| {
            html(settings_page(
                Settings {
                    two_factor: Default::default(),
                    theme: crate::webui::document::Theme::System,
                    profile: Profile {
                        username: "alice".into(),
                        ..Profile::default()
                    },
                    email: "alice@example.test".into(),
                    tab: "tokens".into(),
                    keys: vec![],
                    tokens: vec![],
                    sessions: vec![],
                    new_token: None,
                    password_supported: true,
                    must_change_password: false,
                    manage_access: false,
                    requests: vec![],
                    requests_count: 0,
                    requests_page: 1,
                    values,
                },
                "token",
                None,
                false,
            ))
        };
        let fresh = tokens_page(FormValues::default());
        assert!(!radio(&fresh, "never").contains("checked"));
        assert!(radio(&fresh, "date").contains("checked"));
        assert!(fresh.contains("type=\"date\""));
        assert!(fresh.contains("name=\"expires_at\""));
        let thirty_days = (crate::webui::components::current_time() + chrono::Duration::days(30))
            .format("%Y-%m-%d")
            .to_string();
        assert!(fresh.contains(&format!("value=\"{thirty_days}\"")));
        assert!(!fresh.contains("RFC 3339"));

        let chosen = tokens_page(FormValues {
            expiry: "date".into(),
            expires_at: "2027-01-15".into(),
            ..FormValues::default()
        });
        assert!(radio(&chosen, "date").contains("checked"));
        assert!(!radio(&chosen, "never").contains("checked"));
        assert!(chosen.contains("value=\"2027-01-15\""));
    });
}

/// The whole form that holds the hidden `theme` input with this value.
fn appearance_form<'a>(output: &'a str, theme: &str) -> &'a str {
    let at = output
        .find(&format!("value=\"{theme}\""))
        .expect("theme input is rendered");
    let start = output[..at].rfind("<form").expect("input sits in a form");
    let end = at + output[at..].find("</form>").expect("form closes") + "</form>".len();
    &output[start..end]
}

#[test]
fn appearance_tab_sits_after_the_profile_and_marks_the_saved_theme() {
    crate::webui::tests::in_scope(|| {
        let page = |theme: crate::webui::document::Theme| {
            html(settings_page(
                Settings {
                    two_factor: Default::default(),
                    profile: Profile {
                        username: "alice".into(),
                        ..Profile::default()
                    },
                    email: "alice@example.test".into(),
                    tab: "appearance".into(),
                    keys: vec![],
                    tokens: vec![],
                    sessions: vec![],
                    new_token: None,
                    password_supported: true,
                    must_change_password: false,
                    manage_access: false,
                    requests: vec![],
                    requests_count: 0,
                    requests_page: 1,
                    values: preserved_values(&HashMap::new()),
                    theme,
                },
                "token",
                None,
                false,
            ))
        };
        let output = page(crate::webui::document::Theme::Dark);
        let profile = output
            .find("href=\"/settings?tab=profile\"")
            .expect("profile link");
        let appearance = output
            .find("href=\"/settings?tab=appearance\"")
            .expect("appearance link");
        let keys = output
            .find("href=\"/settings?tab=keys\"")
            .expect("keys link");
        assert!(profile < appearance && appearance < keys);
        assert!(output.contains(">Appearance<"));
        assert!(output.contains("Choose how Codeyard looks on this device."));
        assert_eq!(
            output.matches("class=\"identity-appearance-row\"").count(),
            3
        );
        for (value, label) in [("system", "System"), ("light", "Light"), ("dark", "Dark")] {
            let form = appearance_form(&output, value);
            assert!(form.contains("action=\"/_forms/theme\""));
            assert!(form.contains("value=\"/settings?tab=appearance\""));
            assert!(form.contains("name=\"csrf_token\""));
            assert!(form.contains(&format!("Use {label}")));
        }
        let saved = appearance_form(&output, "dark");
        assert!(saved.contains("aria-current=\"true\""));
        assert!(saved.contains("count-badge"));
        for value in ["system", "light"] {
            let form = appearance_form(&output, value);
            assert!(form.contains("aria-current=\"false\""));
            assert!(!form.contains("count-badge"));
        }
    });
}

#[test]
fn picture_preview_shows_the_account_picture_or_the_uploaded_file_at_72_pixels() {
    crate::webui::tests::in_scope(|| {
        let page = |avatar_url: Option<&str>| {
            html(settings_page(
                Settings {
                    two_factor: Default::default(),
                    profile: Profile {
                        username: "alice".into(),
                        avatar_url: avatar_url.map(str::to_owned),
                        ..Profile::default()
                    },
                    email: "alice@example.test".into(),
                    tab: "profile".into(),
                    keys: vec![],
                    tokens: vec![],
                    sessions: vec![],
                    new_token: None,
                    password_supported: true,
                    must_change_password: false,
                    manage_access: false,
                    requests: vec![],
                    requests_count: 0,
                    requests_page: 1,
                    values: preserved_values(&HashMap::new()),
                    theme: crate::webui::document::Theme::System,
                },
                "settings-csrf",
                None,
                false,
            ))
        };
        let generated = page(None);
        assert!(generated.contains("src=\"/avatars/alice\""));
        assert!(generated.contains("width=\"72\""));
        // An uploaded picture is linked by its own file, so a new upload shows at once.
        let uploaded = page(Some("/api/files/7/download"));
        assert!(uploaded.contains("src=\"/api/files/7/download\""));
        assert!(uploaded.contains("class=\"identity-settings-avatar\""));
    });
}

fn settings_for(tab: &str, sessions: Vec<AccountSession>) -> Settings {
    Settings {
        two_factor: Default::default(),
        theme: crate::webui::document::Theme::System,
        profile: Profile::default(),
        email: "alice@example.test".into(),
        tab: tab.into(),
        keys: vec![],
        tokens: vec![],
        sessions,
        new_token: None,
        password_supported: true,
        must_change_password: false,
        manage_access: false,
        requests: vec![],
        requests_count: 0,
        requests_page: 1,
        values: FormValues::default(),
    }
}

#[test]
fn email_tab_asks_for_the_current_password_and_posts_to_the_email_route() {
    crate::webui::tests::in_scope(|| {
        let output = html(settings_page(
            settings_for("email", vec![]),
            "token",
            None,
            false,
        ));
        assert!(output.contains("action=\"/settings/email\""));
        assert!(output.contains("name=\"current_password\""));
        assert!(output.contains("value=\"alice@example.test\""));
        assert!(!output.contains("name=\"new_password\""));
        assert!(output.contains("href=\"/settings?tab=email\""));
    });
}

#[test]
fn sessions_tab_marks_this_session_and_never_shows_a_session_secret() {
    crate::webui::tests::in_scope(|| {
        let sessions = vec![
            AccountSession {
                key: "digest-current".into(),
                current: true,
                created: "Oct 1, 2026 10:00 UTC".into(),
                expires: "Oct 8, 2026".into(),
                user_agent: "Firefox on Linux".into(),
                ip_address: "203.0.113.4".into(),
            },
            AccountSession {
                key: "digest-other".into(),
                current: false,
                created: "Oct 2, 2026 09:00 UTC".into(),
                expires: "Oct 9, 2026".into(),
                user_agent: String::new(),
                ip_address: String::new(),
            },
        ];
        let output = html(settings_page(
            settings_for("sessions", sessions),
            "token",
            None,
            false,
        ));
        assert!(output.contains("This session"));
        assert!(output.contains("name=\"session\" value=\"digest-other\""));
        assert!(output.contains("action=\"/settings/sessions/revoke\""));
        assert!(output.contains("action=\"/settings/sessions/revoke-others\""));
        assert!(output.contains("Unknown browser"));
        assert!(output.contains("IP unknown"));
    });
}

#[test]
fn sessions_tab_hides_sign_out_everywhere_else_when_no_other_session_exists() {
    crate::webui::tests::in_scope(|| {
        let sessions = vec![AccountSession {
            key: "digest-current".into(),
            current: true,
            created: "Oct 1, 2026 10:00 UTC".into(),
            expires: "Oct 8, 2026".into(),
            user_agent: String::new(),
            ip_address: String::new(),
        }];
        let output = html(settings_page(
            settings_for("sessions", sessions),
            "token",
            None,
            false,
        ));
        assert!(!output.contains("/settings/sessions/revoke-others"));
    });
}
