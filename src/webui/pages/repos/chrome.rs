use crate::webui::navigation::NavLink;
pub(super) use crate::webui::pages::fields::csrf;
use dioxus::prelude::*;

#[derive(Clone, Debug, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct RepoChrome {
    /// Immutable repository ID. Repository forms send it back, so a form rendered for one
    /// repository cannot act on another that later takes its owner and name.
    pub id: i64,
    pub owner: String,
    pub name: String,
    /// The branch the repository opens on. The branch menu marks it.
    pub default_branch: String,
    pub description: String,
    /// The About website; empty when the repository has none.
    pub website: String,
    /// The About topics, alphabetically.
    pub topics: Vec<String>,
    pub is_private: bool,
    pub is_archived: bool,
    pub can_write: bool,
    pub can_admin: bool,
    pub is_owner: bool,
    /// Only personal owners manage collaborators; organization repositories have no such form.
    pub can_manage_collaborators: bool,
    /// Why the signed-in viewer cannot fork this repository. `None` means the Fork form is shown.
    pub fork_refusal: Option<String>,
    pub signed_in: bool,
    /// The signed-in account, or 0. Drafts kept in the browser are scoped to it.
    pub viewer_id: i64,
    pub starred: bool,
    /// How the viewer watches the repository: empty when they only participate, else
    /// `watching`, `custom` or `ignoring`.
    pub watch_level: String,
    /// The events a custom watch wants, comma-separated: `issues`, `pulls`.
    pub watch_events: String,
    pub stars: i32,
    pub watchers: i32,
    pub forks: i32,
    /// Open issues and open pull requests, for the tab counts.
    pub open_issues: i64,
    pub open_pulls: i64,
    pub fork_url: Option<String>,
    pub parent_name: Option<String>,
    pub parent_url: Option<String>,
    pub csrf_token: String,
    pub current_path: String,
}
impl RepoChrome {
    pub fn base(&self) -> String {
        format!(
            "/{}/{}",
            crate::webui::encode_component(&self.owner),
            crate::webui::encode_component(&self.name)
        )
    }
    pub fn action(&self, action: &str) -> String {
        format!("{}/_forms/{action}", self.base())
    }
}

/// The repository page body. The name and the visibility badges are drawn by the site header band;
/// see `repo_badges` and `repo_tabs`. The Watch, Fork and Star buttons sit in the header band on
/// wider screens. A phone keeps them in the More menu of the tab row.
pub fn chrome(repo: &RepoChrome, body: Element) -> Element {
    rsx! {
        div { class: "repo-content page-shell",
            if let (Some(name), Some(url)) = (&repo.parent_name, &repo.parent_url) {
                p { class: "repo-fork-notice", "forked from " NavLink { href: url.clone(), "{name}" } }
            }
            if repo.is_archived {
                p { class: "repo-archived-banner", role: "status", {icon("archive")} span { "This repository is archived and read-only." } }
            }
            {body}
        }
    }
}

/// Tabs that stay in the strip at every width.
const PRIMARY_TABS: [(&str, &str, &str); 3] = [
    ("code", "Code", ""),
    ("issues", "Issues", "/issues"),
    ("pulls", "Pull requests", "/pulls"),
];

/// Tabs that move behind the More menu on a phone. Settings is listed only for administrators.
const SECONDARY_TABS: [(&str, &str, &str); 3] = [
    ("actions", "Actions", "/actions"),
    ("insights", "Insights", "/insights"),
    ("settings", "Settings", "/settings"),
];

