use super::{IssueManagement, Item, Repository};
use crate::webui::{navigation::NavLink, pages::repos::chrome::csrf};
use dioxus::prelude::*;

pub(super) fn panel(
    repo: &Repository,
    url: &str,
    item: &Item,
    controls: &IssueManagement,
) -> Element {
    rsx! {
        section { class: "collaboration-sidebar-section",
            h3 { "Development" }
            for (name, href) in &controls.branches { p { NavLink { href: href.clone(), "{name}" } } }
            for (name, href) in &controls.closing_pulls { p { NavLink { href: href.clone(), "{name}" } } }
            if let Some((name,href)) = &controls.duplicate { p { "Duplicate of " NavLink { href: href.clone(), "{name}" } } }
            if repo.can_write && !repo.chrome.is_archived {
                form { method: "post", action: format!("{url}/manage"), class: "collaboration-form-fields",
                    {csrf(&repo.csrf)} input { r#type: "hidden", name: "issue_id", value: "{item.id}" }
                    input { r#type: "hidden", name: "operation", value: "branch" }
                    label { r#for: "issue-branch", "Create a branch" }
                    input { id: "issue-branch", name: "name", class: "collaboration-input", required: true, placeholder: "Branch name" }
                    button { r#type: "submit", class: "btn btn-secondary btn-sm", "Create branch" }
                }
            }
        }
        if repo.can_write && !repo.chrome.is_archived {
            section { class: "collaboration-sidebar-section",
                form { method: "post", action: format!("{url}/manage"),
                    {csrf(&repo.csrf)} input { r#type: "hidden", name: "issue_id", value: "{item.id}" }
                    input { r#type: "hidden", name: "operation", value: if controls.pinned { "unpin" } else { "pin" } }
                    button { r#type: "submit", class: "btn btn-secondary btn-sm", if controls.pinned { "Unpin issue" } else { "Pin issue" } }
                }
                details {
                    summary { "Transfer issue" }
                    form { method: "post", action: format!("{url}/manage"), class: "collaboration-form-fields",
                        {csrf(&repo.csrf)} input { r#type: "hidden", name: "issue_id", value: "{item.id}" }
                        input { r#type: "hidden", name: "operation", value: "transfer" }
                        label { r#for: "issue-destination", "Repository of this owner" }
                        input { id: "issue-destination", name: "destination", class: "collaboration-input", required: true }
                        button { r#type: "submit", class: "btn btn-secondary btn-sm", "Transfer" }
                    }
                }
                if item.state=="open" {
                    details {
                        summary { "Close as duplicate" }
                        form { method: "post", action: format!("{url}/manage"), class: "collaboration-form-fields",
                            {csrf(&repo.csrf)} input { r#type: "hidden", name: "issue_id", value: "{item.id}" }
                            input { r#type: "hidden", name: "operation", value: "duplicate" }
                            label { r#for: "issue-duplicate", "Duplicate issue" }
                            input { id: "issue-duplicate", name: "duplicate", placeholder: "Number or owner/repository#number", class: "collaboration-input", required: true }
                            button { r#type: "submit", class: "btn btn-secondary btn-sm", "Close as duplicate" }
                        }
                    }
                }
                if repo.can_admin {
                    details {
                        summary { "Delete issue" }
                        form { method: "post", action: format!("{url}/manage"), class: "collaboration-form-fields",
                            {csrf(&repo.csrf)} input { r#type: "hidden", name: "issue_id", value: "{item.id}" }
                            input { r#type: "hidden", name: "operation", value: "delete" }
                            p { "This permanently deletes the issue and its comments." }
                            label { r#for: "issue-delete-confirm", "Type the issue number to confirm" }
                            input { id: "issue-delete-confirm", name: "confirm", class: "collaboration-input", required: true }
                            button { r#type: "submit", class: "btn btn-danger btn-sm", "Delete issue" }
                        }
                    }
                }
            }
        }
    }
}
