//! Site settings: who can sign up and the AI crawler policy.
use super::nav::{admin_layout, Section};
use crate::components::{DraftInput, DraftSelect, SelectOption};
use crate::pages::fields::{csrf, notice};
use dioxus::prelude::*;

#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct AdminSettings {
    /// A private node reviews each signup request. A public one lets anyone register.
    pub private: bool,
    #[serde(default)]
    pub scaling: Vec<codeyard_support::cluster::ScalingPolicy>,
    /// `block` or `allow`.
    pub ai_crawlers: String,
    pub osv_base_url: String,
    pub osv_ecosystems: String,
}

/// One setting: a heading, a select and its own Save button, posting only its field. A
/// component, so its select runs inside the renderer's runtime.
#[component]
fn SelectSetting(
    token: String,
    name: String,
    title: String,
    label: String,
    hint: String,
    value: String,
    options: Vec<SelectOption>,
) -> Element {
    let title_id = format!("admin-{name}-title");
    rsx! {
        section { class: "identity-admin-panel", aria_labelledby: title_id.clone(),
            header { class: "identity-admin-panel-header",
                div { h2 { id: title_id, "{title}" } }
            }
            form { method: "post", action: "/admin/settings", class: "identity-admin-crawlers",
                {csrf(&token)}
                div { class: "identity-admin-crawlers-field",
                    label { r#for: name.clone(), "{label}" }
                    DraftSelect { id: Some(name.clone()), name: name.clone(), class: "border border-border bg-background",
                        initial_value: value, options }
                    p { class: "identity-admin-hint", "{hint}" }
                }
                button { r#type: "submit", class: "btn", "Save" }
            }
        }
    }
}

pub fn admin_settings_page(
    model: AdminSettings,
    token: &str,
    message: Option<&str>,
    error: bool,
) -> Element {
    admin_layout(
        Section::Settings,
        "Site settings",
        "Who can sign up, and how crawlers treat this site.",
        rsx! {
            {notice(message, error)}
            section {class:"identity-admin-panel",style:"min-width:0;overflow-wrap:anywhere",
                h2 {"Dependency vulnerabilities"}
                form {method:"post",action:"/admin/settings",
                    {csrf(token)}
                    label {"OSV mirror base URL" DraftInput {name:"osv_base_url",initial_value:model.osv_base_url,maxlength:Some(2048),required:true}}
                    label {"Enabled ecosystems (comma-separated)" DraftInput {name:"osv_ecosystems",initial_value:model.osv_ecosystems,maxlength:Some(256)}}
                    p {"npm, PyPI, crates.io, Go, Maven, RubyGems, Packagist, NuGet, Pub, Hex. Empty disables downloads. Mirrors must provide <ecosystem>/all.zip. Sync runs every six hours."}
                    button {r#type:"submit",class:"btn","Save vulnerability sources"}
                }
            }
            section { class: "identity-admin-panel",
                h2 { "Automatic scaling" }
                p { "Policies start paused and in dry-run mode. Storage nodes never scale down automatically. The maximum includes outstanding invitations." }
                for policy in &model.scaling {
                    form { method: "post", action: "/admin/settings",
                        {csrf(token)}
                        h3 { "{policy.role}" }
                        input { r#type: "hidden", name: "scaling_role", value: policy.role.clone() }
                        label { "Minimum nodes" input { r#type: "number", name: "scaling_minimum", min: "0", max: "1000", value: policy.minimum.to_string(), required: true } }
                        label { "Maximum nodes" input { r#type: "number", name: "scaling_maximum", min: "0", max: "1000", value: policy.maximum.to_string(), required: true } }
                        label { "Step" input { r#type: "number", name: "scaling_step", min: "1", max: "1000", value: policy.step.to_string(), required: true } }
                        label { "Cooldown (seconds)" input { r#type: "number", name: "scaling_cooldown", min: "60", max: "86400", value: policy.cooldown_seconds.to_string(), required: true } }
                        label { "Trigger (storage free percent, or runner queue age in seconds)" input { r#type: "number", name: "scaling_threshold", min: "1", max: "3600", value: policy.trigger_threshold.to_string(), required: true } }
                        label { "Trigger duration (seconds)" input { r#type: "number", name: "scaling_duration", min: "0", max: "86400", value: policy.trigger_seconds.to_string(), required: true } }
                        label { "Runner idle time (seconds)" input { r#type: "number", name: "scaling_idle", min: "60", max: "86400", value: policy.idle_seconds.to_string(), required: true } }
                        label { input { r#type: "checkbox", name: "scaling_paused", checked: policy.paused } "Paused" }
                        label { input { r#type: "checkbox", name: "scaling_dry_run", checked: policy.dry_run } "Dry run" }
                        button { r#type: "submit", class: "btn", "Save scaling policy" }
                    }
                }
            }
            SelectSetting { token: token.to_owned(), name: "node_mode", title: "Sign-up", label: "Who can join",
                hint: "By approval also asks visitors to sign in before they see anything. Pending requests are under Users.",
                value: if model.private { "private" } else { "public" },
                options: vec![
                    SelectOption::new("public", "Open: anyone can register"),
                    SelectOption::new("private", "By approval: an administrator approves each account"),
                ] }
            SelectSetting { token: token.to_owned(), name: "ai_crawlers", title: "AI crawlers", label: "AI crawler policy",
                hint: "Block disallows AI training crawlers in robots.txt and ai.txt, and asks crawlers not to use HTML pages. Allow lets every crawler in.",
                value: model.ai_crawlers,
                options: vec![
                    SelectOption::new("block", "Block (default)"),
                    SelectOption::new("allow", "Allow"),
                ] }
        },
    )
}
