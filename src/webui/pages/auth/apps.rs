//! Native GitHub App registration and installation forms.
use crate::webui::pages::fields::{csrf, FIELD, SUBMIT};
use dioxus::prelude::*;

#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Model {
    pub mode: String,
    pub name: String,
    pub slug: String,
    pub permissions: String,
    pub redirect: String,
    pub applications: Vec<(String, String)>,
    pub registered: Vec<(i64, String, String)>,
    pub credentials: Option<(String, String, String)>,
}
pub fn page(token: &str, model: Model) -> Element {
    rsx! { section { class: "identity-auth",
        h1 { "GitHub Apps" }
        if let Some((client,secret,key))=model.credentials {
            p { "Save these credentials now. The private key and client secret will not be shown again." }
            dl {
                dt { "Client ID" } dd { class: "break-all font-mono", "{client}" }
                dt { "Client secret" } dd { class: "break-all font-mono", "{secret}" }
                dt { "Private key" } dd { pre { class: "overflow-auto", "{key}" } }
            }
        } else if model.mode=="new" {
            form { method: "post", action: "/settings/apps/new", class: "identity-auth-form",
                {csrf(token)}
                label { "Name" input { name: "name", required: true, maxlength: "100", class: FIELD } }
                label { "Slug (lowercase letters, numbers and hyphens)" input { name: "slug", required: true, maxlength: "80", class: FIELD } }
                label { "Homepage and OAuth callback URL" input { name: "homepage", r#type: "url", required: true, class: FIELD } }
                label { "Description" input { name: "description", maxlength: "1000", class: FIELD } }
                label { "Repository permissions (JSON)" textarea { name: "permissions", class: FIELD, "{{\"contents\":\"read\"}}" } }
                label { "Events (comma-separated)" input { name: "events", value: "push", class: FIELD } }
                button { r#type: "submit", class: SUBMIT, "Register GitHub App" }
            }
        } else if model.mode=="manifest" {
            h2 { "Register {model.name}" }
            p { class: "break-all", "Repository permissions: {model.permissions}" }
            p { class: "break-all", "{model.redirect}" }
            p { "Only approve a manifest from an application you trust. Its server will receive the new private key and client secret." }
            form { method: "post", action: "/settings/apps/new", class: "identity-auth-form",
                {csrf(token)}
                input { r#type: "hidden", name: "manifest_code", value: model.slug.clone() }
                button { r#type: "submit", name: "decision", value: "approve_manifest", class: SUBMIT, "Approve GitHub App manifest" }
            }
            a { href: "/settings/apps", "Cancel" }
        } else if model.mode=="manifest-ready" {
            p { "Use this one-time conversion code within one hour:" }
            code { class: "break-all", "{model.slug}" }
        } else if model.mode=="install" {
            h2 { "Install {model.name}" }
            p { class: "break-all", "Requested repository permissions: {model.permissions}" }
            form { method: "post", action: format!("/apps/{}/installations/new",model.slug), class: "identity-auth-form",
                {csrf(token)}
                label { "Organization ID (leave empty for your own account)" input { name: "organization_id", class: FIELD } }
                label { "Repository IDs (comma-separated; empty selects no repositories)" input { name: "repository_ids", class: FIELD } }
                label { input { r#type: "checkbox", name: "all_repositories", value: "yes" } "All current and future repositories" }
                button { r#type: "submit", class: SUBMIT, "Install GitHub App" }
                button { r#type: "submit", name: "decision", value: "request", class: SUBMIT, "Request organization installation" }
            }
        } else if model.mode=="installations" {
            h2 { "Installed GitHub Apps" }
            a { href: "/settings/apps", "Your registered apps" }
            for (name,id) in model.applications {
                article {
                    h3 { "{name}" }
                    form { method: "post", action: format!("/settings/apps/installations/{id}/delete"),
                        {csrf(token)}
                        button { r#type: "submit", class: SUBMIT, "Uninstall GitHub App" }
                    }
                }
            }
        } else {
            a { href: "/settings/apps/new", "Register GitHub App" }
            a { href: "/settings/apps/installations", "Installed GitHub Apps" }
            p { "Lost credentials can be regenerated. This replaces the app's private key and OAuth client secret." }
            for (id,name,slug) in model.registered {
                article { h2 { "{name}" } a { href: format!("/apps/{slug}/installations/new"), "Install" }
                    form { method: "post", action: format!("/settings/apps/{slug}/regenerate"),
                        {csrf(token)}
                        button { r#type: "submit", class: SUBMIT, "Regenerate credentials" }
                    }
                    form { method: "post", action: format!("/settings/apps/{id}/delete"),
                        {csrf(token)}
                        p { "Deleting the app revokes its signing key, installations and derived credentials." }
                        label { input { r#type: "checkbox", name: "confirm", value: "yes", required: true } "Permanently delete this GitHub App" }
                        button { r#type: "submit", class: SUBMIT, "Delete GitHub App" }
                    }
                }
            }
        }
    } }
}
