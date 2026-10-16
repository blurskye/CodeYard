use crate::webui::navigation::NavLink;
use dioxus::prelude::*;

#[derive(Clone, Debug, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct RepoChrome {
    /// Immutable repository ID. Repository forms send it back, so a form rendered for one
    /// repository cannot act on another that later takes its owner and name.
    pub id: i64,
    pub owner: String,
    pub name: String,
    pub description: String,
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
    pub watching: bool,
    pub stars: i32,
    pub watchers: i32,
    pub forks: i32,
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

pub fn chrome(repo: &RepoChrome, body: Element) -> Element {
    let base = repo.base();
    let tabs = [
        ("code", "Code", ""),
        ("issues", "Issues", "/issues"),
        ("pulls", "Pull requests", "/pulls"),
        ("actions", "Actions", "/actions"),
        ("commits", "Commits", "/commits"),
        ("branches", "Branches", "/branches"),
        ("tags", "Tags", "/tags"),
    ];
    let login = format!("/login?redirect={}", crate::webui::encode_component(&base));
    rsx! {
        // Raw-text elements cannot contain Dioxus hydration text markers.
        // This is exclusively trusted, compile-time CSS, never repository/user text.
        style { dangerous_inner_html: include_str!("style.css") }
        section { class: "repo-chrome bg-background",
            div { class: "repo-header w-full px-4 sm:px-6 lg:px-8",
                div { class: "repo-heading flex flex-wrap items-center justify-between gap-3",
                    div { class: "min-w-0",
                        h1 { class: "repo-title flex flex-wrap items-center gap-2 text-xl",
                            {icon("repo")}
                            NavLink { href: format!("/{}", crate::webui::encode_component(&repo.owner)), "{repo.owner}" }
                            span { class: "repo-title-slash", "/" }
                            NavLink { href: base.clone(), class: "repo-name".to_owned(), mark_current: false, "{repo.name}" }
                            span { class: "repo-visibility", if repo.is_private { "Private" } else { "Public" } }
                            if repo.is_archived { span { class: "repo-visibility", "Archived" } }
                        }
                        if let (Some(name), Some(url)) = (&repo.parent_name, &repo.parent_url) {
                            p { class: "repo-fork-notice", "forked from " NavLink { href: url.clone(), "{name}" } }
                        }
                    }
                    div { class: "repo-engagement flex flex-wrap items-center gap-2",
                        if repo.signed_in {
                            {engagement_form(repo, if repo.watching { "unwatch" } else { "watch" }, if repo.watching { "Watching" } else { "Watch" }, repo.watchers)}
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
            nav { class: "repo-tabs flex overflow-x-auto px-4 sm:px-6 lg:px-8", "aria-label": "Repository",
                for (id, label, suffix) in tabs {
                    NavLink { key: "{id}", href: format!("{base}{suffix}"), class: (if active_tab(&base, &repo.current_path, suffix) { "repo-tab repo-tab-active" } else { "repo-tab" }).to_owned(),
                        {icon(id)} span { "{label}" }
                    }
                }
                if repo.can_admin {
                    NavLink { href: format!("{base}/settings"), class: (if active_tab(&base, &repo.current_path, "/settings") { "repo-tab repo-tab-active" } else { "repo-tab" }).to_owned(),
                        {icon("settings")} span { "Settings" }
                    }
                }
            }
            TabStripScroll { strip: ".repo-tabs", path: repo.current_path.clone() }
        }
        div { class: "repo-content page-shell", {body} }
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

pub fn active_tab(base: &str, path: &str, suffix: &str) -> bool {
    let Some(tail) = path.strip_prefix(base) else {
        return false;
    };
    if suffix.is_empty() {
        tail.is_empty()
            || tail == "/"
            || [
                "/tree/", "/blob/", "/blame/", "/find/", "/edit/", "/new/", "/upload/",
            ]
            .iter()
            .any(|prefix| tail.starts_with(prefix))
    } else if suffix == "/pulls" && (tail == "/compare" || tail.starts_with("/compare/")) {
        // Comparing branches is part of the pull request workflow.
        true
    } else if suffix == "/issues" && (tail == "/milestones" || tail.starts_with("/milestones/")) {
        // Milestones are part of the issue tracker.
        true
    } else {
        tail == suffix || tail.starts_with(&format!("{suffix}/"))
    }
}

pub(super) fn csrf(token: &str) -> Element {
    rsx! { input { r#type: "hidden", name: "csrf_token", value: token.to_owned() } }
}

pub(super) fn repository_id(id: i64) -> Element {
    rsx! { input { r#type: "hidden", name: "repository_id", value: id.to_string() } }
}
fn engagement_form(repo: &RepoChrome, action: &str, label: &str, count: i32) -> Element {
    let kind = if action.contains("watch") {
        "watch"
    } else {
        "star"
    };
    rsx! { form { method: "post", action: repo.action(action),
        {csrf(&repo.csrf_token)}
        button { r#type: "submit", class: if action == "unstar" { "repo-engagement-button repo-starred" } else { "repo-engagement-button" },
            {engagement_content(kind, label, count)}
        }
    } }
}

fn engagement_content(kind: &str, label: &str, count: i32) -> Element {
    rsx! {
        span { class: "repo-engagement-label", {icon(kind)} "{label}" }
        span { class: "repo-engagement-count", "{count}" }
    }
}

/// Original repository SVG vocabulary, without browser data/session ownership.
pub(super) fn icon(kind: &str) -> Element {
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
                "commits" => rsx! { path { d: "M3 12a9 9 0 1 0 9-9 9.75 9.75 0 0 0-6.74 2.74L3 8M3 3v5h5M12 7v5l4 2" } },
                "branches" => rsx! { path { d: "M6 3v12M18 9a9 9 0 0 1-9 9" } circle { cx: "18", cy: "6", r: "3" } circle { cx: "6", cy: "18", r: "3" } },
                "tags" => rsx! { path { d: "M12.586 2.586A2 2 0 0 0 11.172 2H4a2 2 0 0 0-2 2v7.172a2 2 0 0 0 .586 1.414l8.704 8.704a2.426 2.426 0 0 0 3.42 0l6.58-6.58a2.426 2.426 0 0 0 0-3.42z" } circle { cx: "7.5", cy: "7.5", r: ".5", fill: "currentColor" } },
                "watch" => rsx! { path { d: "M2.062 12.348a1 1 0 0 1 0-.696 10.75 10.75 0 0 1 19.876 0 1 1 0 0 1 0 .696 10.75 10.75 0 0 1-19.876 0" } circle { cx: "12", cy: "12", r: "3" } },
                "fork" => rsx! { circle { cx: "12", cy: "18", r: "3" } circle { cx: "6", cy: "6", r: "3" } circle { cx: "18", cy: "6", r: "3" } path { d: "M18 9v2c0 .6-.4 1-1 1H7c-.6 0-1-.4-1-1V9M12 12v3" } },
                "star" => rsx! { path { d: "M12 2l3.09 6.26L22 9.27l-5 4.87 1.18 6.88L12 17.77l-6.18 3.25L7 14.14 2 9.27l6.91-1.01L12 2z" } },
                "folder" => rsx! { path { d: "M20 20a2 2 0 0 0 2-2V8a2 2 0 0 0-2-2h-7.9a2 2 0 0 1-1.69-.9L9.6 3.9A2 2 0 0 0 7.93 3H4a2 2 0 0 0-2 2v13a2 2 0 0 0 2 2Z" } },
                "file" => rsx! { path { d: "M15 2H6a2 2 0 0 0-2 2v16a2 2 0 0 0 2 2h12a2 2 0 0 0 2-2V7ZM14 2v4a2 2 0 0 0 2 2h4M16 13H8M16 17H8M10 9H8" } },
                "chevron" => rsx! { path { d: "m6 9 6 6 6-6" } },
                "plus" => rsx! { path { d: "M5 12h14M12 5v14" } },
                "download" => rsx! { path { d: "M12 15V3M21 15v4a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2v-4m-5-5 5 5 5-5" } },
                "readme" => rsx! { path { d: "M12 7v14M3 3h6a3 3 0 0 1 3 3 3 3 0 0 1 3-3h6v18h-6a3 3 0 0 0-3 3 3 3 0 0 0-3-3H3Z" } },
                "edit" => rsx! { path { d: "m16 3 5 5M4 20l4-1L21 6a2.1 2.1 0 0 0-3-3L5 16Z" } },
                "trash" => rsx! { path { d: "M3 6h18M19 6v14a2 2 0 0 1-2 2H7a2 2 0 0 1-2-2V6m3 0V4a2 2 0 0 1 2-2h4a2 2 0 0 1 2 2v2" } },
                "search" => rsx! { circle { cx: "11", cy: "11", r: "8" } path { d: "m21 21-4.3-4.3" } },
                _ => rsx! {
                    path { d: "M12.22 2h-.44a2 2 0 0 0-2 2v.18a2 2 0 0 1-1 1.73l-.43.25a2 2 0 0 1-2 0l-.15-.08a2 2 0 0 0-2.73.73l-.22.38a2 2 0 0 0 .73 2.73l.15.1a2 2 0 0 1 1 1.72v.51a2 2 0 0 1-1 1.74l-.15.09a2 2 0 0 0-.73 2.73l.22.38a2 2 0 0 0 2.73.73l.15-.08a2 2 0 0 1 2 0l.43.25a2 2 0 0 1 1 1.73V20a2 2 0 0 0 2 2h.44a2 2 0 0 0 2-2v-.18a2 2 0 0 1 1-1.73l.43-.25a2 2 0 0 1 2 0l.15.08a2 2 0 0 0 2.73-.73l.22-.38a2 2 0 0 0-.73-2.73l-.15-.09a2 2 0 0 1-1-1.74v-.5a2 2 0 0 1 1-1.74l.15-.09a2 2 0 0 0 .73-2.73l-.22-.38a2 2 0 0 0-2.73-.73l-.15.08a2 2 0 0 1-2 0l-.43-.25a2 2 0 0 1-1-1.73V4a2 2 0 0 0-2-2Z" }
                    circle { cx: "12", cy: "12", r: "3" }
                },
            }
        }
    }
}
