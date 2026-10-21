use super::app::{AppearanceEpoch, CurrentPage, RefreshEpoch};
use dioxus::html::{input_data::MouseButton, ModifiersInteraction};
use dioxus::prelude::*;
use dioxus_router::Routable;
use std::rc::Rc;

mod load;

/// Schedule a scoped, authorized read refresh. Browser feature lanes can invoke
/// this after a live event/timer; it never moves domain authority to the client.
pub fn refresh_page() {
    #[cfg(target_arch = "wasm32")]
    if let Some(RefreshEpoch(mut epoch)) = try_consume_context::<RefreshEpoch>() {
        let next = *epoch.peek() + 1;
        epoch.set(next);
    }
}

#[component]
pub(crate) fn RefreshLink(href: String) -> Element {
    rsx! {
        a { class: "actions-button", href, onclick: move |event| {
            #[cfg(target_arch = "wasm32")]
            {
                // A modified or middle click keeps the browser's own behaviour.
                if event.modifiers().is_empty() && event.trigger_button() == Some(MouseButton::Primary) {
                    event.prevent_default();
                    crate::webui::navigation::refresh_page();
                }
            }
            #[cfg(not(target_arch = "wasm32"))]
            let _ = event;
        }, "Refresh" }
    }
}

/// Points the browser toolbar colour at the page's appearance. The server wrote these tags for
/// the appearance it rendered; System follows the device through two tags with a media query.
#[cfg(target_arch = "wasm32")]
fn set_theme_color(document: &web_sys::Document, theme: super::document::Theme) {
    use super::document::{Theme, CHROME_DARK, CHROME_LIGHT};
    use wasm_bindgen::JsCast;
    let Ok(Some(head)) = document.query_selector("head") else {
        return;
    };
    if let Ok(old) = document.query_selector_all("meta[name=theme-color]") {
        for index in 0..old.length() {
            if let Some(tag) = old
                .item(index)
                .and_then(|node| node.dyn_into::<web_sys::Element>().ok())
            {
                tag.remove();
            }
        }
    }
    let tags: &[(&str, Option<&str>)] = match theme {
        Theme::System => &[
            (CHROME_DARK, Some("(prefers-color-scheme: dark)")),
            (CHROME_LIGHT, Some("(prefers-color-scheme: light)")),
        ],
        Theme::Light => &[(CHROME_LIGHT, None)],
        Theme::Dark => &[(CHROME_DARK, None)],
    };
    for (color, media) in tags {
        if let Ok(tag) = document.create_element("meta") {
            let _ = tag.set_attribute("name", "theme-color");
            let _ = tag.set_attribute("content", color);
            if let Some(media) = media {
                let _ = tag.set_attribute("media", media);
            }
            let _ = head.append_child(&tag);
        }
    }
}

/// An opaque URL for the standard Dioxus Router/history. Product matching stays
/// on the backend. Implement Routable directly because the 0.7.10 derive decodes
/// a spread query before reformatting it (losing encoded '&'/'+'), and its segment
/// formatting does not escape embedded slashes. No custom navigation/DOM runtime.
#[derive(Clone, Debug, PartialEq)]
pub struct PageRoute(String);
impl std::str::FromStr for PageRoute {
    type Err = &'static str;
    fn from_str(target: &str) -> Result<Self, Self::Err> {
        safe_page_target(target)
            .then(|| Self(target.to_string()))
            .ok_or("Invalid local page target")
    }
}
impl std::fmt::Display for PageRoute {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}
impl Routable for PageRoute {
    const SITE_MAP: &'static [dioxus_router::routable::SiteMapSegment] =
        &[dioxus_router::routable::SiteMapSegment {
            segment_type: dioxus_router::routable::SegmentType::CatchAll("path"),
            children: &[],
        }];
    fn render(&self, _level: usize) -> Element {
        rsx! { PageRouteView { target: self.0.clone() } }
    }
}

pub(crate) fn page_history(initial: &str) -> Rc<dyn dioxus_history::History> {
    #[cfg(target_arch = "wasm32")]
    {
        let _ = initial;
        let history = dioxus_web::WebHistory::new(None, false);
        if let Some(history) = web_sys::window().and_then(|window| window.history().ok()) {
            let _ = history.set_scroll_restoration(web_sys::ScrollRestoration::Manual);
        }
        Rc::new(history)
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        Rc::new(dioxus_history::MemoryHistory::with_initial_path(initial))
    }
}

