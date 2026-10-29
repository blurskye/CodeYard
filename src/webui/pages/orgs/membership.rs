use super::{
    fields::{self, controls, controls_as, field_value, input, select, FIELD},
    teams, Page,
};
use crate::webui::{
    components::{confirm_dialog, count_label, Avatar, DraftSelect, SelectOption},
    navigation::NavLink,
};
use dioxus::prelude::*;

// The roles the backend parser accepts. Which role a person may assign is the DAO's decision.
const ROLES: [(&str, &str); 3] = [("member", "Member"), ("admin", "Admin"), ("owner", "Owner")];

fn role_options(owner: bool) -> Vec<SelectOption> {
    ROLES
        .iter()
        .filter(|(value, _)| owner || *value == "member")
        .map(|&(value, label)| SelectOption::new(value, label))
        .collect()
}

pub(super) fn panel(page: &Page, target: &str) -> Element {
    let suspended = page.selected.as_ref().is_none_or(|org| org.suspended);
    // A rejected change keeps the role it was submitted with. A value that is not a role
    // is not offered, so the row shows its stored role instead.
    let role_value = |member: &super::Member| {
        let drafted = page.draft.get("action").is_some_and(|v| v == "member-role")
            && page.draft.get("user_id") == Some(&member.user_id.to_string());
        page.draft
            .get("role")
            .filter(|role| drafted && (page.is_owner || role.as_str() == "member") && ROLES.iter().any(|(value, _)| *value == role.as_str()))
            .cloned()
            .unwrap_or_else(|| member.role.clone())
    };
    // The DAO lets owners change and remove anyone; admins change and remove plain members.
    let manageable = |member: &super::Member| {
        page.can_manage && !suspended && (page.is_owner || member.role == "member")
    };
    rsx! {
        section { id: "organization-members", class: "org-section", aria_labelledby: "organization-members-title",
            div { class: "org-section-header",
                div {
                    div { class: "title-with-count", h2 { id: "organization-members-title", "Members" } span { class: "count-badge", {count_label(page.members.len(), "member", "members")} } }
                    p { "People with access to this organization." }
                }
            }
            if !suspended {
                form { method: "post", action: target.to_owned(), class: "org-inline-form", aria_label: "Your membership visibility",
                    {select("Your membership visibility", "visibility", if page.membership_public { "public".into() } else { "private".into() }, vec![SelectOption::new("private", "Private"), SelectOption::new("public", "Public")])}
                    {controls(page, "membership-visibility", "Save visibility")}
                }
            }
            if page.can_manage && !suspended {
                form { method: "post", action: target.to_owned(), class: "org-inline-form org-add-member", aria_label: "Invite a member",
                    {input(page, "add-member", "username", "Username or email address")}
                    {select("Role", "role", field_value(page, "add-member", "role"), role_options(page.is_owner))}
                    label { class: "org-check-label",
                        input { r#type: "checkbox", name: "reinstate", value: "yes", checked: field_value(page, "add-member", "reinstate") == "yes" }
                        " Restore the previous role instead"
                    }
                    {input(page, "add-member", "team_slugs", "Initial teams (optional)")}
                    {controls(page, "add-member", "Send invitation")}
                }
            }
            ul { class: "org-list",
                if page.members.is_empty() {
                    li { class: "org-empty", h3 { "No members on this page" } }
                }
                for member in &page.members {
                    li { key: "{member.user_id}", class: "org-row", aria_label: format!("{}: {}", member.username, member.role),
                        div { class: "org-row-main",
                            h3 { NavLink { href: crate::webui::profile_path(&member.username), class: "org-row-link org-row-name", Avatar { name: member.username.clone(), size: 24 } "{member.username}" } }
                        }
                        div { class: "org-row-actions",
                            // A manageable row shows its role in the select below, so the badge is only for read-only rows.
                            if !manageable(member) {
                                span { class: "org-label", "{member.role}" }
                            }
                            // Any member may leave; owners leave through their Remove control.
                            if !manageable(member) && member.user_id == page.viewer_id && !suspended {
                                a { class: "btn btn-danger", href: "#remove-member-{member.user_id}", "Leave" }
                                {confirm_dialog(Some(format!("remove-member-{}", member.user_id)).as_deref(), "Leave the organization", target, target, rsx! {
                                    div { class: "org-form",
                                        strong { "Leave this organization?" }
                                        input { r#type: "hidden", name: "user_id", value: "{member.user_id}" }
                                        {fields::removal_confirm(&member.username)}
                                        {fields::hidden_controls(page, "remove-member")}
                                    }
                                }, "Leave")}
                            }
                            if manageable(member) {
                                form { method: "post", action: target.to_owned(), class: "org-inline-form",
                                    input { r#type: "hidden", name: "user_id", value: "{member.user_id}" }
                                    label { span { class: "site-sr-only", "Role for {member.username}" }
                                        DraftSelect { class: FIELD, name: "role", initial_value: role_value(member), options: role_options(page.is_owner), required: true }
                                    }
                                    {controls_as(page, "member-role", "Update", "btn")}
                                }
                                {super::collaborators::conversion(page, member, target)}
                                a { class: "btn btn-danger", href: "#remove-member-{member.user_id}", "Remove" span { class: "site-sr-only", " {member.username}" } }
                                {confirm_dialog(Some(format!("remove-member-{}", member.user_id)).as_deref(), &format!("Remove {} from the organization", member.username), target, target, rsx! {
                                    div { class: "org-form",
                                        strong { "Remove {member.username} from the organization?" }
                                        input { r#type: "hidden", name: "user_id", value: "{member.user_id}" }
                                        {fields::removal_confirm(&member.username)}
                                        {fields::hidden_controls(page, "remove-member")}
                                    }
                                }, "Remove")}
                            }
                        }
                    }
                }
            }
            for (title, invitations, next) in [
                ("Pending invitations", &page.pending_invitations, &page.pending_invitations_next),
                ("Failed invitations", &page.failed_invitations, &page.failed_invitations_next),
            ] {
                if page.can_manage && !invitations.is_empty() {
                section { class: "org-subsection", aria_label: title,
                    h3 { "{title}" }
                    ul { class: "org-list",
                        for invite in invitations {
                            li { key: "pending-{invite.id}", class: "org-row",
                                div { class: "org-row-main",
                                    h3 { "{invite.username}" }
                                    if let Some(reason) = &invite.failed_reason {
                                        p { "Invited as {invite.role}. Cancel this offer or send a new invitation." }
                                        p { class: "repo-alert repo-alert-error", "{reason}" }
                                    } else {
                                        p { "Invited as {invite.role}. Joins when they accept." }
                                    }
                                }
                                div { class: "org-row-actions",
                                    a { class: "btn btn-danger", href: "#cancel-invitation-{invite.id}", "Cancel invitation" }
                                    {confirm_dialog(Some(format!("cancel-invitation-{}", invite.id)).as_deref(), &format!("Cancel the invitation to {}", invite.username), target, target, rsx! {
                                        div { class: "org-form",
                                            strong { "Cancel the invitation to {invite.username}?" }
                                            input { r#type: "hidden", name: "invitation_id", value: "{invite.id}" }
                                            {fields::hidden_controls(page, "cancel-invitation")}
                                        }
                                    }, "Cancel invitation")}
                                }
                            }
                        }
                    }
                }
                }
                if page.can_manage {
                    if let Some(href) = next {
                        NavLink { href: href.clone(), class: "org-pagination", "Next {title}" }
                    }
                }
            }
            {super::collaborators::panel(page, target)}
            {super::blocks::panel(page, target)}
            if page.more_members { NavLink { href: teams::navigation(page, page.members_after, page.teams_cursor, page.selected_team.as_ref().map(|detail| (detail.team.id, detail.members_cursor, detail.repositories_cursor))), class: "org-pagination", "Next members" } }
        }
    }
}