/// The repository tabs. Every repository page draws them in the site header band, under the
/// site row. The active tab follows the page path. On a phone the strip keeps the primary tabs
/// in view and the secondary ones move into a More menu beside it.
pub fn repo_tabs(repo: &RepoChrome) -> Element {
    let base = repo.base();
    let secondary: Vec<(&str, &str, &str)> = SECONDARY_TABS
        .into_iter()
        .filter(|(id, _, _)| *id != "settings" || repo.can_admin)
        .collect();
    let more_current = secondary
        .iter()
        .any(|(_, _, suffix)| active_tab(&base, &repo.current_path, suffix));
    // Counts of three digits or more need room the tab widths do not leave.
    let wide_counts = repo.open_issues >= 100 || repo.open_pulls >= 100;
    rsx! {
        div { class: "repo-tab-bar",
            div { class: if wide_counts { "repo-tab-strip repo-tab-strip-wide" } else { "repo-tab-strip" },
                nav { class: "repo-tabs", "aria-label": "Repository",
                    for (id, label, suffix) in PRIMARY_TABS {
                        {repo_tab(repo, &base, id, label, suffix)}
                    }
                    // Each one joins the strip when it fits; until then the More menu lists it.
                    div { class: "repo-tabs-secondary",
                        for (id, label, suffix) in secondary.iter().copied() {
                            {repo_tab(repo, &base, id, label, suffix)}
                        }
                    }
                }
                details { class: "native-menu repo-tabs-more",
                    summary { class: if more_current { "repo-tab repo-tabs-more-current" } else { "repo-tab" }, "aria-label": "More repository pages", title: "More repository pages",
                        {icon("more")}
                    }
                    div { class: "native-menu-panel",
                        div { class: "site-menu-links",
                            for (id, label, suffix) in secondary.iter().copied() {
                                NavLink { key: "{id}", href: format!("{base}{suffix}"), class: if active_tab(&base, &repo.current_path, suffix) { "site-menu-link repo-more-active".to_owned() } else { "site-menu-link".to_owned() },
                                    {icon(id)} span { "{label}" }
                                }
                            }
                            // A phone has no room for these in the strip.
                            div { class: "repo-more-engagement",
                                hr { class: "site-menu-divider" }
                                {repo_engagement(repo, "repo-engagement repo-engagement-more")}
                            }
                        }
                    }
                }
            }
            // Watch, Fork and Star follow the strip, which takes the width they leave.
            {repo_engagement(repo, "repo-engagement flex flex-wrap items-center gap-2")}
        }
        TabStripScroll { strip: ".repo-tabs", path: repo.current_path.clone() }
    }
}

/// One link in the tab strip. The open count is shown on the tabs that have one.
fn repo_tab(
    repo: &RepoChrome,
    base: &str,
    id: &'static str,
    label: &'static str,
    suffix: &'static str,
) -> Element {
    let class = if active_tab(base, &repo.current_path, suffix) {
        "repo-tab repo-tab-active"
    } else {
        "repo-tab"
    };
    rsx! {
        NavLink { key: "{id}", href: format!("{base}{suffix}"), class: class.to_owned(),
            {icon(id)} span { "{label}" }
            if let Some(count) = tab_count(repo, id) { span { class: "repo-tab-count", "{count}" } }
        }
    }
}

/// Centres the active tab of a horizontal tab strip, so a phone opens on the current tab rather
/// than the first one. `path` is the page path; the tab is centred again when it changes. Only
/// the strip's own scroll position moves, so the page does not jump.
#[component]
pub fn TabStripScroll(strip: &'static str, path: String) -> Element {
    use_effect(use_reactive((&path,), move |(_path,)| {
        center_active_tab(strip)
    }));
    rsx! {}
}

fn center_active_tab(strip: &str) {
    #[cfg(target_arch = "wasm32")]
    {
        let found = web_sys::window()
            .and_then(|window| window.document())
            .and_then(|document| document.query_selector(strip).ok().flatten());
        let Some(strip_element) = found else {
            return;
        };
        let Some(active) = strip_element
            .query_selector("[aria-current='page'], .repo-tab-active")
            .ok()
            .flatten()
        else {
            return;
        };
        let strip_box = strip_element.get_bounding_client_rect();
        let active_box = active.get_bounding_client_rect();
        let offset = (active_box.left() + active_box.width() / 2.0)
            - (strip_box.left() + strip_box.width() / 2.0);
        // The browser clamps the value, so a strip that fits does not move.
        strip_element.set_scroll_left(strip_element.scroll_left() + offset.round() as i32);
    }
    #[cfg(not(target_arch = "wasm32"))]
    let _ = strip;
}

/// The open count shown on a tab, for the tabs that have one. A thousand or more reads "999+",
/// so a count never makes the tab wider than the strip allows for.
fn tab_count(repo: &RepoChrome, id: &str) -> Option<String> {
    let count = match id {
        "issues" => repo.open_issues,
        "pulls" => repo.open_pulls,
        _ => return None,
    };
    Some(if count >= 1000 {
        "999+".to_owned()
    } else {
        count.to_string()
    })
}