/// A link to a page inside the app. It is a plain anchor that navigates through the router, so
/// `aria-current` is written in one place: the caller's value, or "page" when the link points to
/// the current route. A router `Link` adds its own mark as well, which doubles the attribute in
/// the server HTML and clears it when the page changes in the app. `mark_current: false` is for a
/// link that is not a navigation to the page it sits on, such as the logo.
#[component]
pub fn NavLink(
    href: String,
    #[props(default)] class: String,
    #[props(default)] aria_label: Option<String>,
    #[props(default)] aria_current: Option<String>,
    #[props(default)] title: Option<String>,
    #[props(default = true)] mark_current: bool,
    children: Element,
) -> Element {
    // Transitional static/error documents have no Router. They still get useful
    // ordinary anchors instead of a panic or an inert link.
    let router = dioxus_router::try_router();
    if let Some(router) = router.filter(|_| safe_page_target(&href) && !href.contains('#')) {
        let aria_current = aria_current.or_else(|| {
            (mark_current && router.full_route_string() == href).then(|| "page".to_owned())
        });
        let target = href.clone();
        rsx! {
            a { href, class, title, "aria-label": aria_label, "aria-current": aria_current, onclick: move |event| {
                // Modified and middle clicks keep the browser's own behaviour.
                if event.modifiers().is_empty() && event.trigger_button() == Some(MouseButton::Primary) {
                    event.prevent_default();
                    router.push(target.clone());
                }
            }, {children} }
        }
    } else {
        rsx! { a { href, class, title, "aria-label": aria_label, "aria-current": aria_current, {children} } }
    }
}

pub fn safe_page_target(target: &str) -> bool {
    // Only the path decides where a target goes. A query or fragment is data, so an
    // encoded `\` there is harmless.
    let path = target.split(['?', '#']).next().unwrap_or_default();
    path.starts_with('/')
        && !path.starts_with("//")
        && !target.chars().any(|c| c.is_control() || c == '\\')
        && !path.to_ascii_lowercase().starts_with("/%2f")
        && !path.to_ascii_lowercase().contains("%5c")
}

/// Repeated polls of one slow read are coalesced; a new location replaces it.
#[cfg(any(target_arch = "wasm32", test))]
fn should_start_read(active_target: Option<&str>, target: &str) -> bool {
    active_target != Some(target)
}

#[cfg(any(target_arch = "wasm32", test))]
fn may_accept_read(
    read_generation: u64,
    current_generation: u64,
    requested: &str,
    intended: &str,
) -> bool {
    read_generation == current_generation && requested == intended
}

#[cfg(target_arch = "wasm32")]
struct InFlight {
    target: String,
    controller: web_sys::AbortController,
    task: dioxus::dioxus_core::Task,
}
#[cfg(target_arch = "wasm32")]
impl InFlight {
    fn cancel(self) {
        self.controller.abort();
        self.task.cancel();
    }
}

