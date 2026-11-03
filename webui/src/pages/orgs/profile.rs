//! Public organization page: the organization, its repositories, and for members its people and teams.
use super::{avatar, privacy_label, team_page, Member, Organization, Team};
use crate::{
    components::{count_label, Avatar},
    encode_component,
    navigation::NavLink,
    pages::users::{profile_icon, ProfileIcon, ProfileRepositories, Repository},
};
use dioxus::prelude::*;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ProfileContacts {
    pub company: String,
    pub email: String,
    pub twitter_username: String,
    pub location: String,
    pub blog: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OrganizationProfile {
    #[serde(default)]
    pub contacts: ProfileContacts,
    #[serde(default)]
    pub can_create: bool,
    pub organization: Organization,
    /// Already filtered by the server to the repositories this viewer may see.
    pub repositories: Vec<Repository>,
    pub repository_count: i64,
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
        contacts,
        can_create,
        organization: org,
        repositories,
        repository_count,
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
    let path = crate::profile_path(&org.name);
    let manage_href = format!("/settings/organizations/{}", org.id);
    let repository_label = count_label(
        repository_count.max(0) as usize,
        "repository",
        "repositories",
    );
    // Readers who may not see the member count get the same two-part line, not a bare count.
    let meta = match member_count {
        Some(count) => format!(
            "{repository_label} · {}",
            count_label(count.max(0) as usize, "member", "members")
        ),
        None => format!("{repository_label} · Public organization"),
    };
    // Eligible members can create repositories without gaining governance controls.
    let can_create = can_create || manage;
    let empty = if can_create {
        "This organization has no repositories yet."
    } else {
        "No public repositories yet."
    };
    let new_href = format!("/new?owner_org_id={}", org.id);
    let members_visible = member_count.is_some();
    let member_total = member_count.unwrap_or_default();
    let team_total = team_count.unwrap_or_default();
    let repository_total = repository_count;
    let current = |active: bool| active.then(|| "page".to_owned());
    rsx! {
        div { class: "identity-profile",
            div { class: "identity-profile-content",
                header { class: "org-profile-header",
                    {avatar::picture(&org, "org-avatar", 64)}
                    div { class: "identity-profile-heading",
                        h1 { "{label}" }
                        p { "@{org.name}" }
                        if !org.description.is_empty() { p { class: "identity-profile-bio", "{org.description}" } }
                        p { class: "org-profile-meta", "{meta}" }
                        if !contacts.company.is_empty() {p {class:"org-profile-meta break-words","{contacts.company}"}}
                        if !contacts.location.is_empty() {p {class:"org-profile-meta break-words","{contacts.location}"}}
                        if !contacts.email.is_empty() {p {class:"org-profile-meta break-words",a {href:format!("mailto:{}",contacts.email),"{contacts.email}"}}}
                        if !contacts.twitter_username.is_empty() {p {class:"org-profile-meta break-words",a {href:format!("https://twitter.com/{}",encode_component(&contacts.twitter_username)),rel:"nofollow noopener noreferrer",target:"_blank","@{contacts.twitter_username}"}}}
                        if !contacts.blog.is_empty() {p {class:"org-profile-meta break-words",
                            if codeyard_support::website::parse(&contacts.blog).is_some() {a {href:contacts.blog.clone(),rel:"nofollow noopener noreferrer",target:"_blank","{contacts.blog}"}} else {"{contacts.blog}"}
                        }}
                    }
                    if manage {
                        NavLink { href: manage_href, class: "identity-profile-button", "Manage organization" }
                    }
                }
                nav { class: "org-tabs", aria_label: "Organization sections",
                    NavLink { href: format!("{path}?tab=repositories"), aria_current: current(tab == "repositories"), "Repositories" span { class: "count-badge", "{repository_total}" } }
                    NavLink { href: format!("/orgs/{}/packages",crate::encode_component(&org.name)), "Packages" }
                    NavLink { href: format!("/orgs/{}/attestations",crate::encode_component(&org.name)), "Attestations" }
                    NavLink { href: format!("/orgs/{}/projects",crate::encode_component(&org.name)), "Projects" }
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
                                            h3 { NavLink { href: crate::profile_path(&member.username), class: "org-row-link", Avatar { name: member.username.clone(), size: 24 } "{member.username}" } }
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
                            if can_create { NavLink { href: new_href.clone(), class: "identity-profile-button identity-profile-new", "New repository" } }
                        }
                    }
                } else {
                    ProfileRepositories { key: "{path}", path: path.clone(), repositories, own: false, new_href: can_create.then(|| new_href.clone()) }
                    if more { NavLink { href: format!("{path}?tab=repositories&after={after}"), class: "org-pagination", "Next" } }
                }
            }
        }
    }
}
