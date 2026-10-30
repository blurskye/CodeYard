use super::super::chrome::{csrf, repository_id, RepoChrome};
use super::{secrets, Settings};
use crate::webui::components::{DraftInput, DraftSelect, SelectOption};
use dioxus::prelude::*;

#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Environment {
    pub name: String,
    pub secrets: Vec<secrets::Secret>,
    pub variables: Vec<secrets::Variable>,
}

pub fn section(repo: &RepoChrome, settings: &Settings) -> Element {
    let selected = settings
        .environment
        .as_ref()
        .map(|value| value.name.clone())
        .unwrap_or_default();
    let mut options = vec![SelectOption::new("", "Repository")];
    if !selected.is_empty() && !settings.environments.contains(&selected) { options.push(SelectOption::new(selected.clone(), format!("Environment: {selected}"))); }
    options.extend(
        settings
            .environments
            .iter()
            .map(|name| SelectOption::new(name.clone(), format!("Environment: {name}"))),
    );
    rsx! {
        form { class: "repo-settings-section panel", method: "get", action: format!("{}/settings", repo.base()),
            input { r#type: "hidden", name: "tab", value: "secrets" }
            header { class: "repo-section-heading", h3 { "Secrets and variables > Actions" } }
            div { class: "repo-form-body",
                label { class: "repo-field", span { "Repository or environment" }
                    DraftSelect { name: "environment", initial_value: selected, options }
                }
                button { class: "btn", r#type: "submit", "Show values" }
                crate::webui::navigation::NavLink { href: format!("{}/settings?tab=secrets", repo.base()), "First environments" }
                if settings.environments.len() == 100 { crate::webui::navigation::NavLink { href: format!("{}/settings?tab=secrets&environments_page={}", repo.base(), settings.environments_page + 1), "Next environments" } }
            }
        }
        if let Some(environment) = &settings.environment {
            h2 { style: "overflow-wrap: anywhere", "Environment: {environment.name}" }
            {secrets::environment_section(repo, &environment.secrets, &environment.variables, settings.secrets_enabled, &environment.name)}
        } else {
            {secrets::section(repo, &settings.secrets, &settings.variables, settings.secrets_enabled)}
        }
        form { class: "repo-settings-section panel", method: "post", action: repo.action("environment-create"),
            {csrf(&repo.csrf_token)} {repository_id(repo.id)}
            header { class: "repo-section-heading", h3 { "New environment" } }
            div { class: "repo-form-body",
                label { class: "repo-field", span { "Environment name" }
                    DraftInput { name: "environment", initial_value: String::new(), maxlength: Some(510), required: true }
                }
                p { "Use this name in a job's environment field to make that environment's values available. Environment names are case-insensitive." }
            }
            div { class: "repo-form-actions", button { class: "btn", r#type: "submit", "Create environment" } }
        }
    }
}
