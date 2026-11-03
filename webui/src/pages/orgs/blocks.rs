use super::{fields, Page};
use crate::navigation::NavLink;
use dioxus::prelude::*;

pub(super) fn panel(page: &Page, target: &str) -> Element {
    if !page.is_owner {
        return rsx! {};
    }
    let suspended = page.selected.as_ref().is_none_or(|org| org.suspended);
    rsx! {
        section { class: "org-subsection", aria_labelledby: "organization-blocks-title",
            h3 { id: "organization-blocks-title", "Blocked users" }
            p { class: "org-note", "Blocking removes repository access, invitations, stars, watches, assignments, and review requests, and prevents new interactions with this organization's repositories. Public repositories remain readable. Remove organization members before blocking them." }
            if !suspended {
                form { class: "org-form", method: "post", action: target,
                    {fields::input(page,"block-user","username", "Username to block")}
                    {fields::hidden_controls(page,"block-user")}
                    button { class: "btn btn-danger", r#type: "submit", "Block user" }
                }
            }
            ul { class: "org-list",
                if page.blocked_users.is_empty() { li { class: "org-empty", "No blocked users on this page." } }
                for user in &page.blocked_users {
                    li { class: "org-row", key: "{user.user_id}",
                        div { class: "org-row-main",
                            h3 { NavLink { class: "org-row-link", href: crate::profile_path(&user.username), "{user.username}" } }
                            p { "Blocked from this organization" }
                        }
                        if !suspended {
                            form { class: "org-row-actions", method: "post", action: target,
                                input { r#type: "hidden", name: "user_id", value: user.user_id.to_string() }
                                {fields::hidden_controls(page,"unblock-user")}
                                button { class: "btn", r#type: "submit", "Unblock" }
                            }
                        }
                    }
                }
            }
            p { class: "org-note", "Unblocking does not restore removed access or subscriptions." }
            if let Some(next)=&page.blocks_next { NavLink { class: "org-pagination", href: next.clone(), "Next blocked users" } }
        }
    }
}
