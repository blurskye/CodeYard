use super::{Avatar, DraftInput, ThemeForm};
use crate::webui::{
    decode_component,
    document::Chrome,
    encode_component,
    navigation::NavLink,
    pages::{
        discovery::search_tab_url,
        repos::{
            chrome::{icon as repo_icon, repo_badges, repo_tabs, RepoChrome},
            ref_path::encode_path,
        },
    },
};
use dioxus::prelude::*;

/// `repo` is set on repository pages; its tabs then sit in the header band.
pub fn shell(chrome: Chrome, repo: Option<RepoChrome>, body: Element) -> Element {
    shell_at(chrome, repo, true, body)
}

/// `shell` for a page whose address may name nothing, such as a 404: with `located` false the
/// header does not turn the address into an owner / repo crumb.
pub fn shell_at(chrome: Chrome, repo: Option<RepoChrome>, located: bool, body: Element) -> Element {
    rsx! {
        a { href: "#main", class: "skip-link", "Skip to content" }
        SiteHeader { chrome, repo, located }
        // Keep the prebuilt keyed PageBody intact: shell presentation must not remount drafts.
        main { id: "main", class: "site-main", tabindex: "-1", {body} }
    }
}

/// The unread count as the header shows it: the number, or "99+" past 99.
fn unread_badge(count: i64) -> String {
    if count > 99 {
        "99+".to_owned()
    } else {
        count.to_string()
    }
}

