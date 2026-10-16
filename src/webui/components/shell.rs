use super::{DraftInput, ThemeForm};
use crate::webui::{document::Chrome, encode_component, navigation::NavLink};
use dioxus::prelude::*;

pub fn shell(chrome: Chrome, body: Element) -> Element {
    rsx! {
        a { href: "#main", class: "skip-link", "Skip to content" }
        SiteHeader { chrome }
        // Keep the prebuilt keyed PageBody intact: shell presentation must not remount drafts.
        main { id: "main", class: "site-main", tabindex: "-1", {body} }
    }
}

#[component]
fn SiteHeader(chrome: Chrome) -> Element {
    let token = chrome.csrf_token;
    let return_to = chrome.return_to;
    rsx! {
        header { class: "site-header",
            onkeydown: move |event| {
                if event.key() == Key::Escape { close_header_menus(); }
            },
            div { class: "site-header-row",
                details { id: "site-navigation-menu", class: "native-menu site-navigation", "name": "site-header-menu",
                    summary { class: "site-icon-button", "aria-label": "Open main navigation", title: "Main navigation",
                        {shell_icon(ShellIcon::Menu)}
                    }
                    nav { class: "native-menu-panel site-menu-links", "aria-label": "Main navigation", onclick: move |_| close_header_menus(),
                        p { class: "site-menu-heading", "Codeyard" }
                        NavLink { href: "/".to_string(), class: "site-menu-link".to_string(), "Home" }
                        NavLink { href: "/explore".to_string(), class: "site-menu-link".to_string(), "Explore" }
                        if let Some(username) = chrome.username.as_ref() {
                            NavLink { href: format!("/{}?tab=repositories", encode_component(username)), class: "site-menu-link".to_string(), "Your repositories" }
                            NavLink { href: "/new".to_string(), class: "site-menu-link".to_string(), "New repository" }
                        } else {
                            NavLink { href: "/login".to_string(), class: "site-menu-link".to_string(), "Sign in" }
                            NavLink { href: "/register".to_string(), class: "site-menu-link".to_string(), "Sign up" }
                        }
                    }
                }
                NavLink { href: "/".to_string(), class: "site-brand".to_string(), aria_label: Some("Codeyard home".to_string()), mark_current: false,
                                        img { src: "/assets/favicon.png", alt: "", width: "128", height: "128", class: "site-brand-mark" }
                    span { class: "site-brand-name", "Codeyard" }
                }
                {site_search("site-search-query", "site-search site-search-wide")}
                div { class: "site-header-spacer" }
                // On a phone the search is a button that opens the same form in a full-width row.
                details { class: "site-search-disclosure",
                    summary { class: "site-icon-button", "aria-label": "Search repositories", title: "Search repositories",
                        {shell_icon(ShellIcon::Search)}
                    }
                    {site_search("site-search-query-compact", "site-search site-search-compact")}
                }
                div { class: "site-header-actions",
                    if chrome.username.is_some() {
                        NavLink { href: "/new".to_string(), class: "site-icon-button".to_string(), aria_label: Some("New repository".to_string()),
                            {shell_icon(ShellIcon::Plus)}
                            span { class: "site-sr-only", "New repository" }
                        }
                        NavLink { href: "/notifications".to_string(), class: "site-icon-button".to_string(), aria_label: Some("Notifications".to_string()),
                            {shell_icon(ShellIcon::Bell)}
                            span { class: "site-sr-only", "Notifications" }
                        }
                    }
                    if !token.is_empty() {
                        ThemeForm { csrf_token: token.clone(), return_to, theme: chrome.theme }
                    }
                    if let Some(username) = chrome.username {
                        details { id: "site-account-menu", class: "native-menu site-account-menu", "name": "site-header-menu",
                            summary { class: "site-profile-button", "aria-label": format!("Account menu for {username}"), title: username.clone(),
                                // Chrome intentionally has no avatar URL; use the original fallback glyph.
                                span { class: "site-user-glyph", {shell_icon(ShellIcon::User)} }
                            }
                            div { class: "native-menu-panel",
                                p { class: "site-menu-heading",
                                    span { "Signed in as" }
                                    strong { "{username}" }
                                }
                                div { class: "site-menu-links", onclick: move |_| close_header_menus(),
                                    NavLink { href: format!("/{}", encode_component(&username)), class: "site-menu-link".to_string(), "Your profile" }
                                    NavLink { href: format!("/{}?tab=repositories", encode_component(&username)), class: "site-menu-link".to_string(), "Your repositories" }
                                    NavLink { href: "/settings/organizations".to_string(), class: "site-menu-link".to_string(), "Organizations" }
                                    NavLink { href: "/settings".to_string(), class: "site-menu-link".to_string(), "Settings" }
                                    if chrome.is_admin { NavLink { href: "/admin".to_string(), class: "site-menu-link".to_string(), "Administration" } }
                                    form { action: "/logout", method: "post", class: "site-signout-form",
                                        input { r#type: "hidden", name: "csrf_token", value: token.clone() }
                                        button { r#type: "submit", class: "site-menu-link", "Sign out" }
                                    }
                                }
                            }
                        }
                    } else {
                        NavLink { href: "/login".to_string(), class: "site-auth-link".to_string(), "Sign in" }
                        NavLink { href: "/register".to_string(), class: "site-auth-link site-signup-link".to_string(), "Sign up" }
                    }
                }
            }
        }
    }
}

fn site_search(id: &'static str, class: &'static str) -> Element {
    rsx! {
        form { action: "/search", method: "get", class, role: "search", "aria-label": "Search repositories",
            label { r#for: id, class: "site-sr-only", "Search repositories" }
            DraftInput { id: Some(id.to_string()), name: "q".to_string(), initial_value: String::new(), input_type: "search".to_string(), placeholder: "Search repositories…".to_string(), class: "site-search-input".to_string() }
            button { r#type: "submit", class: "site-search-submit", "aria-label": "Search repositories", title: "Search repositories",
                {shell_icon(ShellIcon::Search)}
            }
        }
    }
}

fn close_header_menus() {
    #[cfg(target_arch = "wasm32")]
    if let Some(document) = web_sys::window().and_then(|window| window.document()) {
        for id in ["site-navigation-menu", "site-account-menu"] {
            if let Some(menu) = document.get_element_by_id(id) {
                let _ = menu.remove_attribute("open");
            }
        }
    }
}

#[derive(Clone, Copy)]
pub(super) enum ShellIcon {
    Heading,
    Bold,
    Italic,
    Quote,
    Code,
    Link,
    ListBullets,
    ListNumbers,
    ListTodo,
    Menu,
    Search,
    Plus,
    Bell,
    User,
    Sun,
    Moon,
    Monitor,
}

pub(super) fn shell_icon(icon: ShellIcon) -> Element {
    let glyph = match icon {
        ShellIcon::Menu => rsx! {
            path { d: "M4 6h16M4 12h16M4 18h16" }
        },
        ShellIcon::Search => rsx! {
            circle { cx: "11", cy: "11", r: "8" }
            path { d: "m21 21-4.3-4.3" }
        },
        ShellIcon::Plus => rsx! {
            path { d: "M12 5v14M5 12h14" }
        },
        ShellIcon::Bell => rsx! {
            path { d: "M10.268 21a2 2 0 0 0 3.464 0" }
            path { d: "M3.262 15.326A1 1 0 0 0 4 17h16a1 1 0 0 0 .74-1.673C19.41 13.956 18 12.499 18 8A6 6 0 0 0 6 8c0 4.499-1.411 5.956-2.738 7.326" }
        },
        ShellIcon::User => rsx! {
            path { d: "M20 21v-2a4 4 0 0 0-4-4H8a4 4 0 0 0-4 4v2" }
            circle { cx: "12", cy: "7", r: "4" }
        },
        ShellIcon::Sun => rsx! {
            circle { cx: "12", cy: "12", r: "4" }
            path { d: "M12 2v2" }
            path { d: "M12 20v2" }
            path { d: "m4.93 4.93 1.41 1.41" }
            path { d: "m17.66 17.66 1.41 1.41" }
            path { d: "M2 12h2" }
            path { d: "M20 12h2" }
            path { d: "m6.34 17.66-1.41 1.41" }
            path { d: "m19.07 4.93-1.41 1.41" }
        },
        ShellIcon::Moon => rsx! {
            path { d: "M12 3a6 6 0 0 0 9 9 9 9 0 1 1-9-9Z" }
        },
        ShellIcon::Heading => rsx! {
            path { d: "M4 12h8M4 18V6M12 18V6M17 12l3-2v8" }
        },
        ShellIcon::Bold => rsx! {
            path { d: "M6 12h9a4 4 0 0 1 0 8H7a1 1 0 0 1-1-1V5a1 1 0 0 1 1-1h7a4 4 0 0 1 0 8" }
        },
        ShellIcon::Italic => rsx! {
            path { d: "M19 4h-9M14 20H5M15 4 9 20" }
        },
        ShellIcon::Quote => rsx! {
            path { d: "M7 17a3 3 0 0 0 3-3V8H5v6h2M17 17a3 3 0 0 0 3-3V8h-5v6h2" }
        },
        ShellIcon::Code => rsx! {
            path { d: "m16 18 6-6-6-6M8 6l-6 6 6 6" }
        },
        ShellIcon::Link => rsx! {
            path { d: "M10 13a5 5 0 0 0 7.54.54l3-3a5 5 0 0 0-7.07-7.07l-1.72 1.71M14 11a5 5 0 0 0-7.54-.54l-3 3a5 5 0 0 0 7.07 7.07l1.71-1.71" }
        },
        ShellIcon::ListBullets => rsx! {
            path { d: "M8 6h13M8 12h13M8 18h13M3 6h.01M3 12h.01M3 18h.01" }
        },
        ShellIcon::ListNumbers => rsx! {
            path { d: "M10 6h11M10 12h11M10 18h11M4 6h1v4M4 10h2M6 18H4c0-1 2-2 2-3s-1-1.5-2-1" }
        },
        ShellIcon::ListTodo => rsx! {
            rect { x: "3", y: "5", width: "6", height: "6", rx: "1" }
            path { d: "m3 17 2 2 4-4M13 6h8M13 12h8M13 18h8" }
        },
        ShellIcon::Monitor => rsx! {
            rect { x: "2", y: "3", width: "20", height: "14", rx: "2" }
            path { d: "M8 21h8" }
            path { d: "M12 17v4" }
        },
    };
    rsx! {
        svg { "aria-hidden": "true", width: "16", height: "16", view_box: "0 0 24 24", fill: "none", stroke: "currentColor", stroke_width: "2", stroke_linecap: "round", stroke_linejoin: "round", {glyph} }
    }
}

#[cfg(all(test, not(target_arch = "wasm32")))]
mod shell_tests {
    use super::*;

    #[test]
    fn signed_in_shell_keeps_real_controls_and_compact_brand_mark() {
        let html = dioxus_ssr::render_element(shell(
            Chrome {
                username: Some("alice".into()),
                is_admin: true,
                csrf_token: "owned-csrf-token".into(),
                return_to: "/alice/project/actions".into(),
                ..Default::default()
            },
            rsx! { section { id: "shell-owner-body", "Body from its feature owner" } },
        ));
        for required in [
            "src=\"/assets/favicon.png\"",
            "width=\"128\"",
            "height=\"128\"",
            "aria-label=\"Notifications\"",
            "aria-label=\"New repository\"",
            "aria-label=\"Account menu for alice\"",
            "action=\"/search\"",
            "action=\"/logout\"",
            "action=\"/_forms/theme\"",
            "name=\"csrf_token\"",
            "value=\"owned-csrf-token\"",
            "href=\"/admin\"",
            "href=\"/alice?tab=repositories\"",
            "id=\"shell-owner-body\"",
            "aria-label=\"Codeyard home\"",
            "site-brand-name",
        ] {
            assert!(
                html.contains(required),
                "Missing shell contract: {required}"
            );
        }
        assert_eq!(html.matches("id=\"shell-owner-body\"").count(), 1);
        assert!(!html.contains("<script")); // Native fallback, not a no-hydration policy.
    }

    #[test]
    fn phone_search_is_a_disclosure_beside_the_header_actions() {
        let html = dioxus_ssr::render_element(shell(
            Chrome {
                username: Some("alice".into()),
                csrf_token: "token".into(),
                ..Default::default()
            },
            rsx! { p { "Body" } },
        ));
        // The search form is shown in the wide header and again inside the phone disclosure,
        // whose summary is the icon button that opens it.
        assert_eq!(html.matches("action=\"/search\"").count(), 2);
        let disclosure = html
            .split_once("class=\"site-search-disclosure\"")
            .unwrap_or_else(|| panic!("disclosure: {html}"))
            .1
            .split_once("</details>")
            .unwrap()
            .0;
        assert!(disclosure.contains("<summary"), "{disclosure}");
        assert!(
            disclosure.contains("aria-label=\"Search repositories\""),
            "{disclosure}"
        );
        assert!(disclosure.contains("site-search-compact"), "{disclosure}");
        assert!(
            html.contains("class=\"site-search site-search-wide\""),
            "{html}"
        );
        // The phone row keeps the header actions after the disclosure.
        assert!(html.find("site-search-disclosure") < html.find("site-header-actions"));
    }

    #[test]
    fn shell_escapes_identity_and_does_not_fabricate_private_chrome() {
        let html = dioxus_ssr::render_element(shell(
            Chrome {
                username: Some("<private>".into()),
                csrf_token: "token".into(),
                ..Default::default()
            },
            rsx! { p { "Owned body" } },
        ));
        assert!(html.contains("&#60;private&#62;"));
        assert!(html.contains("href=\"/%3Cprivate%3E\""));
        assert!(!html.contains("<private>"));
        assert!(!html.contains("href=\"/admin\""));
        assert!(!html.contains("avatar_url"));
        assert!(html.contains("site-brand-name"));
        assert!(!html.contains("site-footer"));

        let anonymous =
            dioxus_ssr::render_element(shell(Chrome::default(), rsx! { p { "Public body" } }));
        assert!(!anonymous.contains("site-repository-context"));
        assert!(anonymous.contains("href=\"/login\""));
        assert!(anonymous.contains("href=\"/register\""));
        assert!(!anonymous.contains("href=\"/notifications\""));
        assert!(!anonymous.contains("href=\"/new\""));
        assert!(!anonymous.contains("action=\"/logout\""));
        assert!(!anonymous.contains("action=\"/_forms/theme\""));
    }
}