#[component]
fn PageRouteView(target: String) -> Element {
    let CurrentPage(current) = use_context();
    let RefreshEpoch(refresh) = use_context();
    let AppearanceEpoch(appearance) = use_context();
    let loading = use_signal(|| false);
    let failure = use_signal(|| None::<String>);
    #[cfg(target_arch = "wasm32")]
    {
        let mut loading = loading;
        let mut failure = failure;
        let router = dioxus_router::router();
        let mut started = use_signal(|| false);
        let mut adopted = use_signal(|| None::<(String, u64)>);
        let mut pending_scroll = use_signal(|| None::<[f64; 2]>);
        let mut generation = use_signal(|| 0_u64);
        let active = use_hook(|| Rc::new(std::cell::RefCell::new(None::<InFlight>)));
        let drop_active = active.clone();
        use_drop(move || {
            if let Some(request) = drop_active.borrow_mut().take() {
                request.cancel();
            }
        });
        use_effect(move || {
            let target = router.full_route_string();
            let epoch = refresh();
            if adopts_loaded_location(adopted.peek().as_ref(), &target, epoch) {
                adopted.set(None);
                return;
            }
            if !*started.peek() {
                started.set(true);
                return; // The authorized initial server props are already complete.
            }
            if !should_start_read(
                active
                    .borrow()
                    .as_ref()
                    .map(|request| request.target.as_str()),
                &target,
            ) {
                return; // Never restart/starve a slow request on each poll tick.
            }
            if let Some(previous) = active.borrow_mut().take() {
                previous.cancel();
            }
            let controller = match web_sys::AbortController::new() {
                Ok(controller) => controller,
                Err(_) => {
                    failure.set(Some(
                        "Interactive navigation is unavailable. Open the link normally.".into(),
                    ));
                    return;
                }
            };
            let scroll = (target != current.peek().location).then(history_scroll);
            let signal = controller.signal();
            let timeout_controller = controller.clone();
            let next = *generation.peek() + 1;
            generation.set(next);
            loading.set(true);
            failure.set(None);
            let request_target = target.clone();
            let read_appearance = *appearance.peek();
            let request_active = active.clone();
            let task = spawn(async move {
                let request = load::load_page(&request_target, &signal);
                let timeout = gloo_timers::future::TimeoutFuture::new(MODEL_TIMEOUT_MS);
                futures_util::pin_mut!(request, timeout);
                let result = match futures_util::future::select(request, timeout).await {
                    futures_util::future::Either::Left((result, _)) => result,
                    futures_util::future::Either::Right(_) => {
                        timeout_controller.abort();
                        Err(load::LoadError::Timeout)
                    }
                };
                if *generation.peek() != next {
                    return;
                }
                request_active.borrow_mut().take();
                // History changes synchronously, before the replacement effect
                // can advance generation. Reject models, errors AND redirects in
                // that interval without preventing the next route's request.
                if !may_accept_read(
                    next,
                    *generation.peek(),
                    &request_target,
                    &router.full_route_string(),
                ) {
                    return;
                }
                loading.set(false);
                match result {
                    Ok(load::PageLoad::Model(mut envelope)) => {
                        let location = envelope.location.clone();
                        let mut current = current;
                        envelope.chrome.theme = super::app::theme_for_read(
                            read_appearance,
                            *appearance.peek(),
                            envelope.chrome.theme,
                            current.peek().chrome.theme,
                        );
                        if let Some(scroll) = scroll {
                            pending_scroll.set(Some(scroll));
                        }
                        current.set(envelope);
                        if location != request_target.split('#').next().unwrap_or(&request_target) {
                            adopted.set(Some((location.clone(), epoch)));
                            router.replace(location);
                        }
                    }
                    Ok(load::PageLoad::Navigate(location)) => {
                        if let Some(window) = web_sys::window() {
                            let _ = window.location().assign(&location);
                        }
                    }
                    Err(error) => failure.set(Some(error.to_string())),
                }
            });
            *active.borrow_mut() = Some(InFlight {
                target,
                controller,
                task,
            });
        });
        use_effect(move || {
            let page = current.read();
            if let Some([x, y]) = pending_scroll.write().take() {
                if let Some(window) = web_sys::window() {
                    window.scroll_to_with_x_and_y(x, y);
                }
            }
            if let Some(document) = web_sys::window().and_then(|w| w.document()) {
                document.set_title(&crate::webui::app::tab_title(&page.title));
                if let Some(html) = document.document_element() {
                    html.set_class_name(page.chrome.theme.as_str());
                }
                set_theme_color(&document, page.chrome.theme);
            }
        });
    }
    #[cfg(not(target_arch = "wasm32"))]
    let _ = (refresh, appearance);
    let _ = current;
    rsx! {
        if loading() {
            div { role: "status", class: "page-navigation-progress",
                span { class: "sr-only", "Loading page…" }
            }
        }
        if let Some(message) = failure() {
            div { role: "alert", class: "page-navigation-error",
                p { "{message}" }
                a { href: target.clone(), class: "text-primary hover:underline", "Reload this page" }
            }
        }
        CurrentPageView {}
    }
}

#[cfg(any(target_arch = "wasm32", test))]
fn adopts_loaded_location(adopted: Option<&(String, u64)>, target: &str, epoch: u64) -> bool {
    adopted.is_some_and(|(location, accepted_epoch)| location == target && *accepted_epoch == epoch)
}

// Content search has a 35-second server deadline; leave room for response transfer.
#[cfg(any(target_arch = "wasm32", test))]
const MODEL_TIMEOUT_MS: u32 = 45_000;