pub fn active_tab(base: &str, path: &str, suffix: &str) -> bool {
    let Some(tail) = path.strip_prefix(base) else {
        return false;
    };
    if suffix.is_empty() {
        // Commits, branches, tags and releases open from the code page, as on GitHub, so they
        // belong to its tab.
        tail.is_empty()
            || tail == "/"
            || [
                "/tree/", "/blob/", "/blame/", "/find/", "/edit/", "/new/", "/upload/",
            ]
            .iter()
            .any(|prefix| tail.starts_with(prefix))
            || ["/commits", "/commit", "/branches", "/tags", "/releases"]
                .iter()
                .any(|page| tail == *page || tail.starts_with(&format!("{page}/")))
    } else if suffix == "/pulls" && (tail == "/compare" || tail.starts_with("/compare/")) {
        // Comparing branches is part of the pull request workflow.
        true
    } else if suffix == "/issues"
        && (tail == "/milestones" || tail.starts_with("/milestones/") || tail == "/labels")
    {
        // Milestones and labels are part of the issue tracker.
        true
    } else {
        tail == suffix || tail.starts_with(&format!("{suffix}/"))
    }
}

/// The hidden field that carries the repository's immutable ID back with a form.
pub(super) fn repository_id(id: i64) -> Element {
    rsx! { input { r#type: "hidden", name: "repository_id", value: id.to_string() } }
}

/// The visibility and archive labels. The header band draws them after the repository name.
pub fn repo_badges(repo: &RepoChrome) -> Element {
    let (icon_name, label) = if repo.is_private {
        ("lock", "Private")
    } else {
        ("globe", "Public")
    };
    rsx! {
        // A phone keeps only the icon; the title and the hidden word say what it means.
        span { class: "repo-visibility", title: label, {icon(icon_name)} span { class: "repo-visibility-text", "{label}" } }
        if repo.is_archived { span { class: "repo-visibility", title: "Archived", {icon("archive")} span { class: "repo-visibility-text", "Archived" } } }
    }
}

/// The Watch, Fork and Star buttons, in the wrapper `class`. The header band draws them at the right
/// end of the tab row, and the page draws a phone row under the header. Each one is a native form or
/// a link to sign in, so they work without JavaScript.
pub fn repo_engagement(repo: &RepoChrome, class: &str) -> Element {
    let login = format!(
        "/login?redirect={}",
        crate::webui::encode_component(&repo.base())
    );
    rsx! {
        div { class: "{class}",
            if repo.signed_in {
                {watch_menu(repo)}
            } else {
                NavLink { class: "repo-engagement-button".to_owned(), href: login.clone(),
                    {engagement_content("watch", "Watch", repo.watchers)}
                }
            }
            if let Some(url) = &repo.fork_url {
                NavLink { class: "repo-engagement-button".to_owned(), href: url.clone(),
                    {engagement_content("fork", "Forked", repo.forks)}
                }
            } else if repo.signed_in {
                if let Some(reason) = &repo.fork_refusal {
                    // A refusal, such as forking your own or an archived repository, keeps a
                    // disabled control that says why.
                    button { class: "repo-engagement-button", r#type: "button", disabled: true, title: reason.clone(),
                        {engagement_content("fork", "Fork", repo.forks)}
                    }
                } else {
                    form { method: "post", action: repo.action("fork"),
                        {csrf(&repo.csrf_token)}
                        button { class: "repo-engagement-button", r#type: "submit",
                            {engagement_content("fork", "Fork", repo.forks)}
                        }
                    }
                }
            } else {
                NavLink { class: "repo-engagement-button".to_owned(), href: login.clone(),
                    {engagement_content("fork", "Fork", repo.forks)}
                }
            }
            if repo.signed_in {
                {engagement_form(repo, if repo.starred { "unstar" } else { "star" }, if repo.starred { "Starred" } else { "Star" }, repo.stars)}
            } else {
                NavLink { class: "repo-engagement-button".to_owned(), href: login,
                    {engagement_content("star", "Star", repo.stars)}
                }
            }
        }
    }
}

/// The Watch button opens a menu of watch levels. Apply posts the choice, so it works without
/// JavaScript.
fn watch_menu(repo: &RepoChrome) -> Element {
    let level = repo.watch_level.as_str();
    let label = match level {
        "watching" => "Watching",
        "custom" => "Custom",
        "ignoring" => "Ignoring",
        _ => "Watch",
    };
    let wants = |name: &str| repo.watch_events.split(',').any(|event| event == name);
    // The tooltip names the events a custom level picked, since the closed button only says Custom.
    let tooltip = if level == "custom" {
        let chosen: Vec<&str> = [
            ("issues", "Issues"),
            ("pulls", "Pull requests"),
            ("releases", "Releases"),
        ]
        .into_iter()
        .filter(|&(id, _)| wants(id))
        .map(|(_, name)| name)
        .collect();
        if chosen.is_empty() {
            "Custom: no activity chosen".to_owned()
        } else {
            format!("Custom: {}", chosen.join(", "))
        }
    } else {
        "Notification settings for this repository".to_owned()
    };
    let choices = [
        (
            "participating",
            "Participating and @mentions",
            "Only when you take part or are @mentioned.",
        ),
        (
            "watching",
            "All activity",
            "Every new issue, pull request and release.",
        ),
        ("custom", "Custom", "Only the kinds of activity you pick."),
        (
            "ignoring",
            "Ignore",
            "Never notified about this repository.",
        ),
    ];
    rsx! {
        details { class: "native-menu repo-watch-menu",
            summary { class: "repo-engagement-button", title: "{tooltip}",
                span { class: "repo-engagement-label", title: "{tooltip}", {icon("watch")} span { class: "repo-engagement-text", "{label}" } }
                span { class: "repo-engagement-count", "{repo.watchers}" }
            }
            div { class: "native-menu-panel repo-watch-panel",
                form { method: "post", action: repo.action("watch-level"),
                    {csrf(&repo.csrf_token)}
                    strong { "Notifications" }
                    for (value, title, hint) in choices {
                        label { class: "repo-watch-choice",
                            input { r#type: "radio", name: "level", value, checked: level == value || (value == "participating" && level.is_empty()) }
                            span { strong { "{title}" } small { "{hint}" } }
                        }
                        if value == "custom" {
                            div { class: "repo-watch-events",
                                label { class: "repo-check-label", input { r#type: "checkbox", name: "events_issues", checked: wants("issues") } " Issues" }
                                label { class: "repo-check-label", input { r#type: "checkbox", name: "events_pulls", checked: wants("pulls") } " Pull requests" }
                                label { class: "repo-check-label", input { r#type: "checkbox", name: "events_releases", checked: wants("releases") } " Releases" }
                            }
                        }
                    }
                    button { class: "btn btn-primary btn-sm", r#type: "submit", "Apply" }
                }
            }
        }
    }
}

/// The Star and Unstar button: a form that posts `action`.
fn engagement_form(repo: &RepoChrome, action: &str, label: &str, count: i32) -> Element {
    rsx! { form { method: "post", action: repo.action(action),
        {csrf(&repo.csrf_token)}
        {repository_id(repo.id)}
        button { r#type: "submit", class: if action == "unstar" { "repo-engagement-button repo-starred" } else { "repo-engagement-button" },
            {engagement_content("star", label, count)}
        }
    } }
}

fn engagement_content(kind: &str, label: &str, count: i32) -> Element {
    rsx! {
        span { class: "repo-engagement-label", title: "{label}", {icon(kind)} span { class: "repo-engagement-text", "{label}" } }
        span { class: "repo-engagement-count", "{count}" }
    }
}

/// The icon named `kind`. A name that has no icon draws a gear.
/// A page's sections as one menu, for a phone: a row of tabs has no room there, and a wrapped
/// row looks broken. Each link is its href, its label and whether it is the current page.
pub(crate) fn section_menu(name: &str, links: &[(String, &str, bool)]) -> Element {
    let label = links
        .iter()
        .find(|(_, _, current)| *current)
        .or(links.first())
        .map_or("", |(_, label, _)| *label);
    rsx! {
        details { class: "native-menu repo-menu repo-settings-picker",
            summary { class: "btn btn-secondary", aria_label: "{name}", span { "{label}" } {icon("chevron")} }
            div { class: "repo-menu-panel",
                for (href, text, current) in links {
                    NavLink {
                        key: "{href}",
                        class: "repo-menu-item".to_owned(),
                        href: href.clone(),
                        aria_current: current.then(|| "page".to_owned()),
                        "{text}"
                    }
                }
            }
        }
    }
}

pub(crate) fn icon(kind: &str) -> Element {
    rsx! {
        svg { class: "repo-icon", view_box: "0 0 24 24", fill: "none", stroke: "currentColor", stroke_width: "2", stroke_linecap: "round", stroke_linejoin: "round", width: "16", height: "16", "aria-hidden": "true", "focusable": "false",
            match kind {
                "repo" => rsx! {
                    path { d: "M4 19.5A2.5 2.5 0 0 1 6.5 17H20" }
                    path { d: "M6.5 2H20v20H6.5A2.5 2.5 0 0 1 4 19.5v-15A2.5 2.5 0 0 1 6.5 2Z" }
                    path { d: "M8 7h8M8 11h6" }
                },
                "code" => rsx! { path { d: "m18 16 4-4-4-4M6 8l-4 4 4 4M14.5 4l-5 16" } },
                "issues" => rsx! { circle { cx: "12", cy: "12", r: "10" } circle { cx: "12", cy: "12", r: "1" } },
                "pulls" => rsx! { circle { cx: "18", cy: "18", r: "3" } circle { cx: "6", cy: "6", r: "3" } path { d: "M13 6h3a2 2 0 0 1 2 2v7M6 9v12" } },
                "actions" => rsx! { circle { cx: "12", cy: "12", r: "9" } path { d: "m10 8 6 4-6 4z" } },
                "insights" => rsx! { path { d: "M3 3v16a2 2 0 0 0 2 2h16M7 16v-4M12 16V8M17 16v-7" } },
                "commits" => rsx! { path { d: "M3 12a9 9 0 1 0 9-9 9.75 9.75 0 0 0-6.74 2.74L3 8M3 3v5h5M12 7v5l4 2" } },
                "branches" => rsx! { path { d: "M6 3v12M18 9a9 9 0 0 1-9 9" } circle { cx: "18", cy: "6", r: "3" } circle { cx: "6", cy: "18", r: "3" } },
                "smile" => rsx! { circle { cx: "12", cy: "12", r: "10" } path { d: "M8 14s1.5 2 4 2 4-2 4-2M9 9h.01M15 9h.01" } },
                "releases" => rsx! { path { d: "m7.5 4.27 9 5.15M21 8a2 2 0 0 0-1-1.73l-7-4a2 2 0 0 0-2 0l-7 4A2 2 0 0 0 3 8v8a2 2 0 0 0 1 1.73l7 4a2 2 0 0 0 2 0l7-4A2 2 0 0 0 21 16z" } path { d: "m3.3 7 8.7 5 8.7-5M12 22V12" } },
                "tags" => rsx! { path { d: "M12.586 2.586A2 2 0 0 0 11.172 2H4a2 2 0 0 0-2 2v7.172a2 2 0 0 0 .586 1.414l8.704 8.704a2.426 2.426 0 0 0 3.42 0l6.58-6.58a2.426 2.426 0 0 0 0-3.42z" } circle { cx: "7.5", cy: "7.5", r: ".5", fill: "currentColor" } },
                "watch" => rsx! { path { d: "M2.062 12.348a1 1 0 0 1 0-.696 10.75 10.75 0 0 1 19.876 0 1 1 0 0 1 0 .696 10.75 10.75 0 0 1-19.876 0" } circle { cx: "12", cy: "12", r: "3" } },
                "fork" => rsx! { circle { cx: "12", cy: "18", r: "3" } circle { cx: "6", cy: "6", r: "3" } circle { cx: "18", cy: "6", r: "3" } path { d: "M18 9v2c0 .6-.4 1-1 1H7c-.6 0-1-.4-1-1V9M12 12v3" } },
                "star" => rsx! { path { d: "M12 2l3.09 6.26L22 9.27l-5 4.87 1.18 6.88L12 17.77l-6.18 3.25L7 14.14 2 9.27l6.91-1.01L12 2z" } },
                "folder" => rsx! { path { d: "M20 20a2 2 0 0 0 2-2V8a2 2 0 0 0-2-2h-7.9a2 2 0 0 1-1.69-.9L9.6 3.9A2 2 0 0 0 7.93 3H4a2 2 0 0 0-2 2v13a2 2 0 0 0 2 2Z" } },
                "file" => rsx! { path { d: "M15 2H6a2 2 0 0 0-2 2v16a2 2 0 0 0 2 2h12a2 2 0 0 0 2-2V7ZM14 2v4a2 2 0 0 0 2 2h4M16 13H8M16 17H8M10 9H8" } },
                "chevron" => rsx! { path { d: "m6 9 6 6 6-6" } },
                "plus" => rsx! { path { d: "M5 12h14M12 5v14" } },
                "download" => rsx! { path { d: "M12 15V3M21 15v4a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2v-4m-5-5 5 5 5-5" } },
                "link" => rsx! { path { d: "M10 13a5 5 0 0 0 7.54.54l3-3a5 5 0 0 0-7.07-7.07l-1.72 1.71" } path { d: "M14 11a5 5 0 0 0-7.54-.54l-3 3a5 5 0 0 0 7.07 7.07l1.71-1.71" } },
                "readme" => rsx! { path { d: "M12 7v14M3 3h6a3 3 0 0 1 3 3 3 3 0 0 1 3-3h6v18h-6a3 3 0 0 0-3 3 3 3 0 0 0-3-3H3Z" } },
                "edit" => rsx! { path { d: "m16 3 5 5M4 20l4-1L21 6a2.1 2.1 0 0 0-3-3L5 16Z" } },
                "trash" => rsx! { path { d: "M3 6h18M19 6v14a2 2 0 0 1-2 2H7a2 2 0 0 1-2-2V6m3 0V4a2 2 0 0 1 2-2h4a2 2 0 0 1 2 2v2" } },
                "search" => rsx! { circle { cx: "11", cy: "11", r: "8" } path { d: "m21 21-4.3-4.3" } },
                "upload" => rsx! { path { d: "M21 15v4a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2v-4M17 8l-5-5-5 5M12 3v12" } },
                "lock" => rsx! { rect { x: "4", y: "11", width: "16", height: "10", rx: "2" } path { d: "M8 11V7a4 4 0 0 1 8 0v4" } },
                "globe" => rsx! { circle { cx: "12", cy: "12", r: "9" } path { d: "M3 12h18M12 3a14 14 0 0 1 0 18M12 3a14 14 0 0 0 0 18" } },
                "archive" => rsx! { rect { x: "3", y: "4", width: "18", height: "4", rx: "1" } path { d: "M5 8v11a1 1 0 0 0 1 1h12a1 1 0 0 0 1-1V8M10 12h4" } },
                "check" => rsx! { path { d: "M20 6 9 17l-5-5" } },
                "more" => rsx! { circle { cx: "5", cy: "12", r: "1" } circle { cx: "12", cy: "12", r: "1" } circle { cx: "19", cy: "12", r: "1" } },
                "files" => rsx! { path { d: "M3 3h18v18H3zM9 3v18" } },
                "expand" => rsx! { path { d: "M7 9l5-5 5 5M7 15l5 5 5-5" } },
                "collapse" => rsx! { path { d: "M7 4l5 5 5-5M7 20l5-5 5 5" } },
                "outline" => rsx! { path { d: "M8 6h13M8 12h13M8 18h13M3 6h.01M3 12h.01M3 18h.01" } },
                _ => rsx! {
                    path { d: "M12.22 2h-.44a2 2 0 0 0-2 2v.18a2 2 0 0 1-1 1.73l-.43.25a2 2 0 0 1-2 0l-.15-.08a2 2 0 0 0-2.73.73l-.22.38a2 2 0 0 0 .73 2.73l.15.1a2 2 0 0 1 1 1.72v.51a2 2 0 0 1-1 1.74l-.15.09a2 2 0 0 0-.73 2.73l.22.38a2 2 0 0 0 2.73.73l.15-.08a2 2 0 0 1 2 0l.43.25a2 2 0 0 1 1 1.73V20a2 2 0 0 0 2 2h.44a2 2 0 0 0 2-2v-.18a2 2 0 0 1 1-1.73l.43-.25a2 2 0 0 1 2 0l.15.08a2 2 0 0 0 2.73-.73l.22-.38a2 2 0 0 0-.73-2.73l-.15-.09a2 2 0 0 1-1-1.74v-.5a2 2 0 0 1 1-1.74l.15-.09a2 2 0 0 0 .73-2.73l-.22-.38a2 2 0 0 0-2.73-.73l-.15.08a2 2 0 0 1-2 0l-.43-.25a2 2 0 0 1-1-1.73V4a2 2 0 0 0-2-2Z" }
                    circle { cx: "12", cy: "12", r: "3" }
                },
            }
        }
    }
}
