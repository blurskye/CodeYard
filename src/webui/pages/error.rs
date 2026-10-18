//! The one error page. Native HTML errors and hydrated navigation both render it.
//! A repository error passes the repository header, which the backend builds only for
//! a viewer who may read that repository.
use crate::webui::{
    components::SiteSearch,
    navigation::NavLink,
    pages::repos::chrome::{chrome, RepoChrome},
};
use dioxus::prelude::*;

pub fn render(status: u16, message: &str, repo: Option<&RepoChrome>, signed_in: bool) -> Element {
    let heading = heading_for(status);
    let content = rsx! {
        div { class: "error-page",
            p { class: "error-code", "aria-hidden": "true", "{status}" }
            if repo.is_some() {
                h2 { "{heading}" }
            } else {
                h1 { "{heading}" }
            }
            p { class: "error-message", "{message}" }
            if status == 404 {
                SiteSearch { id: "error-search-query", class: "site-search error-search" }
            }
            div { class: "error-actions",
                if let Some(repo) = repo {
                    NavLink { href: repo.base(), class: "btn btn-primary".to_string(), "Go to repository" }
                } else if signed_in {
                    NavLink { href: "/".to_string(), class: "btn btn-primary".to_string(), "Return home" }
                    NavLink { href: "/explore".to_string(), class: "btn".to_string(), "Explore repositories" }
                } else {
                    NavLink { href: "/login".to_string(), class: "btn btn-primary".to_string(), "Sign in" }
                    NavLink { href: "/".to_string(), class: "btn".to_string(), "Return home" }
                }
            }
        }
    };
    match repo {
        Some(repo) => chrome(repo, rsx! { section { class: "py-10", {content} } }),
        None => rsx! { section { class: "page-shell py-10", {content} } },
    }
}

fn heading_for(status: u16) -> &'static str {
    match status {
        401 => "Sign in to continue",
        403 => "Access denied",
        404 => "Page not found",
        413 => "Request is too large",
        429 => "Too many requests",
        503 => "Temporarily unavailable",
        500..=599 => "Something went wrong",
        _ => "Can't complete this request",
    }
}

#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests {
    use super::*;

    fn html(element: Element) -> String {
        dioxus_ssr::render_element(element)
    }

    #[test]
    fn signed_out_error_offers_sign_in_and_plain_heading() {
        let page = html(render(404, "Check the address and try again.", None, false));
        assert!(page.contains("Page not found"));
        assert!(page.contains("Check the address and try again."));
        assert!(page.contains("href=\"/login\""));
        assert!(!page.contains("Request failed"));
    }

    #[test]
    fn not_found_page_offers_a_search_field() {
        let page = html(render(404, "Check the address and try again.", None, true));
        assert!(page.contains("action=\"/search\""));
        assert!(page.contains("error-search"));
        let forbidden = html(render(403, "You cannot open this page.", None, true));
        assert!(!forbidden.contains("action=\"/search\""));
    }

    #[test]
    fn signed_in_error_shows_the_status_and_a_way_back() {
        let page = html(render(403, "You cannot open this page.", None, true));
        assert!(page.contains("error-code"));
        assert!(page.contains(">403<"));
        assert!(page.contains("Access denied"));
        assert!(page.contains("Return home"));
        assert!(page.contains("href=\"/explore\""));
        assert!(!page.contains("href=\"/login\""));
    }

    #[test]
    fn repository_error_keeps_the_repository_header_and_links_to_its_root() {
        let repo = RepoChrome {
            owner: "alice".into(),
            name: "demo".into(),
            current_path: "/alice/demo/blob/main/nope.rs".into(),
            ..RepoChrome::default()
        };
        let page = html(render(
            404,
            "Check the path or revision.",
            Some(&repo),
            true,
        ));
        assert!(page.contains("href=\"/alice/demo\""));
        assert!(page.contains("Go to repository"));
        assert!(page.contains("Page not found"));
        // The repository tabs are drawn by the site header, which gets the repository too.
        let header = html(crate::webui::components::shell(
            crate::webui::document::Chrome::default(),
            Some(repo.clone()),
            rsx! {},
        ));
        assert!(header.contains("repo-tabs"));
        assert!(header.contains("href=\"/alice/demo\""));
    }

    #[test]
    fn statuses_have_plain_headings() {
        assert_eq!(heading_for(403), "Access denied");
        assert_eq!(heading_for(409), "Can't complete this request");
        assert_eq!(heading_for(500), "Something went wrong");
    }
}
