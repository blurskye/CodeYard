//! Safe organization presentation models shared by SSR and normal hydration.
use super::fields::{controls, input};
use super::{avatar, governance, membership, teams};
use crate::webui::components::webhooks::{WebhookPanel, WebhookSection};
use crate::webui::{
    components::count_label,
    navigation::NavLink,
    pages::{
        settings,
        users::{profile_icon, ProfileIcon},
    },
};
use dioxus::prelude::*;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Organization {
    pub id: i64,
    pub name: String,
    pub display_name: Option<String>,
    pub description: String,
    pub suspended: bool,
    /// Where the organization's picture is served from, once one is set.
    pub avatar_url: Option<String>,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Member {
    pub user_id: i64,
    pub username: String,
    pub role: String,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Team {
    pub id: i64,
    pub name: String,
    pub slug: String,
    pub description: String,
    pub privacy: String,
    /// Active members of the team, counted for the organization's own readers.
    pub member_count: i64,
}
/// The label people read for a team's privacy. Team pages are for organization members only,
/// so "visible" means visible to the whole organization.
pub(super) fn privacy_label(privacy: &str) -> &str {
    match privacy {
        "visible" => "Visible to organization members",
        "secret" => "Only team members",
        other => other,
    }
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RepositoryGrant {
    pub repo_id: i64,
    pub owner: String,
    pub name: String,
    pub permission: i16,
}
/// Repositories listed in a team grant's select. One more readable repository than this
/// switches the grant form to a typed repository ID.
pub const REPOSITORY_CHOICE_LIMIT: i64 = 100;
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RepositoryChoice {
    pub repo_id: i64,
    pub owner: String,
    pub name: String,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TeamDetail {
    pub team: Team,
    pub slug: String,
    pub description: String,
    pub members: Vec<Member>,
    pub repositories: Vec<RepositoryGrant>,
    /// True when another page of members, or of repositories, follows this one.
    pub more_members: bool,
    pub more_repositories: bool,
    pub members_after: i64,
    pub repositories_after: i64,
    pub members_cursor: i64,
    pub repositories_cursor: i64,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Page {
    pub csrf_token: String,
    pub organizations: Vec<Organization>,
    pub selected: Option<Organization>,
    /// Owners and admins manage the organization; everyone else gets the read-only view.
    /// Presentation only: every change rechecks the authority it needs.
    pub can_manage: bool,
    /// Owners may change any member's role and remove anyone; admins manage plain members.
    pub is_owner: bool,
    /// The signed-in viewer. Their own row asks for their username before removal.
    pub viewer_id: i64,
    /// Shows the Administration section of the account settings navigation.
    pub manage_access: bool,
    /// The section the URL names: `overview`, `members`, `teams` or `webhooks`. The others render hidden.
    pub tab: String,
    /// The webhooks section, built only on the webhooks tab of an owner. Boxed: most pages lack it.
    pub webhooks: Option<Box<WebhookPanel>>,
    pub members: Vec<Member>,
    pub teams: Vec<Team>,
    /// Repositories a team can be granted; a grant form can only name one of these.
    pub repositories: Vec<RepositoryChoice>,
    /// True when more repositories can be granted than `repositories` lists.
    pub more_repositories: bool,
    /// True when the organizations list has rows after this page.
    pub more_organizations: bool,
    /// True when the member list has rows after this page.
    pub more_members: bool,
    /// True when the team list has rows after this page.
    pub more_teams: bool,
    pub selected_team: Option<TeamDetail>,
    /// Presentation only; every recovery POST rechecks current server authority.
    pub can_recover: bool,
    /// Organizations with no usable owner, which the recovery picker offers. Site administrators only.
    pub ownerless_organizations: Vec<Organization>,
    /// Repositories that deleting this organization also deletes. Owners only; zero otherwise.
    pub owned_repositories: i64,
    /// A success notice, such as after a deletion. Only the organizations index shows it.
    pub notice: Option<String>,
    pub after: i64,
    pub members_after: i64,
    pub teams_after: i64,
    pub members_cursor: i64,
    pub teams_cursor: i64,
    pub message: Option<String>,
    pub draft: HashMap<String, String>,
    /// Organizations this viewer has been invited to join. The organizations list shows them.
    pub invitations: Vec<Invitation>,
    /// Invitations this organization has sent and nobody has answered. Owners and admins see them.
    pub pending_invitations: Vec<Member>,
}

/// An invitation the signed-in viewer can accept or decline.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Invitation {
    pub org_id: i64,
    pub name: String,
    pub display_name: Option<String>,
    pub role: String,
}

pub fn render(page: Page) -> Element {
    let target = teams::form_target(&page);
    rsx! {
        style { dangerous_inner_html: include_str!("../account.css") }
        style { dangerous_inner_html: include_str!("organization.css") }
        div { class: "org-page identity-settings page-shell",
            if let Some(org) = &page.selected {
                {organization_header(org)}
            } else {
                header { class: "identity-settings-header",
                    h1 { "Account settings" }
                    p { "Manage your profile, credentials, and account access." }
                }
            }
            // Every message here is a refusal, so it uses the error banner that refused forms use.
            if let Some(message) = &page.message { div { role: "alert", class: "repo-alert repo-alert-error org-notice", p { "{message}" } } }
            if let Some(org) = &page.selected {
                // Organization pages have their own section list in place of the personal settings navigation.
                div { class: "identity-settings-layout",
                    nav { class: "identity-settings-nav org-settings-nav", aria_label: "Organization sections",
                        p { class: "identity-settings-nav-title", "Organization" }
                        {section_tab(org, &page.tab, "overview", "Overview", ProfileIcon::Building)}
                        p { class: "identity-settings-nav-title identity-settings-nav-section", "People" }
                        {section_tab(org, &page.tab, "members", "Members", ProfileIcon::People)}
                        {section_tab(org, &page.tab, "teams", "Teams", ProfileIcon::Layers)}
                        if page.is_owner { {section_tab(org, &page.tab, "webhooks", "Webhooks", ProfileIcon::Link)} }
                    }
                    div { class: "identity-settings-content",
                        {governance::suspension(org)}
                        {overview(&page, org, &target)}
                        {membership::panel(&page, &target)}
                        {teams::panel(&page, &target)}
                        if page.tab == "webhooks" {
                            section { id: "organization-webhooks", class: "org-section", aria_labelledby: "webhooks-title",
                                if let Some(panel) = &page.webhooks { WebhookSection { panel: panel.as_ref().clone() } }
                            }
                        }
                    }
                }
            } else {
                div { class: "identity-settings-layout",
                    {settings::settings_nav("organizations", page.manage_access)}
                    div { class: "identity-settings-content",
                        {index(&page)}
                    }
                }
            }
        }
    }
}

/// The header of one organization's settings: a way back to the organizations list, the
/// organization's name and a link to its public page.
fn organization_header(org: &Organization) -> Element {
    let label = org
        .display_name
        .as_deref()
        .filter(|name| !name.trim().is_empty())
        .unwrap_or(&org.name);
    let profile = format!("/{}", crate::webui::encode_component(&org.name));
    rsx! {
        nav { class: "org-crumbs org-settings-back", aria_label: "Organization navigation",
            NavLink { href: "/settings/organizations".to_owned(), "← Organizations" }
        }
        header { class: "org-page-header",
            {avatar::picture(org, "org-avatar", 64)}
            div {
                h1 { "{label} settings" }
                p { "@{org.name}" }
            }
            NavLink { href: profile, class: "btn btn-sm org-profile-link", "View profile" }
        }
    }
}

/// One section link in the organization settings navigation. The link is a plain URL, so the
/// server picks the section without scripts.
fn section_tab(
    org: &Organization,
    current: &str,
    tab: &str,
    label: &str,
    icon: ProfileIcon,
) -> Element {
    let active = current == tab;
    let class = if active {
        "identity-settings-nav-link identity-settings-nav-current"
    } else {
        "identity-settings-nav-link"
    };
    let aria_current = active.then(|| "page".to_owned());
    rsx! {
        NavLink { href: format!("/settings/organizations/{}?tab={tab}", org.id), class, aria_current,
            {profile_icon(icon)}
            span { "{label}" }
        }
    }
}

/// The overview: the profile and picture forms for owners and admins, read-only facts for members.
fn overview(page: &Page, org: &Organization, target: &str) -> Element {
    let intro = if page.can_manage {
        "Manage how this organization appears across codeyard."
    } else {
        "How this organization appears across codeyard."
    };
    rsx! {
        section { id: "organization-overview", class: "org-section", hidden: page.tab != "overview", aria_labelledby: "organization-overview-title",
            div { class: "org-section-header", div {
                h2 { id: "organization-overview-title", "Overview" }
                p { "{intro}" }
            } }
            if org.suspended {
                div { class: "org-panel org-empty", h3 { "Profile settings are unavailable" } p { "Appoint an active owner to resume organization changes." } }
            } else if page.can_manage {
                div { class: "org-overview-grid",
                    section { class: "org-panel",
                        header { class: "org-panel-header", h3 { "Organization profile" } }
                        div { class: "org-panel-body",
                            form { method: "post", action: target.to_owned(), class: "org-form",
                                {input(page, "profile", "display_name", "Display name")}
                                {input(page, "profile", "description", "Description")}
                                {controls(page, "profile", "Save profile")}
                            }
                        }
                    }
                    {avatar::panel(page, org)}
                }
            } else {
                {facts(org)}
            }
            if page.is_owner && !org.suspended {
                {governance::deletion(page, target)}
            }
        }
    }
}

/// What a member can read about the organization. The same facts, without the forms.
fn facts(org: &Organization) -> Element {
    let display = org
        .display_name
        .clone()
        .filter(|name| !name.trim().is_empty())
        .unwrap_or_else(|| "Not set".to_owned());
    let description = if org.description.is_empty() {
        "No description yet.".to_owned()
    } else {
        org.description.clone()
    };
    rsx! {
        section { class: "org-panel", aria_labelledby: "organization-facts-title",
            header { class: "org-panel-header", h3 { id: "organization-facts-title", "Organization profile" } }
            dl { class: "org-facts",
                dt { "Display name" }
                dd { "{display}" }
                dt { "Name" }
                dd { "@{org.name}" }
                dt { "Description" }
                dd { "{description}" }
            }
        }
    }
}

fn index(page: &Page) -> Element {
    rsx! {
        if let Some(notice) = &page.notice { div { role: "status", class: "repo-alert repo-alert-success org-notice", p { "{notice}" } } }
        if !page.invitations.is_empty() {
            section { class: "org-panel org-invitations", aria_labelledby: "organization-invitations-title",
                header { class: "org-panel-header", h2 { id: "organization-invitations-title", "Invitations" } p { "You join an organization only after you accept." } }
                ul { class: "org-list",
                    for invite in &page.invitations {
                        li { key: "{invite.org_id}", class: "org-row",
                            div { class: "org-row-main",
                                h3 { "{invite.display_name.as_deref().unwrap_or(&invite.name)}" }
                                p { "@{invite.name} · joins as {invite.role}" }
                            }
                            div { class: "org-row-actions",
                                form { method: "post", action: format!("/settings/organizations/invitations/{}/accept", invite.org_id), class: "org-inline-form",
                                    input { r#type: "hidden", name: "csrf_token", value: page.csrf_token.clone() }
                                    button { r#type: "submit", class: "btn btn-primary", "Accept" }
                                }
                                form { method: "post", action: format!("/settings/organizations/invitations/{}/decline", invite.org_id), class: "org-inline-form",
                                    input { r#type: "hidden", name: "csrf_token", value: page.csrf_token.clone() }
                                    button { r#type: "submit", class: "btn", "Decline" }
                                }
                            }
                        }
                    }
                }
            }
        }
        header { class: "org-index-header",
            div {
                div { class: "title-with-count", h2 { "Organizations" } span { class: "count-badge", {count_label(page.organizations.len(), "organization", "organizations")} } }
                p { "Manage the groups you belong to or create a new one." }
            }
        }
        div { class: "org-index-grid",
            section { aria_labelledby: "organizations-list-title",
                div { class: "org-section-header", div { h3 { id: "organizations-list-title", "Your organizations" } } }
                ul { class: "org-list org-index-list",
                    if page.organizations.is_empty() {
                        li { class: "org-empty", h3 { "No organizations yet" } p { "Create one to collaborate under a shared namespace." } }
                    }
                    for org in &page.organizations {
                        li { key: "{org.id}", class: "org-row",
                            div { class: "org-row-main",
                                h3 { NavLink { href: format!("/settings/organizations/{}", org.id), class: "org-row-link org-row-name", {avatar::picture(org, "org-avatar org-avatar-sm", 24)} "{org.display_name.as_deref().unwrap_or(&org.name)}" } }
                                p { "@{org.name}" if !org.description.is_empty() { " · {org.description}" } }
                            }
                            if org.suspended { span { class: "org-label", "Suspended" } }
                        }
                    }
                }
                if page.more_organizations { NavLink { href: format!("/settings/organizations?after={}", page.after), class: "org-pagination", "Next page" } }
            }
            section { class: "org-panel", aria_labelledby: "create-organization-title",
                header { class: "org-panel-header", h2 { id: "create-organization-title", "Create organization" } p { "Start a shared home for repositories and teams." } }
                form { method: "post", action: teams::form_target(page), class: "org-panel-body org-form",
                    {input(page, "create", "slug", "Organization name")}
                    {input(page, "create", "display_name", "Display name")}
                    {input(page, "create", "description", "Description")}
                    {controls(page, "create", "Create organization")}
                }
            }
        }
        {governance::recovery(page)}
    }
}
