use super::{compact_date, DraftInput, DraftSelect, SelectOption};
use crate::webui::pages::fields::{csrf, FIELD};
use dioxus::prelude::*;

#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct InteractionRestriction {
    pub limit: String,
    pub origin: String,
    pub expires_at: String,
}
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct InteractionPanel {
    pub scope: String,
    pub scope_id: i64,
    pub csrf_token: String,
    pub restriction: Option<InteractionRestriction>,
    pub can_set_restriction: bool,
    pub cap: Option<PullCapForm>,
    pub bypass: Vec<String>,
}
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct PullCapForm {
    pub enabled: bool,
    pub maximum: String,
    pub include_drafts: bool,
}

#[component]
pub fn InteractionSettings(panel: InteractionPanel) -> Element {
    let inherited = panel.scope == "repository"
        && panel
            .restriction
            .as_ref()
            .is_some_and(|restriction| restriction.origin != "repository");
    let selected = panel
        .restriction
        .as_ref()
        .map(|restriction| restriction.limit.clone())
        .unwrap_or_else(|| "existing_users".into());
    rsx! {
        section { class: "identity-settings-section", aria_labelledby: "interaction-limits-title",
            header { class: "identity-section-header", h2 { id: "interaction-limits-title", "Interaction limits" } p { "Temporarily limit comments, issues and pull requests. Owner-level limits apply to public repositories; collaborators can still participate." } }
            if let Some(restriction) = &panel.restriction { p { "{restriction.limit} · {restriction.origin} · Until {compact_date(&restriction.expires_at)}" } } else { p { "No active interaction limit." } }
            if !panel.can_set_restriction {p {"Only organization owners can change interaction restrictions."}}
            else if inherited { p { "This limit is managed by the repository's owner. Change or remove it in their settings." } }
            else {
                form { method: "post", action: "/settings/interaction-limits", class: "identity-form",
                    {csrf(&panel.csrf_token)}
                    input { r#type: "hidden", name: "scope", value: panel.scope.clone() }
                    input { r#type: "hidden", name: "scope_id", value: panel.scope_id.to_string() }
                    div { class: "space-y-2", label { r#for: "interaction-group", "Allow interactions from" }
                        DraftSelect { id:Some("interaction-group".into()),name:"limit",class:FIELD,initial_value:selected,options:vec![SelectOption::new("existing_users","Existing users and prior contributors"),SelectOption::new("contributors_only","Prior contributors and collaborators"),SelectOption::new("collaborators_only","Collaborators only")] }
                    }
                    div { class: "space-y-2", label { r#for: "interaction-expiry", "Duration" }
                        DraftSelect {id:Some("interaction-expiry".into()),name:"expiry",class:FIELD,initial_value:"one_day".to_owned(),options:vec![SelectOption::new("one_day","One day"),SelectOption::new("three_days","Three days"),SelectOption::new("one_week","One week"),SelectOption::new("one_month","One month"),SelectOption::new("six_months","Six months")]}
                    }
                    div {class:"identity-form-actions",button {r#type:"submit",class:"btn btn-primary","Set interaction limit"} if panel.restriction.is_some() {button {r#type:"submit",name:"remove",value:"yes",class:"btn","Remove interaction limit"}}}
                }
            }
        }
        if panel.cap.is_some() {PullCapControls {panel}}
    }
}

#[component]
fn PullCapControls(panel: InteractionPanel) -> Element {
    let Some(cap) = &panel.cap else {
        return rsx! {};
    };
    rsx! {
        section {class:"identity-settings-section",aria_labelledby:"pull-cap-title",
            header {class:"identity-section-header",h2 {id:"pull-cap-title","Pull request creation cap"} p {"Limit open pull requests per author. Organization caps count all public repositories together. Repository bypass lists override both caps."}}
            form {method:"post",action:"/settings/pull-request-cap",class:"identity-form",
                {csrf(&panel.csrf_token)}
                input {r#type:"hidden",name:"scope",value:panel.scope.clone()}
                input {r#type:"hidden",name:"scope_id",value:panel.scope_id.to_string()}
                div {class:"space-y-2",label {r#for:"pull-cap-enabled","Cap"}
                    DraftSelect {id:Some("pull-cap-enabled".into()),name:"enabled",class:FIELD,initial_value:cap.enabled.to_string(),options:vec![SelectOption::new("false","Disabled"),SelectOption::new("true","Enabled")]}
                }
                div {class:"space-y-2",label {r#for:"pull-cap-maximum","Maximum open pull requests (1–1000)"}
                    DraftInput {id:Some("pull-cap-maximum".into()),name:"max_open_pull_requests",class:FIELD,initial_value:cap.maximum.clone(),inputmode:Some("numeric".into()),pattern:Some("[0-9]{1,4}".into()),maxlength:Some(4),required:true}
                }
                div {class:"space-y-2",label {r#for:"pull-cap-drafts","Count draft pull requests"}
                    DraftSelect {id:Some("pull-cap-drafts".into()),name:"include_drafts",class:FIELD,initial_value:cap.include_drafts.to_string(),options:vec![SelectOption::new("false","Do not count drafts"),SelectOption::new("true","Count drafts")]}
                }
                div {class:"identity-form-actions",button {r#type:"submit",class:"btn btn-primary","Save pull request cap"}}
            }
            if panel.scope=="repository" {
                h3 {"Bypass list"}
                p {"At most 100 accounts can bypass pull request creation caps for this repository. Other repository permissions and interaction limits still apply."}
                for username in &panel.bypass {
                    div {class:"identity-item-row",span {class:"identity-item-copy","{username}"}
                        form {method:"post",action:"/settings/pull-request-bypass",
                            {csrf(&panel.csrf_token)}
                            input {r#type:"hidden",name:"repo_id",value:panel.scope_id.to_string()}
                            input {r#type:"hidden",name:"username",value:username.clone()}
                            button {r#type:"submit",name:"remove",value:"yes",class:"btn btn-danger btn-sm",aria_label:format!("Remove {username} from bypass list"),"Remove"}
                        }
                    }
                }
                form {method:"post",action:"/settings/pull-request-bypass",class:"identity-form",
                    {csrf(&panel.csrf_token)}
                    input {r#type:"hidden",name:"repo_id",value:panel.scope_id.to_string()}
                    div {class:"space-y-2",label {r#for:"pull-bypass-user","Username"}
                        DraftInput {id:Some("pull-bypass-user".into()),name:"username",class:FIELD,initial_value:String::new(),maxlength:Some(255),required:true}
                    }
                    div {class:"identity-form-actions",button {r#type:"submit",class:"btn","Add to bypass list"}}
                }
            }
        }
    }
}
