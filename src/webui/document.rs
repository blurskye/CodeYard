#[cfg(not(target_arch = "wasm32"))]
use dioxus::prelude::*;
use serde::{Deserialize, Serialize};

/// The browser toolbar colours that go with the dark and light appearances.
pub const CHROME_DARK: &str = "#010409";
pub const CHROME_LIGHT: &str = "#f6f8fa";

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum Theme {
    #[default]
    System,
    Light,
    Dark,
}
impl Theme {
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "system" => Some(Self::System),
            "light" => Some(Self::Light),
            "dark" => Some(Self::Dark),
            _ => None,
        }
    }
    pub fn as_str(self) -> &'static str {
        match self {
            Self::System => "system",
            Self::Light => "light",
            Self::Dark => "dark",
        }
    }
    /// The header button steps through system, dark and light, then starts over.
    pub fn next(self) -> Self {
        match self {
            Self::System => Self::Dark,
            Self::Dark => Self::Light,
            Self::Light => Self::System,
        }
    }
}

/// Only explicit presentation data crosses the server/browser boundary.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Chrome {
    /// This page contains values from a native POST, not GET defaults.
    #[serde(default)]
    pub submitted_form: Option<String>,
    pub username: Option<String>,
    pub is_admin: bool,
    pub csrf_token: String,
    pub return_to: String,
    pub theme: Theme,
    /// Unread inbox notifications for the header badge. None where no count was read.
    pub unread_notifications: Option<i64>,
}

/// Explicit native/error fallback. Normal product GETs use render_model so the
/// browser receives the exact tree and owned data used by server rendering.
#[cfg(not(target_arch = "wasm32"))]
pub fn render(
    title: &str,
    chrome: Chrome,
    repo: Option<crate::webui::pages::repos::chrome::RepoChrome>,
    body: Element,
) -> String {
    let theme = chrome.theme;
    document(title, theme, super::components::shell(chrome, repo, body))
}

