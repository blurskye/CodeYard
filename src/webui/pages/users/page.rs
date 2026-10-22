//! Public identity profile and repository filtering.
use crate::webui::components::{compact_date, count_label, Avatar, DraftSelect, SelectOption};
use crate::webui::navigation::NavLink;
use crate::webui::pages::discovery::{page_links, repository_card, stars_toolbar, RepositoryCard};
use crate::webui::pages::fields::csrf;
use dioxus::prelude::*;

#[expect(
    clippy::too_many_arguments,
    reason = "Pure profile view receives explicit safe presentation and authorized form hints from typed Page dispatch."
)]
pub fn profile_page(
    profile: Profile,
    repositories: Vec<Repository>,
    repository_count: usize,
    activity: ProfileActivity,
    stars: ProfileStars,
    token: &str,
    own: bool,
    authenticated: bool,
    following: bool,
    followers: i64,
    follows: i64,
    message: Option<&str>,
    repositories_newer: Option<String>,
    repositories_older: Option<String>,
) -> Element {
    let path = format!("/{}", crate::webui::encode_component(&profile.username));
    let login = format!("/login?redirect={}", crate::webui::encode_component(&path));
    let display = if profile.display_name.trim().is_empty() {
        profile.username.clone()
    } else {
        profile.display_name.clone()
    };
    let follower_label = if followers == 1 {
        "follower"
    } else {
        "followers"
    };
    let days = activity_days(&activity.items);
    rsx! {
        div { class: "identity-profile",
            div { class: "identity-profile-content",
                if let Some(message) = message {
                    p { role: "status", class: "identity-profile-notice", "{message}" }
                }
                div { class: "identity-profile-grid",
                    aside { class: "identity-profile-sidebar", "aria-label": "Profile",
                        // An uploaded picture is linked by its own address; /avatars/NAME is cached.
                        if let Some(avatar) = profile.avatar_url.as_deref() {
                            img { class: "avatar", src: avatar, width: "256", height: "256", alt: "" }
                        } else {
                            Avatar { name: profile.username.clone(), size: 256 }
                        }
                        div { class: "identity-profile-heading",
                            h1 { "{display}" }
                            p { "@{profile.username}" }
                        }
                        if !profile.bio.is_empty() { p { class: "identity-profile-bio", "{profile.bio}" } }
                        div { class: "identity-profile-follow",
                            if own {
                                NavLink { href: "/settings".to_owned(), class: "identity-profile-button", "Edit profile" }
                            } else if authenticated {
                                form { method: "post", action: format!("{path}/_forms/follow"),
                                    {csrf(token)}
                                    input { r#type: "hidden", name: "action", value: if following { "unfollow" } else { "follow" } }
                                    button { r#type: "submit", class: "identity-profile-button", if following { "Unfollow" } else { "Follow" } }
                                }
                            } else {
                                NavLink { href: login, class: "identity-profile-button", "Sign in to follow" }
                            }
                        }
                        p { class: "identity-profile-follow-counts",
                            {profile_icon(ProfileIcon::People)}
                            span { strong { "{followers}" } " {follower_label}" }
                            span { "aria-hidden": "true", "·" }
                            span { strong { "{follows}" } " following" }
                        }
                        ul { class: "identity-profile-details",
                            if !profile.location.is_empty() {
                                li { {profile_icon(ProfileIcon::Location)} span { "{profile.location}" } }
                            }
                            if !profile.website.is_empty() {
                                li {
                                    {profile_icon(ProfileIcon::Link)}
                                    if let Some(website) = safe_website(&profile.website) {
                                        a { href: website, target: "_blank", rel: "noopener noreferrer", "{website}" }
                                    } else { span { "{profile.website}" } }
                                }
                            }
                            if !profile.joined.is_empty() {
                                li { {profile_icon(ProfileIcon::Calendar)} span { "Joined {profile.joined}" } }
                            }
                        }
                    }
                    div { class: "identity-profile-main",
                        nav { class: "identity-profile-tabs", "aria-label": "Profile sections",
                            div { class: "identity-profile-tab-inner",
                                a { href: path.clone(), class: if stars.open { "identity-profile-tab" } else { "identity-profile-tab identity-profile-tab-current" }, "aria-current": if stars.open { "false" } else { "page" }, "aria-label": format!("Repositories ({repository_count})"),
                                    {profile_icon(ProfileIcon::Repository)}
                                    "Repositories"
                                    span { class: "identity-profile-count", "{repository_count}" }
                                }
                                a { href: format!("{path}?tab=stars"), class: if stars.open { "identity-profile-tab identity-profile-tab-current" } else { "identity-profile-tab" }, "aria-current": if stars.open { "page" } else { "false" }, "aria-label": format!("Stars ({})", stars.count),
                                    {profile_icon(ProfileIcon::Star)}
                                    "Stars"
                                    span { class: "identity-profile-count", "{stars.count}" }
                                }
                            }
                        }
                        if stars.open {
                            ProfileStarList { path: path.clone(), stars: stars.clone(), own }
                        } else {
                            ProfileRepositories { key: "{path}", path: path.clone(), repositories, own, new_href: own.then(|| "/new".to_owned()) }
                            {page_links(repositories_newer, repositories_older, "Profile repository pages")}
                            section { id: "profile-activity", class: "identity-profile-activity", "aria-labelledby": "profile-activity-heading",
                                h2 { id: "profile-activity-heading", "Recent activity" }
                                if days.is_empty() {
                                    p { class: "identity-profile-empty", "No recent activity." }
                                }
                                for (day, items) in days {
                                    section { class: "identity-profile-activity-day", "aria-label": compact_date(day),
                                        h3 { class: "identity-profile-activity-date", "{compact_date(day)}" }
                                        ul { class: "identity-profile-activity-list",
                                            for (index, item) in items.into_iter().enumerate() {
                                                li { key: "{day}-{index}", class: "identity-profile-activity-item",
                                                    span { class: "identity-profile-activity-icon", {activity_icon(&item.kind)} }
                                                    a { class: "identity-profile-activity-title", href: item.url.clone(), "{activity_text(item)}" }
                                                    a { class: "identity-profile-activity-repo", href: repository_href(&item.repository), "{item.repository}" }
                                                }
                                            }
                                        }
                                    }
                                }
                                if let Some(next) = activity.next_page {
                                    a { href: format!("{path}?page={next}#profile-activity"), class: "identity-profile-button", "Show more" }
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}

/// Groups one page of activity by the day it happened (UTC). The server lists the newest first,
/// so the items of one day are always next to each other.
fn activity_days(items: &[ProfileActivityItem]) -> Vec<(&str, Vec<&ProfileActivityItem>)> {
    let mut days: Vec<(&str, Vec<&ProfileActivityItem>)> = Vec::new();
    for item in items.iter().filter(|item| shows_in_feed(item)) {
        let day = item.created_at.get(..10).unwrap_or_default();
        match days.last_mut() {
            Some((last, group)) if *last == day => group.push(item),
            _ => days.push((day, vec![item])),
        }
    }
    days
}

/// A push with no commits says nothing about the work, so the feed does not list it.
fn shows_in_feed(item: &ProfileActivityItem) -> bool {
    item.kind != "push" || item.commits.unwrap_or_default() > 0
}

fn activity_icon(kind: &str) -> Element {
    let icon = match kind {
        "push" => ProfileIcon::Commit,
        "issue_opened" | "issue_comment" => ProfileIcon::Issue,
        "pull_request_opened" | "pull_request_merged" | "pull_request_comment" => {
            ProfileIcon::PullRequest
        }
        _ => ProfileIcon::Repository,
    };
    profile_icon(icon)
}

/// `owner/name` as a link path, with each part encoded the same way as the repository links.
fn repository_href(repository: &str) -> String {
    let parts: Vec<String> = repository
        .split('/')
        .map(crate::webui::encode_component)
        .collect();
    format!("/{}", parts.join("/"))
}

fn activity_text(item: &ProfileActivityItem) -> String {
    let number = item.number.unwrap_or_default();
    let title = item.title.as_deref().unwrap_or_default();
    match item.kind.as_str() {
        "push" => {
            let commits = item.commits.unwrap_or_default();
            let plural = if commits == 1 { "" } else { "s" };
            let branch = item.branch.as_deref().unwrap_or_default();
            format!("Pushed {commits} commit{plural} to {branch}")
        }
        "repository_created" => "Created repository".to_owned(),
        "issue_opened" => format!("Opened issue #{number}: {title}"),
        "pull_request_opened" => format!("Opened pull request #{number}: {title}"),
        "pull_request_merged" => format!("Merged pull request #{number}: {title}"),
        "issue_comment" => format!("Commented on issue #{number}"),
        "pull_request_comment" => format!("Commented on pull request #{number}"),
        _ => "Updated".to_owned(),
    }
}

/// Filtering is presentation-only: these are the rows already authorized by the server, and only
/// the ones on the current page. Default order preserves the server's name ordering; formatted
/// dates are not parsed/sorted.
pub(super) fn profile_repository_rows<'a>(
    repositories: &'a [Repository],
    query: &str,
    kind: &str,
    sort: &str,
) -> Vec<&'a Repository> {
    let query = query.trim().to_lowercase();
    let mut rows: Vec<_> = repositories
        .iter()
        .filter(|repo| match kind {
            "public" => !repo.private,
            "private" => repo.private,
            "archived" => repo.archived,
            "active" => !repo.archived,
            _ => true,
        })
        .filter(|repo| {
            query.is_empty()
                || repo.name.to_lowercase().contains(&query)
                || repo.description.to_lowercase().contains(&query)
        })
        .collect();
    match sort {
        "stars" => rows.sort_by(|a, b| b.stars.cmp(&a.stars).then_with(|| a.name.cmp(&b.name))),
        "forks" => rows.sort_by(|a, b| b.forks.cmp(&a.forks).then_with(|| a.name.cmp(&b.name))),
        _ => {}
    }
    rows
}

#[component]
pub fn ProfileRepositories(
    path: String,
    repositories: Vec<Repository>,
    own: bool,
    // Where the New repository button goes; no button without it.
    #[props(default)] new_href: Option<String>,
) -> Element {
    let mut query = use_signal(String::new);
    let mut kind = use_signal(|| "all".to_owned());
    let mut sort = use_signal(|| "name".to_owned());
    // Clearing the filters remounts the Type menu, which keeps its own copy of the choice.
    let mut cleared = use_signal(|| 0_u32);
    let mut ready = use_signal(|| false);
    use_effect(move || ready.set(true));
    let query_value = query();
    let kind_value = kind();
    let sort_value = sort();
    let rows = profile_repository_rows(&repositories, &query_value, &kind_value, &sort_value);
    let filtered = !query_value.trim().is_empty() || kind_value != "all";
    let has_private = repositories.iter().any(|repo| repo.private);
    let has_archived = repositories.iter().any(|repo| repo.archived);
    let mut kind_options = vec![
        SelectOption::new("all", "All"),
        SelectOption::new("public", "Public"),
    ];
    if has_private {
        kind_options.push(SelectOption::new("private", "Private"));
    }
    if has_archived {
        kind_options.push(SelectOption::new("active", "Active"));
        kind_options.push(SelectOption::new("archived", "Archived"));
    }
    rsx! {
        section { id: "profile-repositories", class: "identity-profile-repositories", "aria-label": "Repositories",
            if !repositories.is_empty() {
                div { class: "identity-profile-toolbar", role: "search", "aria-label": "Filter the repositories on this page",
                    label { class: "identity-profile-search",
                        span { class: "identity-profile-sr-only", "Filter the repositories on this page" }
                        {profile_icon(ProfileIcon::Search)}
                        input { r#type: "search", placeholder: "Filter this page…", value: query_value,
                            maxlength: "256", disabled: !ready(), oninput: move |event| query.set(event.value()) }
                    }
                    label { class: "identity-profile-select",
                        span { "Type" }
                        DraftSelect { key: "type-{cleared}", name: "type", initial_value: kind_value, options: kind_options, disabled: !ready(),
                            aria_label: Some("Repository type".to_owned()), on_value: move |value: String| kind.set(value) }
                    }
                    label { class: "identity-profile-select",
                        span { "Sort" }
                        DraftSelect { name: "sort", initial_value: sort_value, disabled: !ready(),
                            aria_label: Some("Repository sort order".to_owned()), on_value: move |value: String| sort.set(value),
                            options: vec![
                                SelectOption::new("name", "Name"),
                                SelectOption::new("stars", "Stars"),
                                SelectOption::new("forks", "Forks"),
                            ] }
                    }
                    if let Some(href) = new_href {
                        NavLink { href, class: "identity-profile-button identity-profile-new",
                            {profile_icon(ProfileIcon::Repository)} "New repository"
                        }
                    }
                }
            }
            if filtered {
                div { class: "identity-profile-filter-result", role: "status",
                    p { "{rows.len()} of " {count_label(repositories.len(), "repository", "repositories")} " on this page" }
                    button { r#type: "button", onclick: move |_| { query.set(String::new()); kind.set("all".to_owned()); cleared.set(cleared() + 1); }, "Clear filters" }
                }
            }
            div { class: "identity-profile-repository-list",
                if repositories.is_empty() {
                    div { class: "identity-profile-empty",
                        {profile_icon(ProfileIcon::Repository)}
                        h3 { if own { "You don't have any repositories yet." } else { "No repositories visible to you." } }
                        if own { NavLink { href: "/new".to_owned(), class: "identity-profile-button identity-profile-new", "Create a repository" } }
                    }
                } else if rows.is_empty() {
                    div { class: "identity-profile-empty", h3 { "No repositories on this page match your filters." } }
                } else {
                    for repo in rows {
                        article { key: "{repo.name}", class: "identity-profile-repository-row",
                            h3 {
                                NavLink { href: format!("{path}/{}", crate::webui::encode_component(&repo.name)), class: "identity-profile-repository-name", "{repo.name}" }
                                span { class: "identity-profile-visibility", if repo.private { "Private" } else { "Public" } }
                                if repo.archived { span { class: "identity-profile-visibility", "Archived" } }
                            }
                            if !repo.description.is_empty() { p { class: "identity-profile-repository-description", "{repo.description}" } }
                            div { class: "identity-profile-repository-meta",
                                span { "aria-label": count_label(repo.stars.max(0) as usize, "star", "stars"), title: "Stars", {profile_icon(ProfileIcon::Star)} "{repo.stars}" }
                                span { "aria-label": format!("{} forks", repo.forks), title: "Forks", {profile_icon(ProfileIcon::Fork)} "{repo.forks}" }
                                if !repo.updated.is_empty() { span { "Updated {repo.updated}" } }
                            }
                        }
                    }
                }
            }
        }
    }
}

#[derive(Clone, Copy)]
pub(crate) enum ProfileIcon {
    Repository,
    User,
    People,
    Location,
    Link,
    Calendar,
    Search,
    Star,
    Fork,
    Commit,
    Issue,
    PullRequest,
    Appearance,
    Terminal,
    Key,
    Lock,
    Globe,
    Shield,
    Building,
    Layers,
    Chart,
    Gear,
    Mail,
    Server,
    Database,
    Play,
}

/// Preserve the original profile/navbar/Avatar paths and stroke-based icon style.
/// The address of one page of a profile's Stars tab, with its order and text. Defaults stay out of
/// the address, and page 1 has no page number.
fn stars_href(path: &str, stars: &ProfileStars, page: usize) -> String {
    let mut parts = vec!["tab=stars".to_owned()];
    if !stars.sort.is_empty() {
        parts.push(format!(
            "sort={}",
            crate::webui::encode_component(&stars.sort)
        ));
    }
    if !stars.query.is_empty() {
        parts.push(format!(
            "q={}",
            crate::webui::encode_component(&stars.query)
        ));
    }
    if page > 1 {
        parts.push(format!("page={page}"));
    }
    format!("{path}?{}", parts.join("&"))
}

/// The Stars tab: the repositories the user starred that the viewer can read, as the Explore
/// list shows them, with the same filter and sort bar as /stars.
#[component]
pub fn ProfileStarList(path: String, stars: ProfileStars, own: bool) -> Element {
    let newer = (stars.page > 1).then(|| stars_href(&path, &stars, stars.page - 1));
    let older = stars
        .has_older
        .then(|| stars_href(&path, &stars, stars.page + 1));
    let query = stars.query.clone();
    rsx! {
        section { id: "profile-stars", class: "identity-profile-repositories", "aria-label": "Starred repositories",
            {stars_toolbar("profile-stars-filters", &path, &stars.query, &stars.sort, rsx! {
                input { r#type: "hidden", name: "tab", value: "stars" }
            })}
            if stars.repositories.is_empty() {
                p { class: "identity-profile-empty",
                    if !query.is_empty() { "No starred repositories match “{query}”." } else if own { "You have not starred any repository you can read yet." } else { "No starred repositories visible to you." }
                }
            }
            for repo in &stars.repositories { {repository_card(repo)} }
            {page_links(newer, older, "Starred repository pages")}
        }
    }
}

pub(crate) fn profile_icon(icon: ProfileIcon) -> Element {
    let paths = match icon {
        ProfileIcon::Repository => rsx! {
            path { d: "M4 19.5v-15A2.5 2.5 0 0 1 6.5 2H20v20H6.5a2.5 2.5 0 0 1 0-5H20" }
            path { d: "M10 6h6" } path { d: "M10 10h6" }
        },
        ProfileIcon::User => rsx! {
            path { d: "M20 21v-2a4 4 0 0 0-4-4H8a4 4 0 0 0-4 4v2" }
            circle { cx: "12", cy: "7", r: "4" }
        },
        ProfileIcon::People => rsx! {
            path { d: "M16 21v-2a4 4 0 0 0-4-4H6a4 4 0 0 0-4 4v2" }
            circle { cx: "9", cy: "7", r: "4" }
            path { d: "M22 21v-2a4 4 0 0 0-3-3.87" }
            path { d: "M16 3.13a4 4 0 0 1 0 7.75" }
        },
        ProfileIcon::Location => rsx! {
            path { d: "M20 10c0 4.993-5.539 10.193-7.399 11.799a1 1 0 0 1-1.202 0C9.539 20.193 4 14.993 4 10a8 8 0 0 1 16 0" }
            circle { cx: "12", cy: "10", r: "3" }
        },
        ProfileIcon::Link => rsx! {
            path { d: "M9 17H7A5 5 0 0 1 7 7h2" } path { d: "M15 7h2a5 5 0 1 1 0 10h-2" } path { d: "M8 12h8" }
        },
        ProfileIcon::Calendar => rsx! {
            path { d: "M8 2v4" } path { d: "M16 2v4" }
            rect { x: "3", y: "4", width: "18", height: "18", rx: "2" } path { d: "M3 10h18" }
        },
        ProfileIcon::Search => {
            rsx! { circle { cx: "11", cy: "11", r: "8" } path { d: "m21 21-4.3-4.3" } }
        }
        ProfileIcon::Star => {
            rsx! { path { d: "M12 2l3.09 6.26L22 9.27l-5 4.87 1.18 6.88L12 17.77l-6.18 3.25L7 14.14 2 9.27l6.91-1.01L12 2z" } }
        }
        ProfileIcon::Fork => rsx! {
            circle { cx: "12", cy: "18", r: "3" } circle { cx: "6", cy: "6", r: "3" } circle { cx: "18", cy: "6", r: "3" }
            path { d: "M18 9v2c0 .6-.4 1-1 1H7c-.6 0-1-.4-1-1V9" } path { d: "M12 12v3" }
        },
        ProfileIcon::Commit => rsx! {
            circle { cx: "12", cy: "12", r: "3" } path { d: "M3 12h6" } path { d: "M15 12h6" }
        },
        ProfileIcon::Issue => rsx! {
            circle { cx: "12", cy: "12", r: "10" } circle { cx: "12", cy: "12", r: "1" }
        },
        ProfileIcon::PullRequest => rsx! {
            circle { cx: "18", cy: "18", r: "3" } circle { cx: "6", cy: "6", r: "3" }
            path { d: "M13 6h3a2 2 0 0 1 2 2v7" } path { d: "M6 9v12" }
        },
        ProfileIcon::Appearance => rsx! {
            circle { cx: "12", cy: "12", r: "4" }
            path { d: "M12 2v2M12 20v2M4.93 4.93l1.41 1.41M17.66 17.66l1.41 1.41M2 12h2M20 12h2M6.34 17.66l-1.41 1.41M19.07 4.93l-1.41 1.41" }
        },
        ProfileIcon::Terminal => rsx! { path { d: "M4 17l6-6-6-6" } path { d: "M12 19h8" } },
        ProfileIcon::Key => rsx! {
            circle { cx: "7.5", cy: "15.5", r: "5.5" } path { d: "M21 2l-9.6 9.6" } path { d: "M15.5 7.5l3 3L22 7l-3-3" }
        },
        ProfileIcon::Lock => rsx! {
            rect { x: "3", y: "11", width: "18", height: "11", rx: "2" } path { d: "M7 11V7a5 5 0 0 1 10 0v4" }
        },
        ProfileIcon::Globe => rsx! {
            circle { cx: "12", cy: "12", r: "10" } path { d: "M12 2a14.5 14.5 0 0 0 0 20 14.5 14.5 0 0 0 0-20" } path { d: "M2 12h20" }
        },
        ProfileIcon::Shield => rsx! { path { d: "M12 22s8-4 8-10V5l-8-3-8 3v7c0 6 8 10 8 10z" } },
        ProfileIcon::Building => rsx! {
            path { d: "M6 22V4a2 2 0 0 1 2-2h8a2 2 0 0 1 2 2v18Z" }
            path { d: "M6 12H4a2 2 0 0 0-2 2v6a2 2 0 0 0 2 2h2" }
            path { d: "M18 9h2a2 2 0 0 1 2 2v9a2 2 0 0 1-2 2h-2" }
            path { d: "M10 6h4M10 10h4M10 14h4M10 18h4" }
        },
        ProfileIcon::Layers => rsx! {
            path { d: "m12 2 10 5-10 5L2 7l10-5Z" } path { d: "m2 17 10 5 10-5" } path { d: "m2 12 10 5 10-5" }
        },
        ProfileIcon::Chart => {
            rsx! { path { d: "M3 3v18h18" } path { d: "M8 17v-5" } path { d: "M13 17V8" } path { d: "M18 17v-8" } }
        }
        ProfileIcon::Gear => rsx! {
            path { d: "M12.22 2h-.44a2 2 0 0 0-2 2v.18a2 2 0 0 1-1 1.73l-.43.25a2 2 0 0 1-2 0l-.15-.08a2 2 0 0 0-2.73.73l-.22.38a2 2 0 0 0 .73 2.73l.15.1a2 2 0 0 1 1 1.72v.51a2 2 0 0 1-1 1.74l-.15.09a2 2 0 0 0-.73 2.73l.22.38a2 2 0 0 0 2.73.73l.15-.08a2 2 0 0 1 2 0l.43.25a2 2 0 0 1 1 1.73V20a2 2 0 0 0 2 2h.44a2 2 0 0 0 2-2v-.18a2 2 0 0 1 1-1.73l.43-.25a2 2 0 0 1 2 0l.15.08a2 2 0 0 0 2.73-.73l.22-.39a2 2 0 0 0-.73-2.73l-.15-.08a2 2 0 0 1-1-1.74v-.5a2 2 0 0 1 1-1.74l.15-.09a2 2 0 0 0 .73-2.73l-.22-.38a2 2 0 0 0-2.73-.73l-.15.08a2 2 0 0 1-2 0l-.43-.25a2 2 0 0 1-1-1.73V4a2 2 0 0 0-2-2z" }
            circle { cx: "12", cy: "12", r: "3" }
        },
        ProfileIcon::Mail => {
            rsx! { rect { x: "2", y: "4", width: "20", height: "16", rx: "2" } path { d: "m22 7-10 6L2 7" } }
        }
        ProfileIcon::Server => rsx! {
            rect { x: "2", y: "2", width: "20", height: "8", rx: "2" } rect { x: "2", y: "14", width: "20", height: "8", rx: "2" }
            path { d: "M6 6h.01M6 18h.01" }
        },
        ProfileIcon::Database => rsx! {
            ellipse { cx: "12", cy: "5", rx: "9", ry: "3" } path { d: "M3 5v14a9 3 0 0 0 18 0V5" } path { d: "M3 12a9 3 0 0 0 18 0" }
        },
        ProfileIcon::Play => rsx! {
            circle { cx: "12", cy: "12", r: "9" } path { d: "m10 8 6 4-6 4z" }
        },
    };
    rsx! { svg { view_box: "0 0 24 24", fill: "none", stroke: "currentColor", stroke_width: "1.75",
    stroke_linecap: "round", stroke_linejoin: "round", width: "16", height: "16", "aria-hidden": "true", {paths} } }
}

#[derive(Clone, Debug, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Profile {
    pub username: String,
    pub display_name: String,
    pub bio: String,
    pub location: String,
    pub website: String,
    pub avatar_url: Option<String>,
    pub joined: String,
}
/// One item of a profile's recent activity. `url` leads to what it is about.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ProfileActivityItem {
    pub kind: String,
    pub created_at: String,
    pub repository: String,
    pub url: String,
    pub number: Option<i32>,
    pub title: Option<String>,
    pub branch: Option<String>,
    pub commits: Option<i32>,
}

/// A page of recent activity. `next_page` is set when another page has items.
#[derive(Clone, Debug, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ProfileActivity {
    pub items: Vec<ProfileActivityItem>,
    pub next_page: Option<i64>,
}

#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Repository {
    pub name: String,
    pub description: String,
    pub private: bool,
    pub archived: bool,
    pub stars: i64,
    pub forks: i64,
    pub updated: String,
}

/// The Stars tab of a profile. `count` is how many of the user's stars the viewer can read. The
/// list is filled only while the tab is open, with one page of it at `page`.
#[derive(Clone, Debug, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ProfileStars {
    pub count: i64,
    pub open: bool,
    pub page: usize,
    pub has_older: bool,
    pub repositories: Vec<RepositoryCard>,
    /// The order and the text the list is narrowed to.
    #[serde(default)]
    pub sort: String,
    #[serde(default)]
    pub query: String,
}

/// Whitelist public website link schemes. Unsafe stored values remain editable text.
pub(super) fn safe_website(raw: &str) -> Option<&str> {
    let value = raw.trim();
    let has_scheme = |scheme: &str| {
        value
            .get(..scheme.len())
            .is_some_and(|head| head.eq_ignore_ascii_case(scheme))
    };
    if !value.chars().any(|c| c.is_control() || c == '\\')
        && (has_scheme("https://") || has_scheme("http://"))
    {
        let authority = value.split_once("://")?.1.split(['/', '?', '#']).next()?;
        if !authority.is_empty() && !authority.contains('@') {
            return Some(value);
        }
    }
    None
}
