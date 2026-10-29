use super::super::chrome::{csrf, repository_id, RepoChrome};
use crate::webui::components::{DraftInput, DraftTextarea};
use dioxus::prelude::*;

#[derive(Clone, Debug, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Secret {
    pub name: String,
    pub updated_at: String,
}

#[derive(Clone, Debug, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Variable {
    pub name: String,
    pub value: String,
}

pub fn section(
    repo: &RepoChrome,
    secrets: &[Secret],
    variables: &[Variable],
    enabled: bool,
) -> Element {
    values_section(repo, secrets, variables, enabled, None)
}

pub fn environment_section(
    repo: &RepoChrome,
    secrets: &[Secret],
    variables: &[Variable],
    enabled: bool,
    environment: &str,
) -> Element {
    values_section(repo, secrets, variables, enabled, Some(environment))
}

fn action(repo: &RepoChrome, environment: Option<&str>, command: &str) -> String {
    repo.action(&format!(
        "{}{command}",
        if environment.is_some() {
            "environment-"
        } else {
            ""
        }
    ))
}

fn fields(repo: &RepoChrome, environment: Option<&str>) -> Element {
    rsx! {
        {csrf(&repo.csrf_token)} {repository_id(repo.id)}
        if let Some(environment) = environment { input { r#type: "hidden", name: "environment", value: environment.to_owned() } }
    }
}

fn values_section(
    repo: &RepoChrome,
    secrets: &[Secret],
    variables: &[Variable],
    enabled: bool,
    environment: Option<&str>,
) -> Element {
    let scope = if environment.is_some() {
        "environment"
    } else {
        "repository"
    };
    rsx! {
        {scoped_secret_section(repo, secrets, enabled, "actions", environment)}
        section { class: "repo-settings-section panel",
            header { class: "repo-section-heading", div { h3 { "Actions variables" } p { "Variables are readable by repository readers and available in workflows as vars.NAME." } } }
            div { class: "repo-form-body",
                if environment.is_some() { p { "These variables are available only to jobs that declare this environment." } }
                if variables.is_empty() { p { "No {scope} variables." } }
                for variable in variables {
                    form { key: "edit-{variable.name}", method: "post", action: action(repo, environment, "variable-save"),
                        {fields(repo, environment)}
                        input { r#type: "hidden", name: "previous", value: variable.name.clone() }
                        label { class: "repo-field", span { "Name" } DraftInput { name: "name", initial_value: variable.name.clone(), maxlength: Some(255), required: true } }
                        label { class: "repo-field", span { "Value" } DraftTextarea { name: "value", initial_value: variable.value.clone(), rows: 3, maxlength: Some(49152) } }
                        button { class: "btn", r#type: "submit", "Update variable" }
                    }
                    form { method: "post", action: action(repo, environment, "variable-delete"),
                        {fields(repo, environment)}
                        input { r#type: "hidden", name: "name", value: variable.name.clone() }
                        label { class: "repo-check-label", input { r#type: "checkbox", name: "confirm", value: "yes", required: true } "Confirm deletion" }
                        button { class: "btn btn-danger", r#type: "submit", "Delete variable" }
                    }
                }
            }
        }
        form { class: "repo-settings-section panel", method: "post", action: action(repo, environment, "variable-save"),
            {fields(repo, environment)}
            header { class: "repo-section-heading", div { h3 { "New {scope} variable" } } }
            div { class: "repo-form-body",
                label { class: "repo-field", span { "Name" } DraftInput { name: "name", initial_value: String::new(), maxlength: Some(255), required: true } }
                label { class: "repo-field", span { "Value" } DraftTextarea { name: "value", initial_value: String::new(), rows: 3, maxlength: Some(49152) } }
            }
            div { class: "repo-form-actions", button { class: "btn btn-primary", r#type: "submit", "Add variable" } }
        }
    }
}

pub fn secret_section(repo: &RepoChrome, secrets: &[Secret], enabled: bool, kind: &str) -> Element {
    scoped_secret_section(repo, secrets, enabled, kind, None)
}

fn scoped_secret_section(
    repo: &RepoChrome,
    secrets: &[Secret],
    enabled: bool,
    kind: &str,
    environment: Option<&str>,
) -> Element {
    let scope = if environment.is_some() {
        "environment"
    } else {
        "repository"
    };
    let (title, description) = if kind == "dependabot" {
        ("Dependabot secrets", "Dependabot secrets are encrypted and kept separate from Actions secrets. They are not available to workflows.")
    } else {
        ("Actions secrets", "Secrets are encrypted and available to workflows as secrets.NAME. Fork pull requests do not receive secrets.")
    };
    rsx! {
        section { class: "repo-settings-section panel",
            header { class: "repo-section-heading", div { h3 { "{title}" } p { "{description}" } } }
            if !enabled {
                div { class: "repo-form-body", p { "Secret encryption is not configured. Ask the site administrator to configure SECRETS_MASTER_KEY." } }
            }
            div { class: "repo-form-body",
                if environment.is_some() { p { "These secrets are available only to jobs that declare this environment." } }
                if secrets.is_empty() { p { "No {scope} secrets." } }
                for secret in secrets {
                    form { key: "{kind}-{secret.name}", method: "post", action: action(repo, environment, "secret-delete"),
                        {fields(repo, environment)}
                        input { r#type: "hidden", name: "secret_kind", value: kind }
                        input { r#type: "hidden", name: "name", value: secret.name.clone() }
                        p { strong { style: "overflow-wrap: anywhere", "{secret.name}" } }
                        p { class: "text-sm text-muted-foreground", "Updated {secret.updated_at}" }
                        label { class: "repo-check-label", input { r#type: "checkbox", name: "confirm", value: "yes", required: true } "Confirm deletion" }
                        button { class: "btn btn-danger", r#type: "submit", "Delete secret" }
                    }
                }
            }
        }
        if enabled {
            form { key: "{kind}-new", class: "repo-settings-section panel", method: "post", action: action(repo, environment, "secret-save"),
                {fields(repo, environment)}
                input { r#type: "hidden", name: "secret_kind", value: kind }
                header { class: "repo-section-heading", div { h3 { "New or updated {scope} secret" } p { "Use an existing name to replace its value. Saved values cannot be read back." } } }
                div { class: "repo-form-body",
                    label { class: "repo-field", span { "Name" } DraftInput { name: "name", initial_value: String::new(), maxlength: Some(255), required: true } }
                    label { class: "repo-field", span { "Secret" } DraftTextarea { name: "value", initial_value: String::new(), rows: 6, maxlength: Some(49152) } }
                }
                div { class: "repo-form-actions", button { class: "btn btn-primary", r#type: "submit", "Save secret" } }
            }
        }
    }
}