#[cfg(target_arch = "wasm32")]
fn history_scroll() -> [f64; 2] {
    let state = web_sys::window()
        .and_then(|window| window.history().ok())
        .and_then(|history| history.state().ok());
    let value = state.map(|state| js_sys::Array::from(&state));
    value
        .map(|state| {
            [
                state.get(0).as_f64().unwrap_or(0.0),
                state.get(1).as_f64().unwrap_or(0.0),
            ]
        })
        .unwrap_or([0.0, 0.0])
}

/// Loading and failure banners do not clone or diff the current page payload.
#[component]
fn CurrentPageView() -> Element {
    let CurrentPage(current) = use_context();
    let envelope = current.read().clone();
    super::app::page_tree(envelope)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn canonical_adoption_skips_only_the_duplicate_route_read_not_a_refresh() {
        let adopted = ("/a/r/tree/main".to_owned(), 3);
        assert!(adopts_loaded_location(Some(&adopted), "/a/r/tree/main", 3));
        assert!(!adopts_loaded_location(Some(&adopted), "/a/r/tree/main", 4));
        assert!(!adopts_loaded_location(Some(&adopted), "/help", 3));
        assert!(!adopts_loaded_location(None, "/a/r/tree/main", 3));
    }

    #[test]
    fn navigation_allows_the_server_content_search_deadline_and_transfer() {
        const { assert!(MODEL_TIMEOUT_MS > 35_000 + 5_000) };
    }

    #[test]
    fn slow_same_selection_polls_are_coalesced_but_navigation_replaces_the_request() {
        let active = "/alice/repo/actions?job=10";
        assert!(should_start_read(None, active));
        for _ in 0..1_000 {
            assert!(!should_start_read(Some(active), active));
        }
        assert!(should_start_read(
            Some(active),
            "/alice/repo/actions?job=11"
        ));
        assert!(should_start_read(Some(active), "/settings"));
        assert!(should_start_read(None, active)); // completion/timeout releases the read slot
    }

    #[test]
    fn history_intent_rejects_a_ready_response_before_the_next_effect() {
        assert!(may_accept_read(
            7,
            7,
            "/alice/repo/actions?job=a",
            "/alice/repo/actions?job=a"
        ));
        assert!(!may_accept_read(
            7,
            7,
            "/alice/repo/actions?job=a",
            "/alice/repo/actions?job=b"
        ));
        assert!(!may_accept_read(7, 8, "/settings", "/settings"));
    }

    /// The opening tag of the first anchor that contains `needle`.
    fn anchor_tag<'a>(html: &'a str, needle: &str) -> &'a str {
        let at = html
            .find(needle)
            .unwrap_or_else(|| panic!("no {needle} in {html}"));
        let start = html[..at].rfind('<').unwrap();
        let end = at + html[at..].find('>').unwrap();
        &html[start..=end]
    }

    #[test]
    fn current_link_is_marked_once_and_the_logo_is_never_marked() {
        let page = crate::webui::app::PageEnvelope {
            title: "Explore".into(),
            location: "/explore".into(),
            chrome: crate::webui::document::Chrome::default(),
            page: crate::webui::app::Page::Error {
                status: 404,
                message: "Not found".into(),
                repo: None,
                signed_in: false,
            },
        };
        let html = crate::webui::document::render_model(page, "/assets/app.js").unwrap();
        let explore = anchor_tag(&html, "href=\"/explore\"");
        assert_eq!(explore.matches("aria-current").count(), 1, "{explore}");
        assert!(explore.contains("aria-current=\"page\""), "{explore}");
        let logo = anchor_tag(&html, "aria-label=\"Home\"");
        assert!(!logo.contains("aria-current"), "{logo}");
    }

    #[test]
    fn route_preserves_queries_and_slash_refs() {
        for url in [
            "/",
            "/search?q=rust&page=2",
            "/search?q=a%26b%2Bc%3Dd",
            "/alice/repo/tree/feature%2Fbranch?tab=files",
            "/alice/repo/issues/1#comment-4",
        ] {
            let route: PageRoute = url.parse().expect("valid route");
            assert_eq!(route.to_string(), url);
        }
        assert!(!safe_page_target("//evil.test"));
        assert!(!safe_page_target("/%2fevil.test"));
        assert!(!safe_page_target("/\\evil.test"));
        assert!(!safe_page_target("/%5cevil.test"));
        assert!(safe_page_target("/search?q=a%5Cb"));
    }
}
