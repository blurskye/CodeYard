//! Auth page contracts: escaped sign-in forms, secrets that are never repopulated, and the password field.
use super::page::password_strength;
use super::*;
use crate::webui::pages::account::Page;
use crate::webui::pages::fields::{preserved_values, FormValues};
use crate::webui::pages::settings::{settings_page, Settings};
use crate::webui::pages::users::Profile;
use dioxus::prelude::Element;
use std::collections::HashMap;

fn html(element: Element) -> String {
    dioxus_ssr::render_element(element)
}

#[test]
fn auth_forms_escape_attributes_and_never_repopulate_secrets() {
    let form = HashMap::from([
        ("username".into(), "\"><script>alert(1)</script>".into()),
        ("password".into(), "secret-password-marker".into()),
        ("admin_secret".into(), "secret-admin-marker".into()),
        ("confirm_password".into(), "secret-confirm-marker".into()),
    ]);
    let output = crate::webui::tests::in_scope(|| {
        html(auth_page(
            AuthMode::Setup,
            "/setup",
            "csrf-marker",
            preserved_values(&form),
            false,
            Vec::new(),
            "/",
            Some("<b>invalid</b>"),
            true,
        ))
    });
    assert!(!output.contains("<script>"));
    assert!(output.contains("&#60;script&#62;"));
    assert!(output.contains("&#60;b&#62;invalid&#60;/b&#62;"));
    assert!(!output.contains("secret-password-marker"));
    assert!(!output.contains("secret-admin-marker"));
    assert!(!output.contains("secret-confirm-marker"));
    assert!(output.contains("name=\"csrf_token\""));
    assert!(output.contains("method=\"post\""));
    assert!(output.contains("action=\"/setup\""));
}

#[test]
fn login_has_compact_brand_heading_labeled_native_form_and_quiet_signup() {
    let output = html(auth_page(
        AuthMode::Login,
        "/login",
        "login-csrf",
        FormValues {
            username: "alice".into(),
            ..FormValues::default()
        },
        false,
        Vec::new(),
        "/alice/project?tab=code&ref=main",
        Some("Invalid username or password."),
        true,
    ));
    let brand = output.find("class=\"identity-auth-brand\"").unwrap();
    let heading = output.find("id=\"identity-auth-title\"").unwrap();
    let form = output.find("<form").unwrap();
    let form_end = output.find("</form>").unwrap();
    let switch = output.find("class=\"identity-auth-switch\"").unwrap();
    assert!(brand < heading && heading < form && form_end < switch);
    assert!(output.contains("aria-labelledby=\"identity-auth-title\""));
    assert!(output.contains("Sign in to Codeyard"));
    assert!(output.contains("src=\"/assets/logo.webp\""));
    assert!(output.contains("width=\"512\""));
    assert!(output.contains("height=\"512\""));
    assert!(output.contains("class=\"identity-auth-form\""));
    assert!(output.contains("class=\"identity-auth-submit\""));
    assert!(output.contains("method=\"post\""));
    assert!(output.contains("action=\"/login\""));
    assert!(output.contains("name=\"csrf_token\""));
    assert!(output.contains("value=\"login-csrf\""));
    assert!(output.contains("name=\"redirect\""));
    assert!(output.contains("value=\"signin\""));
    assert!(output.contains("for=\"username\""));
    assert!(output.contains("Username or email"));
    assert!(output.contains("value=\"alice\""));
    assert!(output.contains("autocomplete=\"username\""));
    assert!(output.contains("maxlength=\"510\""));
    assert!(output.contains("for=\"password\""));
    assert!(output.contains("autocomplete=\"current-password\""));
    assert!(output.contains("maxlength=\"4096\""));
    assert!(output.contains("role=\"status\""));
    assert!(output.contains("identity-auth-notice-error"));
    assert!(output.contains("Invalid username or password."));
    assert!(
        output.contains("href=\"/register?redirect=%2Falice%2Fproject%3Ftab%3Dcode%26ref%3Dmain\"")
    );
    assert!(output.contains("Create an account"));
    assert!(!output.contains("name=\"email\""));
    assert!(!output.contains("name=\"confirm_password\""));
    assert!(!output.contains("name=\"admin_secret\""));
    for rejected in [
        "backdrop-blur",
        "blur-[",
        "shadow-2xl",
        "rounded-[2rem]",
        "min-h-[",
        "Git hosting, written in Rust.",
        "Or continue with",
    ] {
        assert!(
            !output.contains(rejected),
            "rejected auth treatment: {rejected}"
        );
    }
}

