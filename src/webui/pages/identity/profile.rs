//! Public identity profile and repository filtering.
use super::*;
use crate::webui::components::{count_label, Avatar, DraftSelect, SelectOption};

#[expect(
    clippy::too_many_arguments,
    reason = "Pure profile view receives explicit safe presentation and authorized form hints from typed Page dispatch."
)]
pub fn profile_page(
    profile: Profile,
    repositories: Vec<Repository>,
    activity: ProfileActivity,
    token: &str,
    own: bool,
    authenticated: bool,
    following: bool,
    followers: i64,
    follows: i64,
    message: Option<&str>,
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
    let repository_count = repositories.len();
    rsx! {
        // Trusted static CSS only: prerender text-node markers would corrupt style raw text.
        style { dangerous_inner_html: include_str!("profile.css") }
        div { class: "identity-profile",
            nav { class: "identity-profile-tabs", "aria-label": "Profile sections",
                div { class: "identity-profile-tab-inner",
                    a { href: "#profile-repositories", class: "identity-profile-tab identity-profile-tab-current", "aria-label": format!("Repositories ({repository_count})"),
                        {profile_icon(ProfileIcon::Repository)}
                        "Repositories"
                        span { class: "identity-profile-count", "{repository_count}" }
                    }
                    a { href: "#profile-about", class: "identity-profile-tab",
                        {profile_icon(ProfileIcon::User)} "About"
                    }
                }
            }
            div { class: "identity-profile-content",
                if let Some(message) = message {
                    p { role: "status", class: "identity-profile-notice", "{message}" }
                }
                header { class: "identity-profile-masthead",
                    Avatar { name: profile.username.clone(), size: 96 }
                    div { class: "identity-profile-heading",
                        h1 { "{display}" }
                        p { "@{profile.username}" }
                    }
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
                }
                section { id: "profile-activity", class: "identity-profile-activity", "aria-labelledby": "profile-activity-heading",
                    h2 { id: "profile-activity-heading", "Recent activity" }
                    if activity.items.is_empty() {
                        p { class: "identity-profile-empty", "No recent activity." }
                    }
                    ul { class: "identity-profile-activity-list",
                        for (index, item) in activity.items.iter().enumerate() {
                            li { key: "{index}", class: "identity-profile-activity-item",
                                a { href: item.url.clone(), "{activity_text(item)}" }
                                span { class: "identity-profile-activity-repo", " in {item.repository}" }
                                span { class: "identity-profile-activity-date", "{item.created_at.get(..10).unwrap_or_default()}" }
                            }
                        }
                    }
                    if let Some(next) = activity.next_page {
                        a { href: format!("{path}?page={next}#profile-activity"), class: "identity-profile-button", "Show more" }
                    }
                }
                div { class: "identity-profile-grid",
                    ProfileRepositories { key: "{path}", path, repositories, own }
                    aside { id: "profile-about", class: "identity-profile-about", "aria-labelledby": "profile-about-heading",
                        section { class: "identity-profile-about-section",
                            h2 { id: "profile-about-heading", "About" }
                            if !profile.bio.is_empty() { p { class: "identity-profile-bio", "{profile.bio}" } }
                            ul { class: "identity-profile-details",
                                li { {profile_icon(ProfileIcon::User)} span { "@{profile.username}" } }
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
                        section { class: "identity-profile-about-section", "aria-label": "Follow counts",
                            p { class: "identity-profile-follow-counts",
                                {profile_icon(ProfileIcon::People)}
                                span { strong { "{followers}" } " {follower_label}" }
                                span { "aria-hidden": "true", "·" }
                                span { strong { "{follows}" } " following" }
                            }
                        }
                    }
                }
            }
        }
    }
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

/// Filtering is presentation-only: these are the rows already authorized by the server.
/// Default order preserves the server's name ordering; formatted dates are not parsed/sorted.
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
pub fn ProfileRepositories(path: String, repositories: Vec<Repository>, own: bool) -> Element {
    let mut query = use_signal(String::new);
    let mut kind = use_signal(|| "all".to_owned());
    let mut sort = use_signal(|| "name".to_owned());
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
                div { class: "identity-profile-toolbar", role: "search", "aria-label": "Filter listed repositories",
                    label { class: "identity-profile-search",
                        span { class: "identity-profile-sr-only", "Find a listed repository" }
                        {profile_icon(ProfileIcon::Search)}
                        input { r#type: "search", placeholder: "Find a repository…", value: query_value,
                            maxlength: "256", disabled: !ready(), oninput: move |event| query.set(event.value()) }
                    }
                    label { class: "identity-profile-select",
                        span { "Type" }
                        DraftSelect { name: "type", initial_value: kind_value, options: kind_options, disabled: !ready(),
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
                    if own {
                        NavLink { href: "/new".to_owned(), class: "identity-profile-button identity-profile-new",
                            {profile_icon(ProfileIcon::Repository)} "New"
                        }
                    }
                }
            }
            if filtered {
                div { class: "identity-profile-filter-result", role: "status",
                    p { "{rows.len()} of " {count_label(repositories.len(), "repository", "repositories")} }
                    button { r#type: "button", onclick: move |_| { query.set(String::new()); kind.set("all".to_owned()); }, "Clear filters" }
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
                    div { class: "identity-profile-empty", h3 { "No repositories match your filters." } }
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
                                span { "aria-label": format!("{} stars", repo.stars), title: "Stars", {profile_icon(ProfileIcon::Star)} "{repo.stars}" }
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
}

/// Preserve the original profile/navbar/Avatar paths and stroke-based icon style.
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
    };
    rsx! { svg { view_box: "0 0 24 24", fill: "none", stroke: "currentColor", stroke_width: "1.75",
    stroke_linecap: "round", stroke_linejoin: "round", width: "16", height: "16", "aria-hidden": "true", {paths} } }
}
