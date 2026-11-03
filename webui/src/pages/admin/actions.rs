//! Admin Actions page: whether workflows run on this site, the limits on each run, and the
//! runner image. Repositories can set lower limits in their own settings.
use crate::components::{DraftSelect, SelectOption};
use crate::pages::admin::{admin_layout, Section};
use crate::pages::fields::{csrf, notice, text_field, FIELD, SUBMIT};
use dioxus::prelude::*;
use serde::{Deserialize, Serialize};

/// The form's values as text, so a refused number comes back as it was typed. An empty limit
/// means none; an empty timeout or image means the server's own.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Settings {
    pub default_engine: String,
    pub codeyard_actions_enabled: bool,
    pub act_enabled: bool,
    pub default_workflow_permissions: String,
    pub enabled: bool,
    pub run_timeout_minutes: String,
    pub job_cpus: String,
    pub job_memory_gib: String,
    pub runner_image: String,
    pub results_url: String,
    pub cache_size_gib: String,
    pub cache_retention_days: String,
    pub artifact_retention_days: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Page {
    pub csrf_token: String,
    pub settings: Settings,
    /// The server's own run timeout and runner image, used while the fields are empty.
    pub default_timeout_minutes: u32,
    pub default_image: String,
    /// `CI_MAX_ACTIVE_RUNS` on this node.
    pub runs_per_node: usize,
    pub message: Option<String>,
    pub is_error: bool,
}

