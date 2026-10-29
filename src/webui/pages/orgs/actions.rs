use crate::webui::components::{DraftInput, DraftSelect, DraftTextarea, SelectOption};
use dioxus::prelude::*;

#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Panel {
    #[serde(default)]
    pub oidc_claim_keys: String,
    #[serde(default)]
    pub oidc_properties: Vec<String>,
    pub csrf_token: String,
    pub org_id: i64,
    pub encryption_enabled: bool,
    pub values: Vec<Value>,
    #[serde(default)]
    pub previous: Option<String>,
    #[serde(default)]
    pub next: Option<String>,
}
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Value {
    pub name: String,
    /// Secrets never populate this field.
    pub value: String,
    pub secret: bool,
    pub visibility: String,
    pub repositories: String,
}
fn tokens(panel: &Panel) -> Element {
    rsx! {
        {crate::webui::pages::fields::csrf(&panel.csrf_token)}
        input { r#type: "hidden", name: "org_id", value: "{panel.org_id}" }
    }
}
fn access(visibility: &str, repositories: &str) -> Element {
    rsx! {
        label { class: "repo-field", span { "Repository access" }
            DraftSelect { name: "visibility", initial_value: visibility.to_owned(), required: true,
                options: vec![SelectOption::new("private", "Private repositories"), SelectOption::new("all", "All organization repositories"), SelectOption::new("selected", "Selected repositories")]
            }
        }
        label { class: "repo-field", span { "Selected repository IDs" }
            DraftInput { name: "repositories", initial_value: repositories.to_owned() }
            small { "For selected access only. Separate numeric repository IDs with commas. Repositories must belong to this organization." }
        }
    }
}
pub fn section(panel: &Panel) -> Element {
    let base = format!("/settings/organizations/{}/actions", panel.org_id);
    rsx! {
        section { class: "org-section repo-settings-section panel",
            header { class: "repo-section-heading", div { h2 { "OpenID Connect" } p { "Repositories must opt in to use this subject template. Configure matching trust conditions in your cloud provider before changing it." } } }
            form { class: "repo-form-body", method: "post", action: format!("{base}/oidc-save"),
                {tokens(panel)}
                label { class: "repo-field", span { "Subject claim keys, in order" }
                    DraftInput { name: "include_claim_keys", initial_value: panel.oidc_claim_keys.clone() }
                    small { "Separate keys with commas, for example: repo, context, job_workflow_ref." }
                }
                button { class: "btn btn-primary", r#type: "submit", "Save OIDC template" }
            }
            div { class: "repo-form-body",
                h3 { "Repository property claims" }
                for name in &panel.oidc_properties {
                    form { method: "post", action: format!("{base}/oidc-property-delete"),
                        {tokens(panel)}
                        input { r#type: "hidden", name: "custom_property_name", value: name.clone() }
                        span { style: "overflow-wrap: anywhere", "{name} " }
                        button { class: "btn btn-danger", r#type: "submit", "Remove" }
                    }
                }
                form { method: "post", action: format!("{base}/oidc-property-add"),
                    {tokens(panel)}
                    label { class: "repo-field", span { "Existing repository property name" }
                        DraftInput { name: "custom_property_name", initial_value: String::new(), required: true, maxlength: Some(75) }
                        small { "Values appear as repo_property_NAME claims in every repository's tokens." }
                    }
                    button { class: "btn", r#type: "submit", "Include property" }
                }
            }
        }
        section { class: "org-section repo-settings-section panel",
            header { class: "repo-section-heading", div { h2 { "Actions secrets and variables" } p { "Organization values are inherited by eligible repositories. Repository values take precedence. Fork pull requests do not receive secrets." } } }
            if !panel.encryption_enabled { p { class: "repo-form-body", "Secret encryption is not configured. Ask the site administrator to configure SECRETS_MASTER_KEY." } }
            div { class: "repo-form-body",
                if panel.values.is_empty() { p { "No organization secrets or variables." } }
                for value in &panel.values {
                    article { key: "{value.secret}-{value.name}", style: "min-width: 0",
                        h3 { style: "overflow-wrap: anywhere", "{value.name}" }
                        if value.secret { p { "Secret · saved values cannot be read back." } }
                        form { method: "post", action: format!("{base}/{}", if value.secret {"secret-access"} else {"variable-save"}),
                            {tokens(panel)}
                            input { r#type: "hidden", name: "previous", value: value.name.clone() }
                            if value.secret { input { r#type: "hidden", name: "name", value: value.name.clone() } }
                            else {
                                label { class: "repo-field", span { "Variable name" } DraftInput { name: "name", initial_value: value.name.clone(), maxlength: Some(255), required: true } }
                                label { class: "repo-field", span { "Value" } DraftTextarea { name: "value", initial_value: value.value.clone(), rows: 3, maxlength: Some(49152) } }
                            }
                            {access(&value.visibility, &value.repositories)}
                            button { class: "btn", r#type: "submit", if value.secret {"Update secret access"} else {"Update variable"} }
                        }
                        form { method: "post", action: format!("{base}/{}", if value.secret {"secret-delete"} else {"variable-delete"}),
                            {tokens(panel)}
                            input { r#type: "hidden", name: "name", value: value.name.clone() }
                            label { class: "repo-check-label", input { r#type: "checkbox", name: "confirm", value: "yes", required: true } "Confirm deletion" }
                            button { class: "btn btn-danger", r#type: "submit", "Delete" }
                        }
                    }
                }
                {crate::webui::pages::discovery::page_links(panel.previous.clone(), panel.next.clone(), "Secrets and variables")}
            }
        }
        for secret in [false, true] {
            if !secret || panel.encryption_enabled {
                form { class: "repo-settings-section panel", method: "post", action: format!("{base}/{}", if secret {"secret-save"} else {"variable-create"}),
                    {tokens(panel)}
                    header { class: "repo-section-heading", h3 { if secret {"New or replacement organization secret"} else {"New organization variable"} } }
                    div { class: "repo-form-body",
                        label { class: "repo-field", span { "Name" } DraftInput { name: "name", initial_value: String::new(), maxlength: Some(255), required: true } }
                        label { class: "repo-field", span { if secret {"Secret"} else {"Value"} } DraftTextarea { name: "value", initial_value: String::new(), rows: 3, maxlength: Some(49152) } }
                        {access("private", "")}
                    }
                    div { class: "repo-form-actions", button { class: "btn btn-primary", r#type: "submit", if secret {"Save secret"} else {"Add variable"} } }
                }
            }
        }
    }
}