#[test]
fn signup_and_setup_retain_private_approval_and_genuine_conditional_fields() {
    for private in [false, true] {
        let output = crate::webui::tests::in_scope(|| {
            html(auth_page(
                AuthMode::Register,
                "/register",
                "signup-csrf",
                FormValues {
                    email: "alice@example.test".into(),
                    ..FormValues::default()
                },
                private,
                Vec::new(),
                "/alice/project",
                None,
                false,
            ))
        });
        assert!(output.contains("Create your account"));
        assert!(output.contains("action=\"/register\""));
        assert!(output.contains("value=\"signup\""));
        assert!(output.contains("for=\"username\""));
        assert!(!output.contains("Username or email"));
        assert!(output.contains("for=\"email\""));
        assert!(output.contains("Email (optional)"));
        assert!(output.contains("value=\"alice@example.test\""));
        assert!(output.contains("autocomplete=\"email\""));
        assert!(output.contains("maxlength=\"320\""));
        assert!(output.contains("for=\"confirm_password\""));
        assert!(output.contains("autocomplete=\"new-password\""));
        assert!(output.contains("Use at least 8 characters."));
        assert!(output.contains("href=\"/login?redirect=%2Falice%2Fproject\""));
        assert_eq!(
            output.contains("An administrator must approve your signup request."),
            private
        );
        assert_eq!(output.contains("Request access"), private);
        assert!(!output.contains("name=\"node_mode\""));
        assert!(!output.contains("name=\"admin_secret\""));
    }
    let output = crate::webui::tests::in_scope(|| {
        html(auth_page(
            AuthMode::Setup,
            "/setup",
            "setup-csrf",
            FormValues {
                node_mode: "private".into(),
                ..FormValues::default()
            },
            false,
            vec!["github".into(), "google".into()],
            "/",
            None,
            false,
        ))
    });
    assert!(output.contains("Set up your node"));
    assert!(output.contains("Setup is only available before any accounts exist."));
    assert!(output.contains("action=\"/setup\""));
    assert!(output.contains("Complete setup"));
    assert!(output.contains("for=\"node_mode\""));
    assert!(output.contains("Private — approved accounts only"));
    let private_option = output
        .split_once("<option value=\"private\"")
        .unwrap()
        .1
        .split_once('>')
        .unwrap()
        .0;
    assert!(private_option.contains("selected"));
    assert!(output.contains("for=\"admin_secret\""));
    assert!(output.contains("Admin setup secret (if configured)"));
    assert!(output.contains("autocomplete=\"off\""));
    assert!(!output.contains("identity-auth-switch\""));
    assert!(!output.contains("<details"));
}

#[test]
fn provider_controls_render_one_link_per_configured_provider() {
    const LINKS: [(&str, &str); 5] = [
        ("github", "Continue with GitHub"),
        ("google", "Continue with Google"),
        ("gitlab", "Continue with GitLab"),
        ("discord", "Continue with Discord"),
        ("bitbucket", "Continue with Bitbucket"),
    ];
    for (mode, action) in [
        (AuthMode::Login, "/login"),
        (AuthMode::Register, "/register"),
    ] {
        let render = |providers: Vec<String>| {
            html(auth_page(
                mode,
                action,
                "csrf",
                FormValues::default(),
                false,
                providers,
                "/",
                None,
                false,
            ))
        };
        let output = render(LINKS.iter().map(|(id, _)| id.to_string()).collect());
        for (id, label) in LINKS {
            assert!(
                output.contains(&format!("href=\"/api/auth/oauth/{id}\"")),
                "{id}"
            );
            assert!(output.contains(label), "{label}");
        }
        for (id, label) in LINKS {
            // An unknown id is ignored rather than rendered as a link.
            let output = render(vec![id.to_owned(), "unknown".to_owned()]);
            for (other, other_label) in LINKS {
                let linked = other == id;
                assert_eq!(
                    output.contains(&format!("href=\"/api/auth/oauth/{other}\"")),
                    linked
                );
                assert_eq!(output.contains(other_label), linked, "{label}");
            }
            assert!(!output.contains("/api/auth/oauth/unknown"));
        }
        let output = render(vec!["unknown".to_owned()]);
        assert!(!output.contains("class=\"identity-auth-providers\""));
        assert!(!output.contains("Other sign-in options"));
        assert!(!output.contains("<details"));
    }
}

#[test]
fn provider_model_carries_only_provider_ids_and_defaults_missing_list_to_hidden() {
    let providers = vec!["github".to_owned(), "bitbucket".to_owned()];
    let page = Page::Auth {
        mode: AuthMode::Login,
        action: "/login".into(),
        csrf_token: "csrf".into(),
        values: FormValues::default(),
        private: false,
        providers,
        redirect: "/".into(),
        message: None,
        is_error: false,
    };
    let mut legacy = serde_json::to_value(page).unwrap();
    assert_eq!(
        legacy["Auth"]["providers"],
        serde_json::json!(["github", "bitbucket"])
    );
    assert!(serde_json::from_value::<Vec<String>>(serde_json::json!([{
        "id": "github",
        "client_secret": "secret-provider-marker",
    }]))
    .is_err());
    legacy
        .get_mut("Auth")
        .unwrap()
        .as_object_mut()
        .unwrap()
        .remove("providers");
    let decoded: Page = serde_json::from_value(legacy).unwrap();
    let Page::Auth { providers, .. } = decoded else {
        panic!("auth page changed variant");
    };
    assert!(providers.is_empty());
}

