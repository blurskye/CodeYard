use super::{
    fields::{self, choice, controls, controls_as, field_value, input, select, FIELD},
    privacy_label, team_page, Page, REPOSITORY_CHOICE_LIMIT,
};
use crate::webui::{
    components::{confirm_dialog, count_label, Avatar, DraftInput, DraftSelect, SelectOption},
    navigation::NavLink,
};
use dioxus::prelude::*;

pub(super) fn navigation(
    page: &Page,
    members: i64,
    teams: i64,
    detail: Option<(i64, i64, i64)>,
) -> String {
    let base = page
        .selected
        .as_ref()
        .map(|org| format!("/settings/organizations/{}", org.id))
        .unwrap_or_else(|| "/settings/organizations".into());
    let mut location = format!("{base}?members_after={members}&teams_after={teams}");
    if let Some((team, members, repositories)) = detail {
        location.push_str(&format!(
            "&team={team}&team_members_after={members}&team_repositories_after={repositories}"
        ));
    }
    if page.tab != "overview" {
        location.push_str(&format!("&tab={}", page.tab));
    }
    location
}

pub(super) fn form_target(page: &Page) -> String {
    if page.selected.is_some()
        && (page.selected_team.is_some() || page.members_cursor != 0 || page.teams_cursor != 0)
    {
        navigation(
            page,
            page.members_cursor,
            page.teams_cursor,
            page.selected_team.as_ref().map(|detail| {
                (
                    detail.team.id,
                    detail.members_cursor,
                    detail.repositories_cursor,
                )
            }),
        )
    } else {
        let tab = if page.tab == "overview" {
            String::new()
        } else {
            format!("?tab={}", page.tab)
        };
        page.selected
            .as_ref()
            .map(|org| format!("/settings/organizations/{}{tab}", org.id))
            .unwrap_or_else(|| "/settings/organizations".into())
    }
}

pub(super) fn permission_name(permission: i16) -> &'static str {
    match permission {
        1 => "Read",
        2 => "Triage",
        3 => "Write",
        4 => "Maintain",
        5 => "Admin",
        _ => "Custom",
    }
}

fn privacy_options() -> Vec<SelectOption> {
    vec![
        choice("visible", privacy_label("visible")),
        choice("secret", privacy_label("secret")),
    ]
}
fn role_options() -> Vec<SelectOption> {
    vec![
        choice("member", "Member"),
        choice("maintainer", "Maintainer"),
    ]
}
fn permission_options() -> Vec<SelectOption> {
    (1..=5)
        .map(|level| choice(&level.to_string(), permission_name(level)))
        .collect()
}
fn repository_options(page: &Page) -> Vec<SelectOption> {
    std::iter::once(choice("", "Choose a repository"))
        .chain(page.repositories.iter().map(|repo| {
            choice(
                &repo.repo_id.to_string(),
                &format!("{}/{}", repo.owner, repo.name),
            )
        }))
        .collect()
}