#[component]
fn SiteHeader(chrome: Chrome, repo: Option<RepoChrome>, located: bool) -> Element {
    // The header stays mounted while the body changes between pages, so one listener serves them
    // all. It looks up the Go to file box again on each key press.
    #[cfg(target_arch = "wasm32")]
    {
        let shortcut = use_hook(|| std::rc::Rc::new(std::cell::RefCell::new(None::<KeyShortcuts>)));
        let installed = shortcut.clone();
        use_effect(move || *installed.borrow_mut() = KeyShortcuts::install());
        use_drop(move || {
            shortcut.borrow_mut().take();
        });
    }
    // The header shows the unread count of a signed-in page, and nothing otherwise.
    let unread = chrome.unread_notifications.filter(|count| *count > 0);
    let badge = unread.map(unread_badge);
    let token = chrome.csrf_token;
    let return_to = chrome.return_to;
    // The phone search sheet offers a search of this repository by its owner/name.
    let search_repo = repo
        .as_ref()
        .map(|repo| format!("{}/{}", repo.owner, repo.name));
    // ...and a row that opens the file finder of that repository.
    let find_files = repo
        .as_ref()
        .map(|repo| format!("{}/find/{}", repo.base(), encode_path(&repo.default_branch)));
    rsx! {
        header { class: "site-header",
            div { class: if repo.is_some() { "site-header-row site-header-row-repo" } else { "site-header-row" },
                details { id: "site-navigation-menu", class: "native-menu site-navigation", "name": "site-header-menu",
                    // While the menu is open, a phone shows the close icon in the same place.
                    summary { class: "site-icon-button icon-tooltip icon-tooltip-start", "aria-label": if unread.is_some() { "Open main navigation, unread notifications" } else { "Open main navigation" },
                        span { class: "site-nav-open-icon", {shell_icon(ShellIcon::Menu)} }
                        span { class: "site-nav-close-icon", {shell_icon(ShellIcon::Close)} }
                        if unread.is_some() {
                            span { class: "site-menu-dot", "aria-hidden": "true" }
                        }
                    }
                    nav { class: "native-menu-panel site-menu-links", "aria-label": "Main navigation",
                        p { class: "site-menu-heading", "Navigation" }
                        NavLink { href: "/".to_string(), class: "site-menu-link".to_string(), "Home" }
                        NavLink { href: "/explore".to_string(), class: "site-menu-link".to_string(), "Explore" }
                        if let Some(username) = chrome.username.as_ref() {
                            NavLink { href: format!("/{}?tab=repositories", encode_component(username)), class: "site-menu-link".to_string(), "Your repositories" }
                            NavLink { href: "/new".to_string(), class: "site-menu-link".to_string(), "New repository" }
                            // The inbox is an icon in the header on wider screens. A phone finds it here.
                            NavLink { href: "/notifications".to_string(), class: "site-menu-link site-menu-phone".to_string(), aria_label: unread.map(|count| format!("Notifications, {} unread", unread_badge(count))),
                                {shell_icon(ShellIcon::Inbox)} "Notifications"
                                if let Some(text) = &badge {
                                    span { class: "site-count-badge site-menu-count", "aria-hidden": "true", "{text}" }
                                }
                            }
                        } else {
                            NavLink { href: "/login".to_string(), class: "site-menu-link".to_string(), "Sign in" }
                            NavLink { href: "/register".to_string(), class: "site-menu-link".to_string(), "Sign up" }
                        }
                        NavLink { href: "/help".to_string(), class: "site-menu-link".to_string(), "Help" }
                        // The theme button is an icon in the header on wider screens. A phone finds it here.
                        if !token.is_empty() {
                            ThemeForm { csrf_token: token.clone(), return_to: return_to.clone(), theme: chrome.theme, menu: true }
                        }
                    }
                }
                NavLink { href: "/".to_string(), class: "site-brand".to_string(), aria_label: Some("Home".to_string()), mark_current: false,
                                        svg { class: "site-brand-mark", view_box: "0 0 256 256", width: "32", height: "32", "aria-hidden": "true",
                        // The mark without its background square, so it sits on the header in either theme.
                        g { fill: "none", stroke: "currentColor", stroke_width: "18", stroke_linecap: "round", stroke_linejoin: "round",
                            path { d: "M108 60L128 40L148 60" } path { d: "M196 108L216 128L196 148" } path { d: "M108 196L128 216L148 196" } path { d: "M60 108L40 128L60 148" }
                            path { d: "M62 62L84 84" } path { d: "M194 62L172 84" } path { d: "M62 194L84 172" } path { d: "M194 194L172 172" }
                        }
                        circle { cx: "128", cy: "128", r: "30", fill: "#5eead4" }
                    }
                }
                HeaderContext { path: return_to.clone(), located, signed_in: chrome.username.is_some(), repo: repo.clone() }
                div { class: "site-header-spacer" }
                SiteSearch { id: "site-search-query", class: "site-search site-search-wide", shortcut: true }
                // On a phone the search is a button. Its summary turns into the close button while the
                // full-screen sheet is open.
                details { class: "site-search-disclosure native-menu",
                    ontoggle: move |_| {
                        // Opening the sheet puts the cursor in its box. Closing it, by the close button
                        // or Escape, returns focus to the summary that opened it.
                        #[cfg(target_arch = "wasm32")]
                        {
                            if let Some(document) = web_sys::window().and_then(|window| window.document()) {
                                let open = document.query_selector(".site-search-disclosure[open]").ok().flatten().is_some();
                                if open {
                                    focus_if_shown(&document, "site-search-sheet-query");
                                } else {
                                    focus_if_shown(&document, "site-search-trigger");
                                }
                            }
                        }
                    },
                    summary { id: "site-search-trigger", class: "site-icon-button icon-tooltip", "aria-label": "Search repositories",
                        span { class: "site-search-open-icon", {shell_icon(ShellIcon::Search)} }
                        span { class: "site-search-close-icon", {shell_icon(ShellIcon::Close)} }
                    }
                    SearchSheet { repo: search_repo, find_files }
                }
                div { class: "site-header-actions",
                    if let Some(username) = chrome.username.as_ref() {
                        // A thin rule sets the search apart from the account controls. Phones
                        // hide it along with the search.
                        span { class: "site-header-divider", "aria-hidden": "true" }
                        details { class: "native-menu site-create-menu", "name": "site-header-menu",
                            summary { class: "site-icon-button icon-tooltip", "aria-label": "Create new",
                                {shell_icon(ShellIcon::Plus)}
                                {shell_icon(ShellIcon::ChevronDown)}
                            }
                            div { class: "native-menu-panel",
                                div { class: "site-menu-links",
                                    // Writers of the repository on screen can add a file from here too.
                                    if let Some(repo) = repo.as_ref().filter(|repo| repo.can_write) {
                                        NavLink { href: format!("{}/new/{}", repo.base(), encode_path(&repo.default_branch)), class: "site-menu-link".to_string(),
                                            {repo_icon("file")} "Create new file"
                                        }
                                        NavLink { href: format!("{}/upload/{}", repo.base(), encode_path(&repo.default_branch)), class: "site-menu-link".to_string(),
                                            {repo_icon("upload")} "Upload files"
                                        }
                                        hr { class: "site-menu-divider" }
                                    }
                                    NavLink { href: "/new".to_string(), class: "site-menu-link".to_string(),
                                        {shell_icon(ShellIcon::Repository)} "New repository"
                                    }
                                    NavLink { href: "/issues/new".to_string(), class: "site-menu-link".to_string(),
                                        {shell_icon(ShellIcon::Issue)} "New issue"
                                    }
                                    NavLink { href: "/settings/organizations".to_string(), class: "site-menu-link".to_string(),
                                        {shell_icon(ShellIcon::Organization)} "New organization"
                                    }
                                }
                            }
                        }
                        // The three links after the create menu are hidden on phones (native.css). The
                        // account menu lists the same destinations there.
                        NavLink { href: "/issues".to_string(), class: "site-icon-button site-header-wide icon-tooltip".to_string(), aria_label: Some("Issues".to_string()),
                            {shell_icon(ShellIcon::Issue)}
                        }
                        NavLink { href: "/pulls".to_string(), class: "site-icon-button site-header-wide icon-tooltip".to_string(), aria_label: Some("Pull requests".to_string()),
                            {shell_icon(ShellIcon::PullRequest)}
                        }
                        NavLink { href: format!("/{}?tab=repositories", encode_component(username)), class: "site-icon-button site-header-wide icon-tooltip".to_string(), aria_label: Some("Repositories".to_string()),
                            {shell_icon(ShellIcon::Repository)}
                        }
                        NavLink { href: "/notifications".to_string(), class: "site-icon-button site-header-wide icon-tooltip".to_string(), aria_label: Some(match unread {
                            Some(count) => format!("Notifications, {} unread", unread_badge(count)),
                            None => "Notifications".to_owned(),
                        }),
                            {shell_icon(ShellIcon::Inbox)}
                            if let Some(text) = &badge {
                                span { class: "site-count-badge", "aria-hidden": "true", "{text}" }
                            }
                        }
                    }
                    if !token.is_empty() {
                        ThemeForm { csrf_token: token.clone(), return_to, theme: chrome.theme }
                    }
                    if let Some(username) = chrome.username {
                        details { id: "site-account-menu", class: "native-menu site-account-menu", "name": "site-header-menu",
                            summary { class: "site-profile-button icon-tooltip icon-tooltip-end", "aria-label": format!("Account menu for {username}"),
                                // The picture is found by name, as for every account, so Chrome needs no avatar URL.
                                Avatar { name: username.clone(), size: 26 }
                            }
                            div { class: "native-menu-panel",
                                p { class: "site-menu-heading",
                                    span { "Signed in as" }
                                    strong { "{username}" }
                                }
                                div { class: "site-menu-links",
                                    NavLink { href: format!("/{}", encode_component(&username)), class: "site-menu-link".to_string(), {shell_icon(ShellIcon::User)} "Your profile" }
                                    NavLink { href: format!("/{}?tab=repositories", encode_component(&username)), class: "site-menu-link".to_string(), {shell_icon(ShellIcon::Repository)} "Your repositories" }
                                    NavLink { href: "/stars".to_string(), class: "site-menu-link".to_string(), {shell_icon(ShellIcon::Star)} "Your stars" }
                                    NavLink { href: "/issues".to_string(), class: "site-menu-link".to_string(), {shell_icon(ShellIcon::Issue)} "Your issues" }
                                    NavLink { href: "/pulls".to_string(), class: "site-menu-link".to_string(), {shell_icon(ShellIcon::PullRequest)} "Your pull requests" }
                                    NavLink { href: "/settings/organizations".to_string(), class: "site-menu-link".to_string(), {shell_icon(ShellIcon::Organization)} "Organizations" }
                                    hr { class: "site-menu-divider" }
                                    NavLink { href: "/settings".to_string(), class: "site-menu-link".to_string(), {shell_icon(ShellIcon::Settings)} "Settings" }
                                    NavLink { href: "/help".to_string(), class: "site-menu-link".to_string(), {shell_icon(ShellIcon::Help)} "Help" }
                                    if chrome.is_admin { NavLink { href: "/admin".to_string(), class: "site-menu-link".to_string(), {shell_icon(ShellIcon::Shield)} "Administration" } }
                                    form { action: "/logout", method: "post", class: "site-signout-form",
                                        input { r#type: "hidden", name: "csrf_token", value: token.clone() }
                                        button { r#type: "submit", class: "site-menu-link", {shell_icon(ShellIcon::LogOut)} "Sign out" }
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
            if let Some(repo) = &repo {
                {repo_tabs(repo)}
            }
        }
    }
}

/// Where the visitor is, beside the logo, as GitHub does: "Dashboard", "Issues" or
/// "owner / repo". The product name is not repeated in the header. On a repository page the
/// names come from the repository, and the visibility badges follow the name.
#[component]
fn HeaderContext(
    path: String,
    located: bool,
    signed_in: bool,
    repo: Option<RepoChrome>,
) -> Element {
    if let Some(repo) = repo {
        // The repository name is the page heading.
        return rsx! {
            nav { class: "site-context", "aria-label": "Current location",
                NavLink { href: format!("/{}", encode_component(&repo.owner)), class: "site-context-link".to_string(), "{repo.owner}" }
                span { class: "site-context-slash", "aria-hidden": "true", "/" }
                h1 { class: "site-context-heading",
                    NavLink { href: repo.base(), class: "site-context-link site-context-repo".to_string(), "{repo.name}" }
                }
                {repo_badges(&repo)}
            }
        };
    }
    if !located {
        return rsx! {};
    }
    let path = path.split(['?', '#']).next().unwrap_or("");
    let mut parts = path.trim_matches('/').split('/');
    let (first, second) = (parts.next().unwrap_or(""), parts.next());
    let page = match first {
        "" if signed_in => Some("Dashboard"),
        "explore" => Some("Explore"),
        "issues" => Some("Issues"),
        "pulls" => Some("Pull requests"),
        "notifications" => Some("Notifications"),
        "settings" => Some("Settings"),
        "admin" => Some("Administration"),
        "new" => Some("New repository"),
        "search" => Some("Search"),
        "ci" => Some("Actions"),
        "stars" => Some("Stars"),
        "help" => Some("Help"),
        _ => None,
    };
    if let Some(page) = page {
        return rsx! { span { class: "site-context", "{page}" } };
    }
    // Sign-in pages and the demo pages have no location. `/orgs/:org/teams/:team` is the one
    // address under `orgs`; anything else there names nothing.
    if first.is_empty()
        || matches!(
            first,
            "login"
                | "register"
                | "enter"
                | "logout"
                | "setup"
                | "forgot-password"
                | "reset-password"
                | "verify-email"
                | "demo"
        )
    {
        return rsx! {};
    }
    if first == "orgs" {
        let (Some(org), Some("teams"), Some(team)) = (second, parts.next(), parts.next()) else {
            return rsx! {};
        };
        let (org, team) = (decode_component(org), decode_component(team));
        let org_href = format!("/{}", encode_component(&org));
        return rsx! {
            nav { class: "site-context", "aria-label": "Current location",
                NavLink { href: org_href.clone(), class: "site-context-link".to_string(), "{org}" }
                span { class: "site-context-slash", "aria-hidden": "true", "/" }
                NavLink { href: format!("{org_href}?tab=teams"), class: "site-context-link".to_string(), "teams" }
                span { class: "site-context-slash", "aria-hidden": "true", "/" }
                NavLink { href: format!("/orgs/{}/teams/{}", encode_component(&org), encode_component(&team)), class: "site-context-link site-context-repo".to_string(), "{team}" }
            }
        };
    }
    // The path is already percent-encoded; decode it to show the names and encode them once.
    let first = decode_component(first);
    let second = second.map(decode_component);
    let owner = encode_component(&first);
    rsx! {
        nav { class: "site-context", "aria-label": "Current location",
            NavLink { href: format!("/{owner}"), class: "site-context-link".to_string(), "{first}" }
            if let Some(name) = second {
                span { class: "site-context-slash", "aria-hidden": "true", "/" }
                NavLink { href: format!("/{owner}/{}", encode_component(&name)), class: "site-context-link site-context-repo".to_string(), "{name}" }
            }
        }
    }
}

/// The site search form. The header search (`shortcut`) is the box the "/" key focuses: its
/// placeholder names the key and a hint shows it. The phone search and the error page keep their
/// plain placeholder.
#[component]
pub fn SiteSearch(id: String, class: String, #[props(default)] shortcut: bool) -> Element {
    let placeholder = if shortcut {
        "Type / to search"
    } else {
        "Search repositories…"
    };
    rsx! {
        form { action: "/search", method: "get", class, role: "search", "aria-label": "Search repositories",
            label { r#for: id, class: "site-sr-only", "Search repositories" }
            DraftInput { id: Some(id.clone()), name: "q".to_string(), initial_value: String::new(), input_type: "search".to_string(), placeholder: placeholder.to_string(), class: "site-search-input".to_string() }
            if shortcut {
                span { class: "site-search-key", "aria-hidden": "true", "/" }
            }
            button { r#type: "submit", class: "site-search-submit icon-tooltip icon-tooltip-start", "aria-label": "Search repositories",
                {shell_icon(ShellIcon::Search)}
            }
        }
    }
}

/// The phone search sheet: a full-screen form, opened by the header's search button. Submitting it is
/// a plain GET /search?q=. Once the page runs in the browser, typed text adds rows that search for it
/// in one place or kind of result. Without scripts there are no rows, and the form still searches.
#[component]
fn SearchSheet(repo: Option<String>, find_files: Option<String>) -> Element {
    let mut typed = use_signal(String::new);
    let rows = search_scopes(&typed(), repo.as_deref());
    // Before anything is typed, a hint says what the search covers, since no rows show yet.
    let hint = match repo.as_deref() {
        Some(repo) => {
            format!("Search {repo}, all repositories, issues, pull requests, code or people.")
        }
        None => "Search repositories, issues, pull requests, code or people.".to_owned(),
    };
    rsx! {
        div { class: "site-search-sheet",
            form { class: "site-search-sheet-form", action: "/search", method: "get", role: "search", "aria-label": "Search repositories",
                label { r#for: "site-search-sheet-query", class: "site-sr-only", "Search repositories" }
                DraftInput { id: Some("site-search-sheet-query".to_string()), name: "q".to_string(), initial_value: String::new(), input_type: "search".to_string(), placeholder: "Search repositories…".to_string(), class: "site-search-sheet-input".to_string(),
                    on_value: move |next: String| typed.set(next) }
                button { r#type: "submit", class: "site-search-sheet-submit", "aria-label": "Search repositories", {shell_icon(ShellIcon::Search)} }
            }
            if rows.is_empty() {
                p { class: "site-search-hint", "{hint}" }
            }
            if !rows.is_empty() || find_files.is_some() {
                nav { class: "site-search-scopes", "aria-label": "Search scopes",
                    if let Some(href) = find_files {
                        NavLink { href: href, class: "site-menu-link".to_string(), {repo_icon("file")} span { "Go to file in this repository" } }
                    }
                    for (label, href, icon, scope) in rows {
                        NavLink { key: "{href}", href: href, class: "site-menu-link".to_string(), {shell_icon(icon)}
                            span { class: "site-search-row-text",
                                span { "{label}" }
                                if let Some(scope) = scope { span { class: "site-search-scope", "{scope}" } }
                            }
                        }
                    }
                }
            }
        }
    }
}

/// One row under the search sheet: its label, the link that searches, its icon, and the scope the
/// search covers when the row's label does not say it.
type ScopeRow = (String, String, ShellIcon, Option<&'static str>);

/// The rows under the search sheet, one per place or kind of result, each with the link that searches
/// for `text`. Empty while nothing is typed. "In this repository" adds the `repo:owner/name` word that
/// the search page reads, and searches code, the result kind a repository has. Inside a repository the
/// kinds below it search every repository, and their rows say so.
fn search_scopes(text: &str, repo: Option<&str>) -> Vec<ScopeRow> {
    let text = text.trim();
    if text.is_empty() {
        return Vec::new();
    }
    let kind_scope = repo.map(|_| "All repositories");
    let mut rows = Vec::new();
    if let Some(repo) = repo {
        rows.push((
            format!("Search for '{text}' in this repository"),
            search_tab_url(&format!("{text} repo:{repo}"), "code"),
            ShellIcon::Code,
            None,
        ));
    }
    rows.push((
        format!("Search for '{text}' in all repositories"),
        search_tab_url(text, "repos"),
        ShellIcon::Repository,
        None,
    ));
    rows.push((
        "Issues".to_owned(),
        search_tab_url(text, "issues"),
        ShellIcon::Issue,
        kind_scope,
    ));
    rows.push((
        "Pull requests".to_owned(),
        search_tab_url(text, "pulls"),
        ShellIcon::PullRequest,
        kind_scope,
    ));
    rows.push((
        "Code".to_owned(),
        search_tab_url(text, "code"),
        ShellIcon::Code,
        kind_scope,
    ));
    rows.push((
        "People".to_owned(),
        search_tab_url(text, "users"),
        ShellIcon::User,
        kind_scope,
    ));
    rows
}

/// The "/" key focuses the Go to file box on a repository page that shows its Files panel, and the
/// header search everywhere else. The "?" key opens the help page. A key typed into a text field
/// keeps its meaning. The listener is removed when the header is dropped.
#[cfg(target_arch = "wasm32")]
struct KeyShortcuts(wasm_bindgen::closure::Closure<dyn FnMut(web_sys::KeyboardEvent)>);

#[cfg(target_arch = "wasm32")]
impl KeyShortcuts {
    fn install() -> Option<Self> {
        use wasm_bindgen::JsCast;
        let document = web_sys::window()?.document()?;
        let on_key = wasm_bindgen::closure::Closure::<dyn FnMut(web_sys::KeyboardEvent)>::new(
            |event: web_sys::KeyboardEvent| {
                let key = event.key();
                if (key != "/" && key != "?")
                    || event.ctrl_key()
                    || event.alt_key()
                    || event.meta_key()
                {
                    return;
                }
                let Some(window) = web_sys::window() else {
                    return;
                };
                let Some(document) = window.document() else {
                    return;
                };
                let typing = document.active_element().is_some_and(|element| {
                    matches!(element.tag_name().as_str(), "INPUT" | "TEXTAREA" | "SELECT")
                });
                if typing {
                    return;
                }
                if key == "?" {
                    let _ = window.location().assign("/help");
                    return;
                }
                // A hidden box takes no focus (phone width, or a collapsed tree), so the key then
                // reaches the header search.
                let focused = ["repo-files-filter", "site-search-query"]
                    .into_iter()
                    .any(|id| focus_if_shown(&document, id));
                if focused {
                    event.prevent_default();
                }
            },
        );
        document
            .add_event_listener_with_callback("keydown", on_key.as_ref().unchecked_ref())
            .ok()?;
        Some(Self(on_key))
    }
}

#[cfg(target_arch = "wasm32")]
impl Drop for KeyShortcuts {
    fn drop(&mut self) {
        use wasm_bindgen::JsCast;
        if let Some(document) = web_sys::window().and_then(|window| window.document()) {
            let _ = document
                .remove_event_listener_with_callback("keydown", self.0.as_ref().unchecked_ref());
        }
    }
}

/// Focuses the element with `id` and reports whether focus actually moved there.
#[cfg(target_arch = "wasm32")]
fn focus_if_shown(document: &web_sys::Document, id: &str) -> bool {
    use wasm_bindgen::JsCast;
    let Some(field) = document
        .get_element_by_id(id)
        .and_then(|element| element.dyn_into::<web_sys::HtmlElement>().ok())
    else {
        return false;
    };
    let _ = field.focus();
    document
        .active_element()
        .is_some_and(|active| active.id() == id)
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
    ChevronDown,
    Inbox,
    Issue,
    PullRequest,
    User,
    Sun,
    Moon,
    Monitor,
    Repository,
    Organization,
    Settings,
    Shield,
    Star,
    Help,
    LogOut,
    Close,
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
        ShellIcon::ChevronDown => rsx! {
            path { d: "m6 9 6 6 6-6" }
        },
        ShellIcon::Inbox => rsx! {
            path { d: "M22 12h-6l-2 3h-4l-2-3H2" }
            path { d: "M5.45 5.11 2 12v6a2 2 0 0 0 2 2h16a2 2 0 0 0 2-2v-6l-3.45-6.89A2 2 0 0 0 16.76 4H7.24a2 2 0 0 0-1.79 1.11z" }
        },
        ShellIcon::Issue => rsx! {
            circle { cx: "12", cy: "12", r: "10" }
            circle { cx: "12", cy: "12", r: "1" }
        },
        ShellIcon::PullRequest => rsx! {
            circle { cx: "18", cy: "18", r: "3" }
            circle { cx: "6", cy: "6", r: "3" }
            path { d: "M13 6h3a2 2 0 0 1 2 2v7" }
            path { d: "M6 9v12" }
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
        ShellIcon::Repository => rsx! {
            path { d: "M4 19.5v-15A2.5 2.5 0 0 1 6.5 2H20v20H6.5a2.5 2.5 0 0 1 0-5H20" }
        },
        ShellIcon::Organization => rsx! {
            path { d: "M6 22V4a2 2 0 0 1 2-2h8a2 2 0 0 1 2 2v18Z" }
            path { d: "M6 12H4a2 2 0 0 0-2 2v6a2 2 0 0 0 2 2h2" }
            path { d: "M18 9h2a2 2 0 0 1 2 2v9a2 2 0 0 1-2 2h-2" }
            path { d: "M10 6h4M10 10h4M10 14h4M10 18h4" }
        },
        ShellIcon::Settings => rsx! {
            path { d: "M21 4h-7M10 4H3M21 12h-9M8 12H3M21 20h-5M12 20H3M14 2v4M8 10v4M16 18v4" }
        },
        ShellIcon::Shield => rsx! {
            path { d: "M20 13c0 5-3.5 7.5-7.66 8.95a1 1 0 0 1-.67-.01C7.5 20.5 4 18 4 13V6a1 1 0 0 1 1-1c2 0 4.5-1.2 6.24-2.72a1.17 1.17 0 0 1 1.52 0C14.51 3.81 17 5 19 5a1 1 0 0 1 1 1z" }
        },
        ShellIcon::Star => rsx! {
            path { d: "M11.525 2.295a.53.53 0 0 1 .95 0l2.31 4.679a2.123 2.123 0 0 0 1.595 1.16l5.166.756a.53.53 0 0 1 .294.904l-3.736 3.638a2.123 2.123 0 0 0-.611 1.878l.882 5.14a.53.53 0 0 1-.771.56l-4.618-2.428a2.122 2.122 0 0 0-1.973 0L6.396 21.01a.53.53 0 0 1-.77-.56l.881-5.139a2.122 2.122 0 0 0-.611-1.879L2.16 9.795a.53.53 0 0 1 .294-.906l5.165-.755a2.122 2.122 0 0 0 1.597-1.16z" }
        },
        ShellIcon::Help => rsx! {
            circle { cx: "12", cy: "12", r: "10" }
            path { d: "M9.09 9a3 3 0 0 1 5.83 1c0 2-3 3-3 3" }
            path { d: "M12 17h.01" }
        },
        ShellIcon::LogOut => rsx! {
            path { d: "M9 21H5a2 2 0 0 1-2-2V5a2 2 0 0 1 2-2h4" }
            path { d: "m16 17 5-5-5-5" }
            path { d: "M21 12H9" }
        },
        ShellIcon::Close => rsx! {
            path { d: "M18 6 6 18M6 6l12 12" }
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
    fn header_badges_show_the_unread_count_and_cap_at_99_plus() {
        fn signed_in(unread: Option<i64>) -> Chrome {
            Chrome {
                username: Some("alice".into()),
                csrf_token: "owned-csrf-token".into(),
                return_to: "/".into(),
                unread_notifications: unread,
                ..Default::default()
            }
        }
        let render =
            |chrome: Chrome| dioxus_ssr::render_element(shell(chrome, None, rsx! { p { "Body" } }));
        let some = render(signed_in(Some(3)));
        assert!(some.contains("class=\"site-count-badge\""), "{some}");
        assert!(some.contains(">3</span>"), "{some}");
        assert!(some.contains("aria-label=\"Notifications, 3 unread\""));
        assert!(some.contains("class=\"site-menu-dot\""));
        assert!(some.contains("aria-label=\"Open main navigation, unread notifications\""));
        let capped = render(signed_in(Some(250)));
        assert!(capped.contains(">99+</span>"), "{capped}");
        assert!(capped.contains("aria-label=\"Notifications, 99+ unread\""));
        for quiet in [render(signed_in(Some(0))), render(signed_in(None))] {
            assert!(!quiet.contains("class=\"site-count-badge\""));
            assert!(!quiet.contains("class=\"site-menu-dot\""));
            assert!(quiet.contains("aria-label=\"Notifications\""));
        }
    }

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
            None,
            rsx! { section { id: "shell-owner-body", "Body from its feature owner" } },
        ));
        for required in [
            "class=\"site-brand-mark\"",
            "src=\"/avatars/alice\"",
            "class=\"site-profile-button icon-tooltip icon-tooltip-end\"",
            "aria-label=\"Notifications\"",
            "aria-label=\"Create new\"",
            "aria-label=\"Issues\"",
            "aria-label=\"Pull requests\"",
            "aria-label=\"Repositories\"",
            "href=\"/issues\"",
            "href=\"/pulls\"",
            "href=\"/new\"",
            "href=\"/issues/new\"",
            "aria-label=\"Account menu for alice\"",
            "action=\"/search\"",
            "action=\"/logout\"",
            "action=\"/_forms/theme\"",
            "name=\"csrf_token\"",
            "value=\"owned-csrf-token\"",
            "href=\"/admin\"",
            "href=\"/alice?tab=repositories\"",
            "id=\"shell-owner-body\"",
            "aria-label=\"Home\"",
            "site-context",
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
            None,
            rsx! { p { "Body" } },
        ));
        // The search form is shown in the wide header and again inside the phone disclosure,
        // whose summary is the icon button that opens it.
        assert_eq!(html.matches("action=\"/search\"").count(), 2);
        let disclosure = html
            .split_once("class=\"site-search-disclosure native-menu\"")
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
        assert!(
            disclosure.contains("class=\"site-search-sheet\""),
            "{disclosure}"
        );
        assert!(
            html.contains("class=\"site-search site-search-wide\""),
            "{html}"
        );
        // The phone row keeps the header actions after the disclosure.
        assert!(html.find("site-search-disclosure") < html.find("site-header-actions"));
    }

    #[test]
    fn signed_in_header_actions_keep_their_order_and_the_phone_hides_four() {
        let html = dioxus_ssr::render_element(shell(
            Chrome {
                username: Some("alice".into()),
                csrf_token: "token".into(),
                ..Default::default()
            },
            None,
            rsx! { p { "Body" } },
        ));
        // Only the create menu and the avatar stay on a phone; the other four carry this class.
        assert_eq!(html.matches("site-header-wide").count(), 4, "{html}");
        let order = [
            "aria-label=\"Create new\"",
            "aria-label=\"Issues\"",
            "aria-label=\"Pull requests\"",
            "aria-label=\"Repositories\"",
            "aria-label=\"Notifications\"",
            "aria-label=\"Account menu for alice\"",
        ];
        let positions: Vec<usize> = order
            .iter()
            .map(|marker| {
                html.find(marker)
                    .unwrap_or_else(|| panic!("{marker} missing: {html}"))
            })
            .collect();
        assert!(positions.windows(2).all(|pair| pair[0] < pair[1]), "{html}");
    }

    #[test]
    fn repository_header_names_the_repo_and_draws_its_badge_and_engagement() {
        let repo = RepoChrome {
            owner: "alice".into(),
            name: "project".into(),
            is_private: true,
            signed_in: true,
            csrf_token: "csrf-token".into(),
            stars: 5,
            ..Default::default()
        };
        let html = dioxus_ssr::render_element(shell(
            Chrome {
                username: Some("alice".into()),
                csrf_token: "csrf-token".into(),
                return_to: "/alice/project".into(),
                ..Default::default()
            },
            Some(repo),
            rsx! { p { "Body" } },
        ));
        // The name is the only heading in the band, and the visibility badge follows it.
        assert_eq!(html.matches("<h1").count(), 1, "{html}");
        let heading_end = html.find("</h1>").unwrap();
        let badge = html.find("repo-visibility-text\">Private</span>").unwrap();
        assert!(heading_end < badge, "{html}");
        // Watch, Fork and Star end the tab row, below the create menu's row.
        let engagement = html.find("class=\"repo-engagement ").unwrap();
        assert!(
            html.find("aria-label=\"Create new\"").unwrap() < engagement,
            "{html}"
        );
        assert!(
            html.find("class=\"repo-tabs\"").unwrap() < engagement,
            "{html}"
        );
        assert!(engagement < html.find("<main").unwrap(), "{html}");
        assert!(
            html.contains("action=\"/alice/project/_forms/star\""),
            "{html}"
        );
        assert!(!html.contains("repo-title"), "{html}");
        // Pages that are not repositories keep the plain path crumb, with no heading.
        let plain = dioxus_ssr::render_element(shell(
            Chrome {
                username: Some("alice".into()),
                csrf_token: "csrf-token".into(),
                return_to: "/alice/project".into(),
                ..Default::default()
            },
            None,
            rsx! { p { "Body" } },
        ));
        assert!(!plain.contains("<h1"), "{plain}");
        assert!(!plain.contains("repo-engagement"), "{plain}");
    }

    #[test]
    fn header_crumb_links_to_real_pages_and_skips_sign_in_pages() {
        let crumb = |path: &str| {
            dioxus_ssr::render_element(shell(
                Chrome {
                    username: Some("alice".into()),
                    csrf_token: "token".into(),
                    return_to: path.into(),
                    ..Default::default()
                },
                None,
                rsx! { p { "Body" } },
            ))
        };
        let team = crumb("/orgs/acme/teams/core");
        assert!(team.contains("href=\"/acme\""), "{team}");
        assert!(team.contains("href=\"/acme?tab=teams\""), "{team}");
        assert!(team.contains("href=\"/orgs/acme/teams/core\""), "{team}");
        assert!(!team.contains("href=\"/orgs\""), "{team}");
        assert!(crumb("/ci").contains("<span class=\"site-context\">Actions</span>"));
        for path in ["/forgot-password", "/setup", "/orgs/acme"] {
            let html = crumb(path);
            assert!(!html.contains("aria-label=\"Current location\""), "{html}");
            assert!(!html.contains("class=\"site-context\""), "{html}");
        }
    }

    #[test]
    fn header_search_sits_in_the_right_group_before_the_create_menu() {
        let html = dioxus_ssr::render_element(shell(
            Chrome {
                username: Some("alice".into()),
                csrf_token: "token".into(),
                return_to: "/alice/project".into(),
                ..Default::default()
            },
            None,
            rsx! { p { "Body" } },
        ));
        // The crumb stays on the left. The spacer pushes the search, the divider and the create
        // menu to the right, in that order.
        let order = [
            "aria-label=\"Current location\"",
            "class=\"site-header-spacer\"",
            "class=\"site-search site-search-wide\"",
            "class=\"site-header-divider\"",
            "aria-label=\"Create new\"",
        ];
        let positions: Vec<usize> = order
            .iter()
            .map(|marker| {
                html.find(marker)
                    .unwrap_or_else(|| panic!("{marker} missing: {html}"))
            })
            .collect();
        assert!(positions.windows(2).all(|pair| pair[0] < pair[1]), "{html}");
    }

    #[test]
    fn header_search_names_the_slash_key_and_the_phone_search_does_not() {
        let html = dioxus_ssr::render_element(shell(
            Chrome {
                username: Some("alice".into()),
                csrf_token: "token".into(),
                ..Default::default()
            },
            None,
            rsx! { p { "Body" } },
        ));
        // Only the header search has the hint; the phone search keeps its plain placeholder.
        assert_eq!(
            html.matches("placeholder=\"Type / to search\"").count(),
            1,
            "{html}"
        );
        assert_eq!(
            html.matches("class=\"site-search-key\"").count(),
            1,
            "{html}"
        );
        assert_eq!(
            html.matches("placeholder=\"Search repositories").count(),
            1,
            "{html}"
        );
    }

    #[test]
    fn shell_escapes_identity_and_does_not_fabricate_private_chrome() {
        let html = dioxus_ssr::render_element(shell(
            Chrome {
                username: Some("<private>".into()),
                csrf_token: "token".into(),
                ..Default::default()
            },
            None,
            rsx! { p { "Owned body" } },
        ));
        assert!(html.contains("&#60;private&#62;"));
        assert!(html.contains("href=\"/%3Cprivate%3E\""));
        assert!(!html.contains("<private>"));
        assert!(!html.contains("href=\"/admin\""));
        assert!(!html.contains("avatar_url"));
        assert!(html.contains("site-context"));

        let anonymous = dioxus_ssr::render_element(shell(
            Chrome::default(),
            None,
            rsx! { p { "Public body" } },
        ));
        assert!(!anonymous.contains("site-repository-context"));
        // Signed-out visitors keep the search, but there are no account controls to divide from it.
        assert!(anonymous.contains("class=\"site-search site-search-wide\""));
        assert!(!anonymous.contains("site-header-divider"));
        assert!(anonymous.contains("href=\"/login\""));
        assert!(anonymous.contains("href=\"/register\""));
        assert!(!anonymous.contains("href=\"/notifications\""));
        assert!(!anonymous.contains("href=\"/issues\""));
        assert!(!anonymous.contains("href=\"/pulls\""));
        assert!(!anonymous.contains("href=\"/new\""));
        assert!(!anonymous.contains("action=\"/logout\""));
        assert!(!anonymous.contains("action=\"/_forms/theme\""));
    }

    #[test]
    fn pages_have_no_footer_and_the_menu_keeps_help_and_explore() {
        for chrome in [
            Chrome::default(),
            Chrome {
                username: Some("alice".into()),
                csrf_token: "token".into(),
                ..Default::default()
            },
        ] {
            let html = dioxus_ssr::render_element(shell(chrome, None, rsx! { p { "Body" } }));
            assert!(!html.contains("<footer"), "{html}");
            assert!(html.contains("href=\"/help\""), "{html}");
            assert!(html.contains("href=\"/explore\""), "{html}");
        }
    }

    #[test]
    fn header_is_one_row_and_the_phone_menu_holds_the_inbox_and_theme() {
        let html = dioxus_ssr::render_element(shell(
            Chrome {
                username: Some("alice".into()),
                csrf_token: "token".into(),
                return_to: "/alice".into(),
                ..Default::default()
            },
            None,
            rsx! { p { "Body" } },
        ));
        assert_eq!(html.matches("class=\"site-header-row").count(), 1, "{html}");
        // One row: the menu, the logo, the search button, the create menu, then the account.
        let order = [
            "id=\"site-navigation-menu\"",
            "class=\"site-brand\"",
            "class=\"site-search-disclosure native-menu\"",
            "aria-label=\"Create new\"",
            "aria-label=\"Account menu for alice\"",
        ];
        let positions: Vec<usize> = order
            .iter()
            .map(|marker| {
                html.find(marker)
                    .unwrap_or_else(|| panic!("{marker} missing: {html}"))
            })
            .collect();
        assert!(positions.windows(2).all(|pair| pair[0] < pair[1]), "{html}");
        // The inbox and the theme button are icons in the header on wider screens, so the phone
        // menu carries a labelled copy of each.
        let menu = html
            .split_once("id=\"site-navigation-menu\"")
            .unwrap()
            .1
            .split_once("</nav>")
            .unwrap()
            .0;
        assert!(menu.contains("href=\"/notifications\""), "{menu}");
        assert!(menu.contains("site-menu-phone"), "{menu}");
        assert!(menu.contains("Notifications"), "{menu}");
        assert!(
            menu.contains("class=\"site-theme-toggle site-theme-menu\""),
            "{menu}"
        );
        assert!(menu.contains("Theme: System"), "{menu}");
    }

    #[test]
    fn help_is_in_the_phone_menu_and_after_settings_in_the_account_menu() {
        let signed_out =
            dioxus_ssr::render_element(shell(Chrome::default(), None, rsx! { p { "Body" } }));
        let menu = signed_out
            .split_once("id=\"site-navigation-menu\"")
            .unwrap()
            .1
            .split_once("</nav>")
            .unwrap()
            .0;
        assert!(menu.contains("href=\"/help\""), "{menu}");
        assert!(menu.contains("Help"), "{menu}");
        assert!(!signed_out.contains("id=\"site-account-menu\""));

        let signed_in = dioxus_ssr::render_element(shell(
            Chrome {
                username: Some("alice".into()),
                csrf_token: "token".into(),
                return_to: "/alice".into(),
                ..Default::default()
            },
            None,
            rsx! { p { "Body" } },
        ));
        let account = &signed_in[signed_in.find("id=\"site-account-menu\"").unwrap()..];
        let settings = account.find("href=\"/settings\"").unwrap();
        let help = account
            .find("href=\"/help\"")
            .unwrap_or_else(|| panic!("{account}"));
        assert!(settings < help, "{account}");
        assert!(account[help..].contains("Help"), "{account}");
    }

    #[test]
    fn styles_keep_diff_targets_in_their_rows_and_phone_menus_in_view() {
        let html = include_str!("native.css");
        assert!(html
            .contains("a.patch-add::after { content: \"\"; position: absolute; inset: 0 -12px; }"));
        assert!(html.contains(".site-header .repo-tabs-more > .native-menu-panel { max-height: calc(100dvh - 140px); overflow-y: auto; }"));
    }

    #[test]
    fn phone_search_sheet_is_a_form_with_a_close_summary_and_no_rows_before_typing() {
        let html = dioxus_ssr::render_element(shell(
            Chrome {
                username: Some("alice".into()),
                csrf_token: "token".into(),
                return_to: "/alice/project".into(),
                ..Default::default()
            },
            Some(RepoChrome {
                owner: "alice".into(),
                name: "project".into(),
                signed_in: true,
                csrf_token: "token".into(),
                ..Default::default()
            }),
            rsx! { p { "Body" } },
        ));
        let sheet = html
            .split_once("class=\"site-search-disclosure native-menu\"")
            .unwrap()
            .1
            .split_once("</details>")
            .unwrap()
            .0;
        // The summary shows the search icon, and the close icon once the sheet is open.
        assert!(sheet.contains("site-search-open-icon"), "{sheet}");
        assert!(sheet.contains("site-search-close-icon"), "{sheet}");
        assert!(
            sheet.contains("class=\"site-search-sheet-form\""),
            "{sheet}"
        );
        assert!(sheet.contains("action=\"/search\""), "{sheet}");
        assert!(sheet.contains("name=\"q\""), "{sheet}");
        assert!(sheet.contains("id=\"site-search-sheet-query\""), "{sheet}");
        // Without scripts nothing is typed, so no search row is drawn. The repository's file finder
        // is the one row that needs no text.
        assert!(!sheet.contains("Search for"), "{sheet}");
        assert!(sheet.contains("href=\"/alice/project/find/"), "{sheet}");
        assert!(sheet.contains("Go to file in this repository"), "{sheet}");
        // The hint names the scopes the search covers before anything is typed.
        assert!(sheet.contains("class=\"site-search-hint\""), "{sheet}");
        assert!(
            sheet.contains(
                "Search alice/project, all repositories, issues, pull requests, code or people."
            ),
            "{sheet}"
        );
    }

    #[test]
    fn create_menu_offers_new_and_uploaded_files_to_writers_of_the_open_repository() {
        let render = |can_write: bool, in_repo: bool| {
            dioxus_ssr::render_element(shell(
                Chrome {
                    username: Some("alice".into()),
                    csrf_token: "token".into(),
                    ..Default::default()
                },
                in_repo.then(|| RepoChrome {
                    owner: "alice".into(),
                    name: "project".into(),
                    default_branch: "main".into(),
                    can_write,
                    signed_in: true,
                    csrf_token: "token".into(),
                    ..Default::default()
                }),
                rsx! { p { "Body" } },
            ))
        };
        let writer = render(true, true);
        assert!(
            writer.contains("href=\"/alice/project/new/main\""),
            "{writer}"
        );
        assert!(
            writer.contains("href=\"/alice/project/upload/main\""),
            "{writer}"
        );
        for html in [render(false, true), render(true, false)] {
            assert!(!html.contains("Create new file"), "{html}");
            assert!(html.contains("New repository"), "{html}");
        }
    }

    #[test]
    fn phone_search_summary_is_the_trigger_that_takes_focus_back() {
        let html = dioxus_ssr::render_element(shell(
            Chrome {
                username: Some("alice".into()),
                csrf_token: "token".into(),
                ..Default::default()
            },
            None,
            rsx! { p { "Body" } },
        ));
        let disclosure = html
            .split_once("class=\"site-search-disclosure native-menu\"")
            .unwrap()
            .1
            .split_once("</details>")
            .unwrap()
            .0;
        // The summary is the button that opens the sheet. Focus returns to it when the sheet closes.
        let summary = disclosure
            .split_once("<summary")
            .unwrap_or_else(|| panic!("{disclosure}"))
            .1
            .split_once("</summary>")
            .unwrap()
            .0;
        assert!(summary.contains("id=\"site-search-trigger\""), "{summary}");
    }

    #[test]
    fn search_scopes_link_the_typed_text_to_each_results_page() {
        let text = encode_component("rust & wasm");
        let scoped = encode_component("rust & wasm repo:alice/project");
        let rows = search_scopes("  rust & wasm ", Some("alice/project"));
        let labels: Vec<&str> = rows.iter().map(|(label, _, _, _)| label.as_str()).collect();
        assert_eq!(
            labels,
            [
                "Search for 'rust & wasm' in this repository",
                "Search for 'rust & wasm' in all repositories",
                "Issues",
                "Pull requests",
                "Code",
                "People",
            ]
        );
        // Inside a repository, the kinds below the repository row say they search every repository.
        let scopes: Vec<Option<&str>> = rows.iter().map(|(_, _, _, scope)| *scope).collect();
        assert_eq!(
            scopes,
            [
                None,
                None,
                Some("All repositories"),
                Some("All repositories"),
                Some("All repositories"),
                Some("All repositories"),
            ]
        );
        let hrefs: Vec<String> = rows.into_iter().map(|(_, href, _, _)| href).collect();
        assert_eq!(
            hrefs,
            [
                format!("/search?q={scoped}&type=code"),
                format!("/search?q={text}&type=repos"),
                format!("/search?q={text}&type=issues"),
                format!("/search?q={text}&type=pulls"),
                format!("/search?q={text}&type=code"),
                format!("/search?q={text}&type=users"),
            ]
        );
        // Outside a repository there is no repository row, and nothing typed means no rows.
        assert_eq!(search_scopes("rust", None).len(), 5);
        assert!(search_scopes("rust", None)
            .iter()
            .all(|(_, _, _, scope)| scope.is_none()));
        assert!(search_scopes("   ", Some("alice/project")).is_empty());
    }
}
