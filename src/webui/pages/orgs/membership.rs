use super::{
    fields::{self, choice, controls, controls_as, field_value, input, select, FIELD},
    teams, Page,
};
use crate::webui::{
    components::{confirm_dialog, count_label, Avatar, DraftSelect, SelectOption},
    navigation::NavLink,
};
use dioxus::prelude::*;

// The roles the backend parser accepts. Which role a person may assign is the DAO's decision.
const ROLES: [(&str, &str); 3] = [("member", "Member"), ("admin", "Admin"), ("owner", "Owner")];

fn role_options() -> Vec<SelectOption> {
    ROLES
        .iter()
        .map(|&(value, label)| choice(value, label))
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
            .filter(|role| drafted && ROLES.iter().any(|(value, _)| *value == role.as_str()))
            .cloned()
            .unwrap_or_else(|| member.role.clone())
    };
    // The DAO lets owners change and remove anyone; admins change and remove plain members.
    let manageable = |member: &super::Member| {
        page.can_manage && !suspended && (page.is_owner || member.role == "member")
    };
    rsx! {
        section { id: "organization-members", class: "org-section", hidden: page.tab != "members", aria_labelledby: "organization-members-title",
            div { class: "org-section-header",
                div {
                    div { class: "title-with-count", h2 { id: "organization-members-title", "Members" } span { class: "count-badge", {count_label(page.members.len(), "member", "members")} } }
                    p { "People with access to this organization." }
                }
            }
            if page.can_manage && !suspended {
                form { method: "post", action: target.to_owned(), class: "org-inline-form org-add-member", aria_label: "Invite a member",
                    {input(page, "add-member", "username", "Username")}
                    {select("Role", "role", field_value(page, "add-member", "role"), role_options())}
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
                            h3 { NavLink { href: format!("/{}", crate::webui::encode_component(&member.username)), class: "org-row-link org-row-name", Avatar { name: member.username.clone(), size: 24 } "{member.username}" } }
                        }
                        div { class: "org-row-actions",
                            // A manageable row shows its role in the select below, so the badge is only for read-only rows.
                            if !manageable(member) {
                                span { class: "org-label", "{member.role}" }
                            }
                            if manageable(member) {
                                form { method: "post", action: target.to_owned(), class: "org-inline-form",
                                    input { r#type: "hidden", name: "user_id", value: "{member.user_id}" }
                                    label { span { class: "site-sr-only", "Role for {member.username}" }
                                        DraftSelect { class: FIELD, name: "role", initial_value: role_value(member), options: role_options(), required: true }
                                    }
                                    {controls_as(page, "member-role", "Update", "btn")}
                                }
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
            if page.can_manage && !page.pending_invitations.is_empty() {
                section { class: "org-subsection", aria_labelledby: "organization-pending-title",
                    h3 { id: "organization-pending-title", "Pending invitations" }
                    ul { class: "org-list",
                        for invite in &page.pending_invitations {
                            li { key: "pending-{invite.user_id}", class: "org-row",
                                div { class: "org-row-main",
                                    h3 { "{invite.username}" }
                                    p { "Invited as {invite.role}. Joins when they accept." }
                                }
                                div { class: "org-row-actions",
                                    a { class: "btn btn-danger", href: "#cancel-invitation-{invite.user_id}", "Cancel invitation" }
                                    {confirm_dialog(Some(format!("cancel-invitation-{}", invite.user_id)).as_deref(), &format!("Cancel the invitation to {}", invite.username), target, target, rsx! {
                                        div { class: "org-form",
                                            strong { "Cancel the invitation to {invite.username}?" }
                                            input { r#type: "hidden", name: "user_id", value: "{invite.user_id}" }
                                            {fields::hidden_controls(page, "cancel-invitation")}
                                        }
                                    }, "Cancel invitation")}
                                }
                            }
                        }
                    }
                }
            }
            if page.more_members { NavLink { href: teams::navigation(page, page.members_after, page.teams_cursor, page.selected_team.as_ref().map(|detail| (detail.team.id, detail.members_cursor, detail.repositories_cursor))), class: "org-pagination", "Next members" } }
        }
    }
}
