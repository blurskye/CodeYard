//! Admin Actions page: whether workflows run on this site, the limits on each run, and the
//! runner image. Repositories can set lower limits in their own settings.
use crate::webui::pages::admin::{admin_layout, Section};
use crate::webui::pages::fields::{csrf, notice, text_field, SUBMIT};
use dioxus::prelude::*;
use serde::{Deserialize, Serialize};

/// The form's values as text, so a refused number comes back as it was typed. An empty limit
/// means none; an empty timeout or image means the server's own.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Settings {
    pub enabled: bool,
    pub run_timeout_minutes: String,
    pub job_cpus: String,
    pub job_memory_gib: String,
    pub runner_image: String,
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
                enabled: true,
                run_timeout_minutes: String::new(),
                job_cpus: "4".into(),
                job_memory_gib: String::new(),
                runner_image: String::new(),
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
        let html = dioxus_ssr::render_element(render(page()));
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
