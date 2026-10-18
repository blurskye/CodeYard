//! Public organization page: the organization, its repositories, and for members its people and teams.
use super::{avatar, privacy_label, team_page, Member, Organization, Team};
use crate::webui::{
    components::{count_label, Avatar},
    encode_component,
    navigation::NavLink,
    pages::users::{profile_icon, ProfileIcon, ProfileRepositories, Repository},
};
use dioxus::prelude::*;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OrganizationProfile {
    pub organization: Organization,
    /// Already filtered by the server to the repositories this viewer may see.
    pub repositories: Vec<Repository>,
    /// Members, counted only for readers who may see them; `None` hides the count and the
    /// People and Teams tabs.
    pub member_count: Option<i64>,
    /// Teams, counted only for the same readers as `member_count`; `None` hides the Teams count.
    pub team_count: Option<i64>,
    /// Owners and admins get a link to the management page and the new-repository button.
    /// The management and creation pages check authority again.
    pub manage: bool,
    /// The tab the URL names: `repositories`, `people` or `teams`.
    pub tab: String,
    /// One page of members for the People tab; empty unless that tab is shown.
    pub people: Vec<Member>,
    /// One page of teams for the Teams tab; empty unless that tab is shown.
    pub teams: Vec<Team>,
    /// True when the shown tab has rows after this page; `after` is the cursor for them.
    pub more: bool,
    pub after: i64,
}

pub fn profile_page(page: OrganizationProfile) -> Element {
    let OrganizationProfile {
        organization: org,
        repositories,
        member_count,
        team_count,
        manage,
        tab,
        people,
        teams,
        more,
        after,
    } = page;
    let label = org
        .display_name
        .clone()
        .filter(|name| !name.trim().is_empty())
        .unwrap_or_else(|| org.name.clone());
    let path = format!("/{}", crate::webui::encode_component(&org.name));
    let manage_href = format!("/settings/organizations/{}", org.id);
    let repository_count = count_label(repositories.len(), "repository", "repositories");
    // Readers who may not see the member count get the same two-part line, not a bare count.
    let meta = match member_count {
        Some(count) => format!(
            "{repository_count} · {}",
            count_label(count.max(0) as usize, "member", "members")
        ),
        None => format!("{repository_count} · Public organization"),
    };
    // Owners and admins are the people who may create repositories in the organization.
    let empty = if manage {
        "This organization has no repositories yet."
    } else {
        "No public repositories yet."
    };
    let new_href = format!("/new?owner_org_id={}", org.id);
    let members_visible = member_count.is_some();
    let member_total = member_count.unwrap_or_default();
    let team_total = team_count.unwrap_or_default();
    let repository_total = repositories.len();
    let current = |active: bool| active.then(|| "page".to_owned());
    rsx! {
        // Trusted static CSS: the repository list reuses the profile styles.
        style { dangerous_inner_html: include_str!("../account.css") }
        style { dangerous_inner_html: include_str!("organization.css") }
        div { class: "identity-profile",
            div { class: "identity-profile-content",
                header { class: "org-profile-header",
                    {avatar::picture(&org, "org-avatar", 64)}
                    div { class: "identity-profile-heading",
                        h1 { "{label}" }
                        p { "@{org.name}" }
                        if !org.description.is_empty() { p { class: "identity-profile-bio", "{org.description}" } }
                        p { class: "org-profile-meta", "{meta}" }
                    }
                    if manage {
                        NavLink { href: manage_href, class: "identity-profile-button", "Manage organization" }
                    }
                }
                nav { class: "org-tabs", aria_label: "Organization sections",
                    NavLink { href: format!("{path}?tab=repositories"), aria_current: current(tab == "repositories"), "Repositories" span { class: "count-badge", "{repository_total}" } }
                    if members_visible {
                        NavLink { href: format!("{path}?tab=people"), aria_current: current(tab == "people"), "People" span { class: "count-badge", "{member_total}" } }
                        NavLink { href: format!("{path}?tab=teams"), aria_current: current(tab == "teams"), "Teams" span { class: "count-badge", "{team_total}" } }
                    }
                }
                if tab == "people" {
                    section { id: "profile-people", class: "identity-profile-repositories", aria_label: "People",
                        if people.is_empty() {
                            div { class: "identity-profile-empty", {profile_icon(ProfileIcon::People)} h3 { "No people to show" } }
                        } else {
                            ul { class: "org-list",
                                for member in &people {
                                    li { key: "{member.user_id}", class: "org-row",
                                        div { class: "org-row-main",
                                            h3 { NavLink { href: format!("/{}", encode_component(&member.username)), class: "org-row-link", Avatar { name: member.username.clone(), size: 24 } "{member.username}" } }
                                        }
                                        span { class: "org-label", "{member.role}" }
                                    }
                                }
                            }
                        }
                        if more { NavLink { href: format!("{path}?tab=people&after={after}"), class: "org-pagination", "Next" } }
                    }
                } else if tab == "teams" {
                    section { id: "profile-teams", class: "identity-profile-repositories", aria_label: "Teams",
                        if teams.is_empty() {
                            div { class: "identity-profile-empty", {profile_icon(ProfileIcon::People)} h3 { "No teams yet." } }
                        } else {
                            ul { class: "org-list",
                                for team in &teams {
                                    li { key: "{team.id}", class: "org-row",
                                        div { class: "org-row-main",
                                            h3 { NavLink { href: team_page::href(&org.name, &team.slug), class: "org-row-link", "{team.name}" } }
                                            if !team.description.is_empty() { p { "{team.description}" } }
                                        }
                                        span { class: "org-label", {privacy_label(&team.privacy)} }
                                    }
                                }
                            }
                        }
                        if more { NavLink { href: format!("{path}?tab=teams&after={after}"), class: "org-pagination", "Next" } }
                    }
                } else if repositories.is_empty() {
                    section { id: "profile-repositories", class: "identity-profile-repositories", aria_label: "Repositories",
                        div { class: "identity-profile-empty", {profile_icon(ProfileIcon::Repository)} h3 { "{empty}" }
                            if manage { NavLink { href: new_href, class: "identity-profile-button identity-profile-new", "New repository" } }
                        }
                    }
                } else {
                    ProfileRepositories { key: "{path}", path, repositories, own: false }
                }
            }
        }
    }
}