#[cfg(not(target_arch = "wasm32"))]
pub fn render_model(
    initial: super::app::PageEnvelope,
    runtime_module: &str,
) -> Result<String, serde_json::Error> {
    let title = initial.title.clone();
    let theme = initial.chrome.theme;
    let (html, bootstrap) = super::bootstrap::prerender(initial);
    let json = super::bootstrap::safe_json(&bootstrap)?;
    Ok(document(
        &title,
        theme,
        rsx! {
            div { id: super::bootstrap::ROOT_ID, dangerous_inner_html: html }
            script { id: super::bootstrap::BOOTSTRAP_ID, r#type: "application/json", dangerous_inner_html: json }
            // The dx-generated module initializes its WASM itself. No inline loader,
            // handwritten JS, or second CSR tree is inserted here.
            script { r#type: "module", src: runtime_module.to_string() }
        },
    ))
}

#[cfg(not(target_arch = "wasm32"))]
fn document(title: &str, theme: Theme, body: Element) -> String {
    let title = crate::webui::app::tab_title(title);
    let element = rsx! {
            head {
                meta { charset: "utf-8" }
                meta { name: "viewport", content: "width=device-width, initial-scale=1" }
                meta { name: "color-scheme", content: "dark light" }
                match theme {
                    Theme::System => rsx! {
                        meta { name: "theme-color", content: CHROME_DARK, "media": "(prefers-color-scheme: dark)" }
                        meta { name: "theme-color", content: CHROME_LIGHT, "media": "(prefers-color-scheme: light)" }
                    },
                    Theme::Light => rsx! { meta { name: "theme-color", content: CHROME_LIGHT } },
                    Theme::Dark => rsx! { meta { name: "theme-color", content: CHROME_DARK } },
                }
                title { "{title}" }
                link { rel: "icon", r#type: "image/png", href: "/assets/favicon.png" }
                link { rel: "preload", href: "/assets/fonts/mona-sans.woff2", r#as: "font", r#type: "font/woff2", crossorigin: "anonymous" }
                link { rel: "stylesheet", href: "/assets/tailwind.css" }
                link { rel: "stylesheet", href: "/assets/actions.css" }
                link { rel: "stylesheet", href: "/assets/native.css" }
            }
            body { class: "site-document-body min-h-screen bg-background text-foreground", {body} }
    };
    // The outer document is not hydrated. Only the identical App rooted at
    // #dioxus-root is pre-rendered with standard framework hydration markers.
    // dioxus-html 0.7.10 intentionally has no typed `html` element. The outer
    // frame contains only fixed attributes and a closed Theme enum; request text,
    // head/body and the hydrated application are still rendered by Dioxus.
    format!(
        "<!DOCTYPE html><html lang=\"en\" class=\"{}\">{}</html>",
        theme.as_str(),
        dioxus_ssr::render_element(element)
    )
}

#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests {
    use super::*;
    use crate::webui::{
        app::{Page, PageEnvelope},
        bootstrap::{Bootstrap, BOOTSTRAP_ID},
    };
    #[test]
    fn native_fallback_document_is_complete_and_escaped() {
        let html = render(
            "<private>",
            Chrome::default(),
            None,
            rsx! { h1 { "<script>alert(1)</script>" } },
        );
        assert!(html.starts_with("<!DOCTYPE html><html"));
        assert!(html.contains("&#60;private&#62;"));
        assert!(html.contains("&#60;script&#62;"));
        assert!(html.contains("class=\"site-brand-mark\""));
        assert!(html.contains("rel=\"icon\" type=\"image/png\" href=\"/assets/favicon.png\""));
        assert!(!html.contains("src=\"/assets/logo.svg\""));
        assert!(html.ends_with("</html>"));
        assert!(!html.contains("<script")); // fallback-only evidence, not hydrated target guard
    }
    #[test]
    fn explicit_theme_sets_matching_browser_chrome_color() {
        let light = render(
            "Light",
            Chrome {
                theme: Theme::Light,
                ..Default::default()
            },
            None,
            rsx! { p { "Light page" } },
        );
        assert!(light.contains("name=\"theme-color\" content=\"#f6f8fa\""));
        assert!(!light.contains("prefers-color-scheme"));

        let system = render(
            "System",
            Chrome::default(),
            None,
            rsx! { p { "System page" } },
        );
        assert_eq!(system.matches("name=\"theme-color\"").count(), 2);
        assert!(system.contains("media=\"(prefers-color-scheme: dark)\""));
        assert!(system.contains("media=\"(prefers-color-scheme: light)\""));
    }

    #[test]
    fn signed_in_fallback_shell_contains_native_csrf_forms() {
        let html = render(
            "Home",
            Chrome {
                username: Some("alice".into()),
                csrf_token: "token".into(),
                return_to: "/".into(),
                ..Default::default()
            },
            None,
            rsx! { p { "Hello Alice" } },
        );
        assert!(html.contains("action=\"/logout\""));
        assert!(html.contains("name=\"csrf_token\""));
        assert!(html.contains("method=\"post\""));
        assert!(html.contains("<details"));
    }
    #[test]
    fn hydrated_document_uses_same_safe_model_and_framework_markers_before_loader() {
        let initial = PageEnvelope {
            title: "<private>".into(),
            location: "/search?q=rust".into(),
            chrome: Chrome {
                csrf_token: "a".repeat(64),
                return_to: "/search?q=rust".into(),
                ..Default::default()
            },
            page: Page::Error {
                status: 404,
                message: "</script><script>evil()</script>&\u{2028}".into(),
                repo: None,
                signed_in: false,
            },
        };
        let html = render_model(initial.clone(), "/assets/framework-123.js").unwrap();
        assert!(html.contains("id=\"dioxus-root\""));
        assert!(html.contains("data-node-hydration"));
        assert!(html.contains("&#60;script&#62;evil()&#60;/script&#62;"));
        assert!(!html.contains("<script>evil()"));
        let marker = format!("id=\"{BOOTSTRAP_ID}\"");
        let tag = html.find(&marker).expect("bootstrap JSON script");
        let start = tag + html[tag..].find('>').unwrap() + 1;
        let end = html[start..].find("</script>").unwrap() + start;
        let decoded: Bootstrap = serde_json::from_str(&html[start..end]).unwrap();
        assert_eq!(decoded.initial, initial);
        assert!(html[end..].contains("src=\"/assets/framework-123.js\""));
        assert!(!html.contains("initial_dioxus_hydration_data=")); // no handwritten inline assignment
    }
    #[test]
    fn identical_props_produce_identical_prerendered_tree() {
        let initial = PageEnvelope {
            title: "Missing".into(),
            location: "/missing".into(),
            chrome: Chrome::default(),
            page: Page::Error {
                status: 404,
                message: "Not found".into(),
                repo: None,
                signed_in: false,
            },
        };
        let (first, _) = crate::webui::bootstrap::prerender(initial.clone());
        let decoded = serde_json::from_str(&serde_json::to_string(&initial).unwrap()).unwrap();
        let (second, _) = crate::webui::bootstrap::prerender(decoded);
        assert_eq!(first, second);
    }
}