pub(super) fn panel(page: &Page, target: &str) -> Element {
    let suspended = page.selected.as_ref().is_none_or(|org| org.suspended);
    // Team management is owner and admin work, as the DAO checks it.
    let manage = page.can_manage && !suspended;
    let org_name = page
        .selected
        .as_ref()
        .map(|org| org.name.clone())
        .unwrap_or_default();
    // Members who cannot manage teams open the same panel read-only, so the link says "View".
    let team_action = if manage { "Manage" } else { "View" };
    let permission_value = |team: i64, grant: &super::RepositoryGrant| {
        if page.draft.get("action").is_some_and(|v| v == "grant")
            && page.draft.get("team_id") == Some(&team.to_string())
            && page.draft.get("repo_id") == Some(&grant.repo_id.to_string())
        {
            page.draft
                .get("permission")
                .cloned()
                .unwrap_or_else(|| grant.permission.to_string())
        } else {
            grant.permission.to_string()
        }
    };
    rsx! {
        section { id: "organization-teams", class: "org-section", hidden: page.tab != "teams", aria_labelledby: "organization-teams-title",
            div { class: "org-section-header",
                div {
                    div { class: "title-with-count", h2 { id: "organization-teams-title", "Teams" } span { class: "count-badge", {count_label(page.teams.len(), "team", "teams")} } }
                    p { "Group members and grant repository access." }
                }
            }
            if manage {
                section { class: "org-panel org-create-team", aria_labelledby: "create-team-title",
                    header { class: "org-panel-header", h3 { id: "create-team-title", "Create a team" } }
                    form { method: "post", action: target.to_owned(), class: "org-panel-body org-form org-form-grid",
                        {input(page, "team", "slug", "Team name")}
                        {input(page, "team", "name", "Display name (optional)")}
                        {input(page, "team", "description", "Description")}
                        {select("Privacy", "privacy", field_value(page, "team", "privacy"), privacy_options())}
                        {controls(page, "team", "Create team")}
                    }
                }
            }
            ul { class: "org-list",
                if page.teams.is_empty() {
                    li { class: "org-empty", h3 { "No teams yet" } p { "Create a team to organize access." } }
                }
                for team in &page.teams {
                    li { key: "{team.id}", class: "org-row",
                        div { class: "org-row-main",
                            h3 { NavLink { href: team_page::href(&org_name, &team.slug), class: "org-row-link", "{team.name}" } }
                            if !team.description.is_empty() { p { class: "org-row-description", "{team.description}" } }
                            p { {count_label(team.member_count.max(0) as usize, "member", "members")} " · " NavLink { href: navigation(page, page.members_cursor, page.teams_cursor, Some((team.id, 0, 0))), class: "org-row-link", "{team_action}" } }
                        }
                        span { class: "org-label", {privacy_label(&team.privacy)} }
                    }
                }
            }
            if page.more_teams { NavLink { href: navigation(page, page.members_cursor, page.teams_after, page.selected_team.as_ref().map(|detail| (detail.team.id, detail.members_cursor, detail.repositories_cursor))), class: "org-pagination", "Next teams" } }
            if let Some(detail) = &page.selected_team {
                article { class: "org-panel org-team-detail", aria_labelledby: "team-detail-title",
                    header { class: "org-panel-header org-team-detail-header",
                        div {
                            h3 { id: "team-detail-title", "{detail.team.name}" }
                            p { "@{detail.slug} · " {privacy_label(&detail.team.privacy)} }
                            if !detail.description.is_empty() { p { "{detail.description}" } }
                        }
                        div { class: "org-team-detail-actions",
                            NavLink { href: team_page::href(&org_name, &detail.slug), class: "btn btn-sm", "View team page" }
                            NavLink { href: navigation(page, page.members_cursor, page.teams_cursor, None), class: "btn btn-sm", "Close" }
                        }
                    }
                    div { class: "org-panel-body",
                        div { class: "org-detail-grid",
                            section { aria_labelledby: "team-members-title",
                                h4 { id: "team-members-title", "Team members" }
                                ul { class: "org-list",
                                    if detail.members.is_empty() { li { class: "org-empty", "No members on this page." } }
                                    for member in &detail.members {
                                        li { key: "{member.user_id}", class: "org-row",
                                            div { class: "org-row-main",
                                                h4 { NavLink { href: format!("/{}", crate::webui::encode_component(&member.username)), class: "org-row-link org-row-name", Avatar { name: member.username.clone(), size: 24 } "{member.username}" } }
                                            }
                                            div { class: "org-row-actions",
                                                span { class: "org-label", "{member.role}" }
                                                if manage {
                                                    a { class: "btn btn-sm btn-danger", href: "#remove-team-member-{member.user_id}", "Remove" span { class: "site-sr-only", " {member.username}" } }
                                                    {confirm_dialog(Some(format!("remove-team-member-{}", member.user_id)).as_deref(), &format!("Remove {} from the team", member.username), target, target, rsx! {
                                                        div { class: "org-form",
                                                            strong { "Remove {member.username} from the team?" }
                                                            input { r#type: "hidden", name: "team_id", value: "{detail.team.id}" }
                                                            input { r#type: "hidden", name: "user_id", value: "{member.user_id}" }
                                                            {fields::removal_confirm(&member.username)}
                                                            {fields::hidden_controls(page, "remove-team-member")}
                                                        }
                                                    }, "Remove")}
                                                }
                                            }
                                        }
                                    }
                                }
                                if detail.more_members {
                                    NavLink { href: navigation(page, page.members_cursor, page.teams_cursor, Some((detail.team.id, detail.members_after, detail.repositories_cursor))), class: "org-pagination", "Next team members" }
                                }
                            }
                            section { aria_labelledby: "team-repositories-title",
                                h4 { id: "team-repositories-title", "Team repository grants" }
                                ul { class: "org-list",
                                    if detail.repositories.is_empty() { li { class: "org-empty", "No repositories on this page." } }
                                    for grant in &detail.repositories {
                                        li { key: "{grant.repo_id}", class: "org-row",
                                            div { class: "org-row-main",
                                                h4 { NavLink { href: format!("/{}/{}", crate::webui::encode_component(&grant.owner), crate::webui::encode_component(&grant.name)), class: "org-row-link", "{grant.owner}/{grant.name}" } }
                                            }
                                            div { class: "org-row-actions",
                                                span { class: "org-label", "{permission_name(grant.permission)}" }
                                                if manage {
                                                    form { method: "post", action: target.to_owned(), class: "org-inline-form",
                                                        input { r#type: "hidden", name: "team_id", value: "{detail.team.id}" }
                                                        input { r#type: "hidden", name: "repo_id", value: "{grant.repo_id}" }
                                                        label { span { class: "site-sr-only", "Access to {grant.owner}/{grant.name}" }
                                                            DraftSelect { class: FIELD, name: "permission", initial_value: permission_value(detail.team.id, grant), options: permission_options(), required: true }
                                                        }
                                                        {controls_as(page, "grant", "Save", "btn")}
                                                    }
                                                    form { method: "post", action: target.to_owned(), class: "org-inline-form",
                                                        input { r#type: "hidden", name: "team_id", value: "{detail.team.id}" }
                                                        input { r#type: "hidden", name: "repo_id", value: "{grant.repo_id}" }
                                                        {controls_as(page, "revoke", "Revoke", "btn btn-danger")}
                                                    }
                                                }
                                            }
                                        }
                                    }
                                }
                                if detail.more_repositories {
                                    NavLink { href: navigation(page, page.members_cursor, page.teams_cursor, Some((detail.team.id, detail.members_cursor, detail.repositories_after))), class: "org-pagination", "Next team repositories" }
                                }
                            }
                        }
                        if manage { {management(page, target, detail)} }
                    }
                }
            }
        }
    }
}

/// The team's management sections, and the delete card at the bottom.
fn management(page: &Page, target: &str, detail: &super::TeamDetail) -> Element {
    // One string, so the sentence stays whole in the rendered page.
    let more_note = format!(
        "This organization has more than {REPOSITORY_CHOICE_LIMIT} repositories; enter the repository ID."
    );
    // The delete confirmation is typed, never filled in, except to show a rejected attempt, which
    // reopens its dialog.
    let refused = page.draft.get("action").is_some_and(|v| v == "team-delete");
    let confirmation = if refused {
        page.draft.get("team_id").cloned().unwrap_or_default()
    } else {
        String::new()
    };
    rsx! {
        div { class: "org-team-manage",
            section { class: "org-subpanel", aria_labelledby: "team-edit-title",
                h4 { id: "team-edit-title", "Edit team" }
                form { method: "post", action: target.to_owned(), class: "org-form",
                    input { r#type: "hidden", name: "team_id", value: "{detail.team.id}" }
                    {input(page, "team-edit", "name", "Team name")}
                    {input(page, "team-edit", "description", "Description")}
                    {select("Privacy", "privacy", field_value(page, "team-edit", "privacy"), privacy_options())}
                    {controls(page, "team-edit", "Save team")}
                }
            }
            section { class: "org-subpanel", aria_labelledby: "team-add-member-title",
                h4 { id: "team-add-member-title", "Add a team member" }
                form { method: "post", action: target.to_owned(), class: "org-inline-form",
                    input { r#type: "hidden", name: "team_id", value: "{detail.team.id}" }
                    {input(page, "team-member", "username", "Organization member username")}
                    {select("Role", "role", field_value(page, "team-member", "role"), role_options())}
                    {controls(page, "team-member", "Add team member")}
                }
            }
            section { class: "org-subpanel", aria_labelledby: "team-grant-title",
                h4 { id: "team-grant-title", "Grant repository access" }
                if page.repositories.is_empty() && !page.more_repositories {
                    p { class: "org-note", "No repositories are available to grant." }
                } else {
                    form { method: "post", action: target.to_owned(), class: "org-inline-form",
                        input { r#type: "hidden", name: "team_id", value: "{detail.team.id}" }
                        if page.more_repositories {
                            p { class: "org-note", "{more_note}" }
                            {input(page, "grant", "repo_id", "Repository ID")}
                        } else {
                            {select("Repository", "repo_id", field_value(page, "grant", "repo_id"), repository_options(page))}
                        }
                        {select("Access", "permission", field_value(page, "grant", "permission"), permission_options())}
                        {controls(page, "grant", "Grant repository")}
                    }
                }
            }
        }
        section { class: "org-danger", aria_labelledby: "team-delete-title",
            div { class: "org-danger-header",
                h4 { id: "team-delete-title", "Delete team" }
                p { "Deleting a team removes its memberships and repository access." }
            }
            div { class: "org-danger-action",
                if refused {
                    if let Some(message) = &page.message {
                        div { role: "alert", class: "repo-alert repo-alert-error", p { "{message}" } }
                    }
                }
                a { class: "btn btn-destructive", href: "#team-delete-dialog", "Delete team and its grants" }
                {confirm_dialog(Some("team-delete-dialog"), &format!("Delete team {}", detail.team.name), target, target, rsx! {
                    div { class: "org-form",
                        strong { "Delete team {detail.team.name}?" }
                        label { span { "Type the team ID to confirm" }
                            DraftInput { class: FIELD, name: "team_id", initial_value: confirmation, placeholder: detail.team.id.to_string(), maxlength: Some(32) }
                        }
                        {fields::hidden_controls(page, "team-delete")}
                    }
                }, "Delete team and its grants")}
            }
        }
    }
}
