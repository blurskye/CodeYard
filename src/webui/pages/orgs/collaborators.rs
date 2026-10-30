use super::{fields, Member, Page};
use crate::webui::{components::confirm_dialog, navigation::NavLink};
use dioxus::prelude::*;

pub(super) fn conversion(page: &Page, member: &Member, target: &str) -> Element {
    if !page.is_owner || page.selected.as_ref().is_none_or(|org| org.suspended) {
        return rsx! {};
    }
    let dialog = format!("outside-convert-{}", member.user_id);
    rsx! {
        a { class: "btn", href: "#{dialog}", "Convert to outside collaborator" }
        {confirm_dialog(Some(&dialog), &format!("Convert {} to an outside collaborator", member.username), target, target, rsx! {
            div { class: "org-form",
                strong { "Convert {member.username} to an outside collaborator?" }
                p { "Keeps explicit repository grants and repository permissions inherited through teams, but removes organization membership, every team membership, and organization-wide role assignments." }
                input { r#type: "hidden", name: "user_id", value: member.user_id.to_string() }
                {fields::removal_confirm(&member.username)}
                {fields::hidden_controls(page, "convert-outside")}
            }
        }, "Convert to outside collaborator")}
    }
}

pub(super) fn panel(page: &Page, target: &str) -> Element {
    if !page.is_owner {
        return rsx! {};
    }
    rsx! {
        section { class: "org-subsection", aria_labelledby: "organization-outside-title",
            h3 { id: "organization-outside-title", "Outside collaborators" }
            p { class: "org-note", "People with direct repository access who are not organization members. Public repositories remain public after removal." }
            ul { class: "org-list",
                if page.outside_collaborators.is_empty() {
                    li { class: "org-empty", "No outside collaborators on this page." }
                }
                for person in &page.outside_collaborators {
                    li { class: "org-row", key: "{person.user_id}",
                        div { class: "org-row-main",
                            h3 { NavLink { class: "org-row-link", href: crate::webui::profile_path(&person.username), "{person.username}" } }
                            p { "Outside collaborator" }
                        }
                        if page.selected.as_ref().is_some_and(|org| !org.suspended) {
                            div { class: "org-row-actions",
                                a { class: "btn btn-danger", href: "#outside-remove-{person.user_id}", "Remove access" }
                                {confirm_dialog(Some(&format!("outside-remove-{}",person.user_id)), &format!("Remove {} from organization repositories", person.username), target, target, rsx! {
                                    div { class: "org-form",
                                        strong { "Remove {person.username} from all organization repositories?" }
                                        input { r#type: "hidden", name: "user_id", value: person.user_id.to_string() }
                                        {fields::removal_confirm(&person.username)}
                                        {fields::hidden_controls(page, "remove-outside")}
                                    }
                                }, "Remove access")}
                            }
                        }
                    }
                }
            }
            if let Some(next) = &page.outside_next {
                NavLink { class: "org-pagination", href: next.clone(), "Next outside collaborators" }
            }
        }
    }
}
