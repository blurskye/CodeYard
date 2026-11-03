use super::{
    fields::{controls, select},
    Page as OrganizationPage,
};
use crate::{
    components::{DraftInput, SelectOption},
    encode_component,
    navigation::NavLink,
};
use dioxus::prelude::*;
use std::collections::BTreeMap;

pub const REGISTRY_TYPES: [&str; 15] = [
    "maven_repository",
    "nuget_feed",
    "goproxy_server",
    "npm_registry",
    "rubygems_server",
    "cargo_registry",
    "composer_repository",
    "docker_registry",
    "git_source",
    "helm_registry",
    "hex_organization",
    "hex_repository",
    "pub_repository",
    "python_index",
    "terraform_registry",
];
pub const AUTH_TYPES: [&str; 8] = [
    "token",
    "username_password",
    "oidc_azure",
    "oidc_aws",
    "oidc_jfrog",
    "oidc_cloudsmith",
    "oidc_gcp",
    "anonymous",
];
/// Byte bound of an ordinary registry field.
pub const FIELD_MAX: usize = 4096;
/// Byte bound of the repository selection: 1000 IDs of up to 19 digits joined by ", ".
pub const SELECTED_REPOSITORY_IDS_MAX: usize = 21_000;

/// The byte bound of a registry form field other than the credential.
pub fn field_max(name: &str) -> usize {
    if name == "selected_repository_ids" {
        SELECTED_REPOSITORY_IDS_MAX
    } else {
        FIELD_MAX
    }
}

pub const OIDC_FIELDS: [(&str, &str); 16] = [
    ("tenant_id", "Azure tenant ID"),
    ("client_id", "Azure client ID"),
    ("aws_region", "AWS region"),
    ("account_id", "AWS account ID"),
    ("role_name", "AWS role name"),
    ("domain", "CodeArtifact domain"),
    ("domain_owner", "CodeArtifact domain owner"),
    ("jfrog_oidc_provider_name", "JFrog OIDC provider"),
    ("audience", "OIDC audience"),
    ("identity_mapping_name", "JFrog identity mapping"),
    ("namespace", "Cloudsmith namespace"),
    ("service_slug", "Cloudsmith service account"),
    ("api_host", "Cloudsmith API host"),
    (
        "workload_identity_provider",
        "GCP workload identity provider",
    ),
    ("service_account", "GCP service account"),
    (
        "selected_repository_ids",
        "Selected repository IDs, separated by commas",
    ),
];

#[derive(Clone, Debug, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Registry {
    pub name: String,
    pub registry_type: String,
    pub url: String,
    pub visibility: String,
    pub auth_type: String,
    pub username: String,
    pub replaces_base: bool,
    pub fields: BTreeMap<String, String>,
}

#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Page {
    pub registries: Vec<Registry>,
    pub editing: Option<Registry>,
    pub page: i64,
    pub has_more: bool,
}

pub(super) fn panel(page: &OrganizationPage) -> Element {
    let (Some(org), Some(registries)) = (&page.selected, &page.private_registries) else {
        return rsx! {};
    };
    let target = format!(
        "/settings/organizations/{}/private-registries?tab=private-registries",
        org.id
    );
    let empty = Registry {
        registry_type: "docker_registry".into(),
        auth_type: "token".into(),
        visibility: "private".into(),
        ..Registry::default()
    };
    let editing = registries.editing.as_ref().unwrap_or(&empty);
    let field = |name: &str| {
        if page
            .draft
            .get("action")
            .is_some_and(|v| v == "registry-save")
        {
            if let Some(value) = page.draft.get(name) {
                return value.clone();
            }
        }
        match name {
            "name" => editing.name.clone(),
            "registry_type" => editing.registry_type.clone(),
            "url" => editing.url.clone(),
            "visibility" => editing.visibility.clone(),
            "auth_type" => editing.auth_type.clone(),
            "username" => editing.username.clone(),
            "replaces_base" => editing.replaces_base.to_string(),
            _ => editing.fields.get(name).cloned().unwrap_or_default(),
        }
    };
    rsx! {
        section { class: "org-section", aria_labelledby: "private-registries-title",
            div { class: "org-section-header", div {
                h2 { id: "private-registries-title", "Private registries" }
                p { "Store credentials and access settings for dependency integrations. Codeyard does not run a dependency update service or exchange registry OIDC tokens." }
            } }
            ul { class: "org-list",
                for registry in &registries.registries {
                    li { key: "{registry.name}", class: "org-row",
                        div { class: "org-row-main",
                            h3 { NavLink { href: format!("/settings/organizations/{}?tab=private-registries&registry={}", org.id, encode_component(&registry.name)), "{registry.name}" } }
                            p { "{registry.url} · {registry.auth_type} · {registry.visibility}" }
                        }
                        form { method: "post", action: target.clone(), class: "org-inline-form",
                            input { r#type: "hidden", name: "name", value: registry.name.clone() }
                            {controls(page, "registry-delete", "Delete")}
                        }
                    }
                }
            }
            nav { class: "repo-pagination", aria_label: "Private registries",
                if registries.page > 1 { NavLink { href: format!("/settings/organizations/{}?tab=private-registries&page={}", org.id, registries.page - 1), "Previous" } }
                if registries.has_more { NavLink { href: format!("/settings/organizations/{}?tab=private-registries&page={}", org.id, registries.page + 1), "Next" } }
            }
            section { class: "org-panel",
                header { class: "org-panel-header", h3 { if editing.name.is_empty() { "New registry" } else { "Edit registry" } } }
                form { method: "post", action: target, class: "org-panel-body org-form",
                    input { r#type: "hidden", name: "name", value: field("name") }
                    {select("Registry type", "registry_type", field("registry_type"), REGISTRY_TYPES.iter().map(|v| SelectOption::new(*v, v.replace('_', " "))).collect())}
                    label { span { "Registry URL" } DraftInput { class: "org-field", name: "url", initial_value: field("url"), maxlength: Some(4096) } }
                    {select("Authentication type (cannot be changed after creation)", "auth_type", field("auth_type"), AUTH_TYPES.iter().map(|v| SelectOption::new(*v, v.replace('_', " "))).collect())}
                    label { span { "Username" } DraftInput { class: "org-field", name: "username", initial_value: field("username"), maxlength: Some(1024) } }
                    label { span { "Token or password" } input { class: "org-field", r#type: "password", name: "value", autocomplete: "new-password", maxlength: "16384" } small { "Leave blank to keep the stored credential when editing. Do not provide a credential for anonymous or OIDC authentication." } }
                    {select("Repository access", "visibility", field("visibility"), ["all", "private", "selected"].iter().map(|v| SelectOption::new(*v, *v)).collect())}
                    {select("Replace the base registry", "replaces_base", field("replaces_base"), vec![SelectOption::new("false", "No"), SelectOption::new("true", "Yes")])}
                    for (name, label) in OIDC_FIELDS {
                        label { span { "{label}" } DraftInput { class: "org-field", name: name.to_owned(), initial_value: field(name), maxlength: Some(field_max(name) as u32) } }
                    }
                    {controls(page, "registry-save", "Save registry")}
                }
            }
        }
    }
}