pub fn render(page: Page) -> Element {
    let settings = &page.settings;
    admin_layout(
        Section::Actions,
        "Actions",
        "Whether workflows run on this site, and what each run may use.",
        rsx! {
            {notice(page.message.as_deref(), page.is_error)}
            section { class: "identity-admin-panel", aria_labelledby: "actions-runner-title",
                header { class: "identity-admin-panel-header",
                    div {
                        h2 { id: "actions-runner-title", "Runner" }
                        p { "Each node runs up to {page.runs_per_node} workflow runs at once, set with CI_MAX_ACTIVE_RUNS on that node." }
                    }
                }
                form { method: "post", action: "/admin/actions", class: "identity-form",
                    {csrf(&page.csrf_token)}
                    label { class: "identity-check",
                        input { r#type: "checkbox", name: "enabled", value: "on", checked: settings.enabled }
                        span { "Run workflows on this site" }
                    }
                    fieldset { class: "identity-fieldset",
                        legend { "CI engines" }
                        label { class: "block text-sm font-semibold", r#for: "default_engine", "Default engine" }
                        DraftSelect { class: FIELD, id: Some("default_engine".into()), name: "default_engine", initial_value: settings.default_engine.clone(), options: vec![
                            SelectOption { value: "codeyard-actions".into(), label: "codeyard-actions".into() },
                            SelectOption { value: "act".into(), label: "act".into() },
                        ] }
                        label { class: "identity-check", input { r#type: "checkbox", name: "codeyard_actions_enabled", value: "on", checked: settings.codeyard_actions_enabled } span { "Enable codeyard-actions" } }
                        label { class: "identity-check", input { r#type: "checkbox", name: "act_enabled", value: "on", checked: settings.act_enabled } span { "Enable act" } }
                        p { class: "identity-form-hint", "At least one engine, including the site default, must stay enabled." }
                    }
                    fieldset { class: "identity-fieldset",
                        legend { "Workflow token permissions" }
                        label { class: "block text-sm font-semibold", r#for: "default_workflow_permissions", "Default GITHUB_TOKEN permissions" }
                        DraftSelect { class: FIELD, id: Some("default_workflow_permissions".into()), name: "default_workflow_permissions", initial_value: settings.default_workflow_permissions.clone(), options: vec![
                            SelectOption { value: "restricted".into(), label: "Restricted: contents and metadata read".into() },
                            SelectOption { value: "permissive".into(), label: "Permissive: read and write".into() },
                        ] }
                        p { class: "identity-form-hint", "An explicit workflow or job permissions block replaces this default. Fork pull requests are always read-only." }
                    }
                    fieldset { class: "identity-fieldset",
                        legend { "Limits on each run" }
                        p { class: "identity-form-hint", "Leave a field empty for no limit. A repository can set lower limits in its settings." }
                        {text_field("run_timeout_minutes", "Timeout in minutes", settings.run_timeout_minutes.clone(), "number", false, 5)}
                        p { class: "identity-form-hint", "Empty uses the server's {page.default_timeout_minutes} minutes." }
                        div { class: "identity-field-row",
                            {text_field("job_cpus", "CPUs per job", settings.job_cpus.clone(), "number", false, 4)}
                            {text_field("job_memory_gib", "Memory per job in GiB", settings.job_memory_gib.clone(), "number", false, 4)}
                        }
                    }
                    fieldset { class: "identity-fieldset",
                        legend { "Runner image" }
                        {text_field("runner_image", "Image for ubuntu-latest and the other Ubuntu labels", settings.runner_image.clone(), "text", false, 255)}
                        p { class: "identity-form-hint break-all", "Empty uses " code { "{page.default_image}" } ". A node pulls a missing image on its first run." }
                    }
                    fieldset { class: "identity-fieldset",
                        legend { "Caches and artifacts" }
                        {text_field("results_url", "Server URL reachable by job containers", settings.results_url.clone(), "url", false, 2048)}
                        p { class: "identity-form-hint", "Empty uses PUBLIC_URL. Use an HTTP or HTTPS origin without a path." }
                        {text_field("cache_size_gib", "Cache size per repository in GiB", settings.cache_size_gib.clone(), "number", false, 5)}
                        {text_field("cache_retention_days", "Expire caches after days without use", settings.cache_retention_days.clone(), "number", false, 3)}
                        {text_field("artifact_retention_days", "Artifact retention in days", settings.artifact_retention_days.clone(), "number", false, 3)}
                        p { class: "identity-form-hint", "Defaults: 10 GiB of cache per repository, 7 days without use, and 90 days for artifacts." }
                    }
                    div { class: "identity-form-actions", button { r#type: "submit", class: SUBMIT, "Save" } }
                }
            }
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn page() -> Page {
        Page {
            csrf_token: "csrf-marker".into(),
            settings: Settings {
                default_engine: "codeyard-actions".into(),
                codeyard_actions_enabled: true,
                act_enabled: true,
                default_workflow_permissions: "restricted".into(),
                enabled: true,
                run_timeout_minutes: String::new(),
                job_cpus: "4".into(),
                job_memory_gib: String::new(),
                runner_image: String::new(),
                results_url: String::new(),
                cache_size_gib: String::new(),
                cache_retention_days: String::new(),
                artifact_retention_days: String::new(),
            },
            default_timeout_minutes: 60,
            default_image: "ghcr.io/catthehacker/ubuntu:act-latest".into(),
            runs_per_node: 4,
            message: None,
            is_error: false,
        }
    }

    #[test]
    fn the_form_shows_the_saved_limits_and_the_server_defaults() {
        let html = crate::tests::in_scope(|| dioxus_ssr::render_element(render(page())));
        assert!(html.contains("aria-label=\"Administration\""), "{html}");
        assert!(html.contains("action=\"/admin/actions\""), "{html}");
        assert!(html.contains("name=\"job_cpus\""), "{html}");
        assert!(html.contains("value=\"4\""), "{html}");
        assert!(html.contains("checked"), "{html}");
        assert!(html.contains("60 minutes"), "{html}");
        assert!(
            html.contains("ghcr.io/catthehacker/ubuntu:act-latest"),
            "{html}"
        );
        assert!(html.contains("up to 4 workflow runs"), "{html}");
        assert_eq!(html.matches("csrf-marker").count(), 1);
    }

    #[test]
    fn the_model_survives_hydration() {
        let model = page();
        let json = serde_json::to_string(&model).unwrap();
        assert_eq!(serde_json::from_str::<Page>(&json).unwrap(), model);
    }
}
