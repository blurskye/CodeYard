//! Public team page: a team's members or repository grants, as the viewer may see them.
use super::{
    avatar, privacy_label, teams::permission_name, Member, Organization, RepositoryGrant, Team,
};
use crate::webui::{
    components::{count_label, Avatar},
    encode_component,
    navigation::NavLink,
};
use dioxus::prelude::*;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TeamPage {
    pub organization: Organization,
    pub team: Team,
    pub description: String,
    pub member_count: i64,
    pub repository_count: i64,
    /// Owners, admins and maintainers get a link to the management panel, which checks again.
    pub manage: bool,
    /// Shows the repositories tab instead of the members tab.
    pub show_repositories: bool,
    pub members: Vec<Member>,
    pub repositories: Vec<RepositoryGrant>,
    /// True when the shown tab has rows after this page; `after` is the cursor for them.
    pub more: bool,
    pub after: i64,
}

/// The team's public address, from the organization's name and the team's slug.
pub(super) fn href(org: &str, slug: &str) -> String {
    format!(
        "/orgs/{}/teams/{}",
        encode_component(org),
        encode_component(slug)
    )
}

pub fn team_page(page: TeamPage) -> Element {
    let TeamPage {
        organization: org,
        team,
        description,
        member_count,
        repository_count,
        manage,
        show_repositories,
        members,
        repositories,
        more,
        after,
    } = page;
    let org_label = org
        .display_name
        .clone()
        .filter(|name| !name.trim().is_empty())
        .unwrap_or_else(|| org.name.clone());
    let org_path = format!("/{}", encode_component(&org.name));
    let base = href(&org.name, &team.slug);
    let members_href = format!("{base}?tab=members");
    let repositories_href = format!("{base}?tab=repositories");
    let next_href = format!(
        "{base}?tab={}&after={after}",
        if show_repositories {
            "repositories"
        } else {
            "members"
        }
    );
    let manage_href = format!(
        "/settings/organizations/{}?tab=teams&team={}",
        org.id, team.id
    );
    let members_label = count_label(member_count.max(0) as usize, "member", "members");
    let repositories_label = count_label(
        repository_count.max(0) as usize,
        "repository",
        "repositories",
    );
    let current = |active: bool| active.then(|| "page".to_owned());
    rsx! {
        // Trusted static CSS: the header and cards reuse the organization and profile styles.
        style { dangerous_inner_html: include_str!("../identity/profile.css") }
        style { dangerous_inner_html: include_str!("organization.css") }
        div { class: "org-page identity-profile-content",
            header { class: "org-page-header",
                NavLink { href: org_path.clone(), {avatar::picture(&org, "org-avatar", 64)} }
                div {
                    nav { class: "org-crumbs", aria_label: "Organization",
                        NavLink { href: org_path.clone(), class: "org-row-link", "{org_label}" }
                    }
                    h1 { "{team.name}" }
                    if !description.is_empty() { p { class: "org-team-description", "{description}" } }
                    p { class: "org-team-counts",
                        span { class: "org-label", {privacy_label(&team.privacy)} }
                        span { "{members_label}" }
                        span { "{repositories_label}" }
                    }
                    if manage { NavLink { href: manage_href, class: "btn btn-sm", "Manage team" } }
                }
            }
            nav { class: "org-team-tabs", aria_label: "Team sections",
                NavLink { href: members_href, aria_current: current(!show_repositories), "Members" }
                NavLink { href: repositories_href, aria_current: current(show_repositories), "Repositories" }
            }
            section { class: "org-panel org-team-card", aria_labelledby: "team-list-title",
                header { class: "org-panel-header",
                    if show_repositories {
                        h2 { id: "team-list-title", "Repositories" }
                        p { "Repositories this team can access that you can see." }
                    } else {
                        h2 { id: "team-list-title", "People" }
                        p { "People on this team." }
                    }
                }
                ul { class: "org-list",
                    if show_repositories {
                        if repositories.is_empty() {
                            li { class: "org-empty", h3 { "No repositories to show" } p { "This team has no repository access you can see." } }
                        }
                        for grant in &repositories {
                            li { key: "{grant.repo_id}", class: "org-row",
                                div { class: "org-row-main",
                                    h3 { NavLink { href: format!("/{}/{}", encode_component(&grant.owner), encode_component(&grant.name)), class: "org-row-link", "{grant.owner}/{grant.name}" } }
                                }
                                span { class: "org-label", "{permission_name(grant.permission)}" }
                            }
                        }
                    } else {
                        if members.is_empty() {
                            li { class: "org-empty", h3 { "No members to show" } p { "Organization members added to this team appear here." } }
                        }
                        for member in &members {
                            li { key: "{member.user_id}", class: "org-row",
                                div { class: "org-row-main",
                                    h3 { NavLink { href: format!("/{}", encode_component(&member.username)), class: "org-row-link", Avatar { name: member.username.clone(), size: 24 } "{member.username}" } }
                                }
                                span { class: "org-label", "{member.role}" }
                            }
                        }
                    }
                }
                if more { NavLink { href: next_href, class: "org-pagination", "Next" } }
            }
        }
    }
}
