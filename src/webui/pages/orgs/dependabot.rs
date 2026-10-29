use super::{
    fields::{controls, input, select},
    Page,
};
use crate::webui::{components::SelectOption, navigation::NavLink};
use dioxus::prelude::*;

#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct RepositoryAccess {
    pub default_level: String,
    pub repositories: Vec<Repository>,
    pub page: i64,
    pub has_more: bool,
}

#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Repository {
    pub id: i64,
    pub name: String,
    pub private: bool,
}

pub(super) fn panel(page: &Page) -> Element {
    let (Some(org), Some(access)) = (&page.selected, &page.dependabot) else {
        return rsx! {};
    };
    let target = format!(
        "/settings/organizations/{}/dependabot?tab=dependabot",
        org.id
    );
    let default_level = if page
        .draft
        .get("action")
        .is_some_and(|v| v == "dependabot-default")
    {
        page.draft
            .get("default_level")
            .unwrap_or(&access.default_level)
            .clone()
    } else {
        access.default_level.clone()
    };
    rsx! {
        section { class: "org-section", aria_labelledby: "dependabot-title",
            div { class: "org-section-header", div {
                h2 { id: "dependabot-title", "Dependabot repository access" }
                p { "Manage repository access for dependency update integrations. Codeyard does not run a dependency update service." }
            } }
            section { class: "org-panel",
                header { class: "org-panel-header", h3 { "Default access" } }
                form { method: "post", action: target.clone(), class: "org-panel-body org-form",
                    {select("Default repository access level", "default_level", default_level, vec![SelectOption::new("public", "Public repositories"), SelectOption::new("internal", "Public and internal repositories")])}
                    p { "Codeyard currently has public and private repositories, not internal repositories. Private repositories require explicit access." }
                    {controls(page, "dependabot-default", "Save default")}
                }
            }
            section { class: "org-panel",
                header { class: "org-panel-header", h3 { "Accessible repositories" } }
                ul { class: "org-list",
                    for repository in &access.repositories {
                        li { key: "{repository.id}", class: "org-row",
                            div { class: "org-row-main", h3 { "{repository.name}" } p { "Repository ID: {repository.id}" } }
                            if repository.private {
                                form { method: "post", action: target.clone(), class: "org-inline-form",
                                    input { r#type: "hidden", name: "repo_id", value: "{repository.id}" }
                                    {controls(page, "dependabot-remove", "Remove access")}
                                }
                            } else { span { class: "org-label", "Public access" } }
                        }
                    }
                }
                form { method: "post", action: target, class: "org-panel-body org-form",
                    {input(page, "dependabot-add", "repo_id", "Repository ID to allow")}
                    {controls(page, "dependabot-add", "Add repository")}
                }
            }
            nav { class: "repo-pagination", aria_label: "Accessible repositories",
                if access.page > 1 { NavLink { href: format!("/settings/organizations/{}?tab=dependabot&page={}", org.id, access.page - 1), "Previous" } }
                if access.has_more { NavLink { href: format!("/settings/organizations/{}?tab=dependabot&page={}", org.id, access.page + 1), "Next" } }
            }
        }
    }
}