#[test]
fn auth_prerender_uses_exact_trusted_static_css_without_raw_user_markup() {
    let mut renderer = dioxus_ssr::Renderer::new();
    renderer.pre_render = true;
    let output = renderer.render_element(auth_page(
        AuthMode::Register,
        "/register",
        "csrf-static-css",
        FormValues {
            username: "</style><script>evil</script>".into(),
            email: "\"><img src=x>".into(),
            ..FormValues::default()
        },
        true,
        Vec::new(),
        "/alice?next=\"<script>",
        Some("</style><b>Real validation error</b>"),
        true,
    ));
    let css = output
        .split_once("<style")
        .unwrap()
        .1
        .split_once('>')
        .unwrap()
        .1
        .split_once("</style>")
        .unwrap()
        .0;
    assert_eq!(css, include_str!("../account.css"));
    assert!(!css.contains("<!--"));
    assert!(!output.contains("<script>"));
    assert!(!output.contains("<img src=x>"));
    assert!(!output.contains("<b>Real validation error</b>"));
    assert!(output.contains("&#60;script&#62;evil&#60;/script&#62;"));
    assert!(output.contains("&#60;img src=x&#62;"));
    assert!(output.contains("&#60;b&#62;Real validation error&#60;/b&#62;"));
    assert!(output.contains("value=\"csrf-static-css\""));
}

#[test]
fn auth_text_defaults_stay_uncontrolled_and_styles_stay_compact_and_scoped() {
    let auth_source = include_str!("page.rs")
        .split_once("pub fn auth_page(")
        .unwrap()
        .1
        .split_once("pub fn logout_page(")
        .unwrap()
        .0;
    assert!(auth_source.contains("\"value\": values.get(\"username\")"));
    assert!(auth_source.contains("\"value\": values.get(\"email\")"));
    assert!(!auth_source.contains("oninput:"));
    assert!(!auth_source.contains("onmount:"));
    let css = include_str!("../account.css");
    let auth_css = css.split_once("/* Compact native auth").unwrap().1;
    assert!(auth_css.contains("width: min(360px, calc(100% - 32px))"));
    assert!(auth_css.contains("min-height: 40px"));
    assert!(auth_css.contains("border-radius: 6px"));
    assert!(auth_css.contains("background: #1f883d"));
    for rejected in ["backdrop-filter", "blur(", "100vh", "100dvh", "2rem"] {
        assert!(
            !auth_css.contains(rejected),
            "rejected auth CSS: {rejected}"
        );
    }
    assert!(css.contains(".identity-profile-follow .identity-profile-button { width: 100%; }"));
}

#[test]
fn oauth_only_accounts_have_no_password_mutation_form() {
    let output = html(settings_page(
        Settings {
            two_factor: Default::default(),
            theme: crate::webui::document::Theme::System,
            profile: Profile::default(),
            email: String::new(),
            tab: "password".into(),
            keys: vec![],
            tokens: vec![],
            sessions: vec![],
            new_token: None,
            password_supported: false,
            must_change_password: false,
            manage_access: false,
            requests: vec![],
            values: FormValues::default(),
        },
        "token",
        None,
        false,
    ));
    assert!(output.contains("uses OAuth"));
    assert!(!output.contains("action=\"/settings/password\""));
}

#[test]
fn password_components_render_masked_native_inputs_and_derive_strength_only() {
    let output = html(password_field("new_password", "New password", true));
    assert!(output.contains("type=\"password\""));
    assert!(output.contains("autocomplete=\"new-password\""));
    assert!(!output.contains("value=\""));
    assert_eq!(password_strength(""), 0);
    assert_eq!(password_strength("abc"), 0);
    assert_eq!(password_strength("Password9!"), 4);
}

#[test]
fn sign_in_links_forgotten_passwords_to_a_help_page_that_changes_nothing() {
    let output = html(auth_page(
        AuthMode::Login,
        "/login",
        "login-csrf",
        FormValues::default(),
        false,
        Vec::new(),
        "/alice/project",
        None,
        false,
    ));
    let password = output.find("id=\"password\"").unwrap();
    let forgot = output.find("Forgot password?").unwrap();
    let form_end = output.find("</form>").unwrap();
    assert!(password < forgot && forgot < form_end, "{output}");
    assert!(output.contains("href=\"/forgot-password?redirect=%2Falice%2Fproject\""));
    let signup = html(auth_page(
        AuthMode::Register,
        "/register",
        "register-csrf",
        FormValues::default(),
        false,
        Vec::new(),
        "/alice/project",
        None,
        false,
    ));
    assert!(!signup.contains("Forgot password?"));

    let help = html(password_help_page("/alice/project"));
    assert!(help.contains("Forgot your password?"));
    assert!(help.contains("Codeyard does not email password resets"));
    assert!(help.contains("href=\"/login?redirect=%2Falice%2Fproject\""));
    assert!(!help.contains("<form"));
}
