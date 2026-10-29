//! Runner settings use native forms before and after hydration.
use crate::webui::components::{DraftInput, DraftSelect, DraftTextarea, SelectOption};
use dioxus::prelude::*;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Page {
    pub csrf_token: String,
    pub title: String,
    pub back: String,
    pub action: String,
    pub groups: Vec<Group>,
    pub runners: Vec<Runner>,
    pub organization: bool,
    pub message: Option<String>,
    pub draft: HashMap<String, String>,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Group {
    pub id: i64,
    pub name: String,
    pub visibility: String,
    pub default: bool,
    pub public: bool,
    pub restricted: bool,
    pub workflows: String,
    pub repositories: String,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Runner {
    pub id: i64,
    pub name: String,
    pub online: bool,
    pub busy: bool,
    pub labels: String,
    pub group: Option<i64>,
    pub ephemeral: bool,
}
fn controls(page: &Page, action: &str) -> Element {
    rsx! {input {r#type:"hidden",name:"csrf_token",value:page.csrf_token.clone()} input {r#type:"hidden",name:"action",value:action.to_owned()} }
}
fn submitted(page: &Page, action: &str, id: i64) -> bool {
    page.draft.get("action").is_some_and(|v| v == action)
        && page.draft.get("id").is_none_or(|v| v == &id.to_string())
}
fn flag(page: &Page, action: &str, id: i64, key: &str, stored: bool) -> bool {
    if submitted(page, action, id) {
        page.draft.get(key).is_some_and(|v| v == "yes")
    } else {
        stored
    }
}
fn value(page: &Page, action: &str, id: i64, key: &str, stored: &str) -> String {
    if submitted(page, action, id) {
        page.draft
            .get(key)
            .cloned()
            .unwrap_or_else(|| stored.to_owned())
    } else {
        stored.to_owned()
    }
}
fn group_fields(page: &Page, group: Option<&Group>) -> Element {
    let id = group.map(|g| g.id).unwrap_or(0);
    let action = if group.is_some() {
        "group-save"
    } else {
        "group-create"
    };
    let name = value(
        page,
        action,
        id,
        "name",
        group.map(|g| g.name.as_str()).unwrap_or(""),
    );
    let visibility = value(
        page,
        action,
        id,
        "visibility",
        group.map(|g| g.visibility.as_str()).unwrap_or("all"),
    );
    let repos = value(
        page,
        action,
        id,
        "repositories",
        group.map(|g| g.repositories.as_str()).unwrap_or(""),
    );
    let workflows = value(
        page,
        action,
        id,
        "workflows",
        group.map(|g| g.workflows.as_str()).unwrap_or(""),
    );
    rsx! {
        label {class:"repo-field",span {"Name"} DraftInput {name:"name",initial_value:name,required:true,maxlength:Some(100)}}
        label {class:"repo-field",span {"Repository access"} DraftSelect {name:"visibility",aria_label:Some("Repository access".to_owned()),initial_value:visibility,required:true,options:vec![SelectOption::new("all","All repositories"),SelectOption::new("private","Private repositories"),SelectOption::new("selected","Selected repositories")]}}
        label {class:"repo-field",span {"Selected repository IDs"} DraftInput {name:"repositories",initial_value:repos} small {"Comma-separated IDs, for selected access only. Repositories must belong to this organization."}}
        label {class:"repo-check-label",input {r#type:"checkbox",name:"public",value:"yes",checked:flag(page,action,id,"public",group.is_some_and(|g|g.public))} "Allow public repositories"}
        label {class:"repo-check-label",input {r#type:"checkbox",name:"restricted",value:"yes",checked:flag(page,action,id,"restricted",group.is_some_and(|g|g.restricted))} "Restrict to selected workflows"}
        label {class:"repo-field",span {"Allowed workflows"} DraftTextarea {name:"workflows",initial_value:workflows,rows:3,maxlength:Some(49152)} small {"One owner/repository/.github/workflows/file.yml@ref per line. A commit SHA can pin an exact revision."}}
    }
}
pub fn render(page: Page) -> Element {
    rsx! {div {class:"org-page page-shell",
        a {href:page.back.clone(),"← Back to settings"}
        header {class:"org-page-header",div {h1 {"{page.title}"} p {"Self-hosted Codeyard Actions runners"}}}
        if let Some(message)=&page.message {div {role:"alert",class:"repo-alert repo-alert-error",p {"{message}"}}}
        p {class:"org-note","These are Codeyard cluster workers, not GitHub's runner executable. Registration uses the existing runner pairing path and requires a serving PostgreSQL core. The Podman socket is host-user access, not a sandbox. Changing runner access or labels conservatively stops running work in this scope."}
        if page.organization {section {class:"org-section repo-settings-section panel",h2 {"Runner groups"}
            for group in &page.groups {article {key:"group-{group.id}",class:"repo-form-body",
                h3 {"{group.name}"} if group.default {p {"Default group"}}
                form {method:"post",action:page.action.clone(),{controls(&page,"group-save")}
                    input {r#type:"hidden",name:"id",value:"{group.id}"}
                    {group_fields(&page,Some(group))}
                    button {r#type:"submit",class:"btn","Save group"}
                }
                if !group.default {form {method:"post",action:page.action.clone(),{controls(&page,"group-delete")}
                    input {r#type:"hidden",name:"id",value:"{group.id}"}
                    label {input {r#type:"checkbox",name:"confirm",value:"yes",required:true} "Confirm deletion (runners return to Default)"}
                    button {r#type:"submit",class:"btn btn-danger","Delete group"}
                }}
            }}
            form {method:"post",action:page.action.clone(),class:"repo-form-body",h3 {"Create a runner group"}
                {controls(&page,"group-create")}{group_fields(&page,None)}button {r#type:"submit",class:"btn btn-primary","Create group"}
            }
        }}
        section {class:"org-section repo-settings-section panel",h2 {"Runners"}
            if page.runners.is_empty() {p {"No registered runners."}}
            for runner in &page.runners {article {key:"runner-{runner.id}",class:"repo-form-body",
                h3 {"{runner.name}"} p {if runner.online {"Online"} else {"Offline"} if runner.busy {" · busy"} if runner.ephemeral {" · just-in-time (one run)"}}
                form {method:"post",action:page.action.clone(),{controls(&page,"labels")}
                    input {r#type:"hidden",name:"id",value:"{runner.id}"}
                    label {class:"repo-field",span {"Custom labels"} DraftInput {name:"labels",initial_value:value(&page,"labels",runner.id,"labels",&runner.labels)} small {"Comma-separated labels. self-hosted is always present and cannot be removed."}}
                    button {r#type:"submit",class:"btn","Save labels"}
                }
                if page.organization {form {method:"post",action:page.action.clone(),{controls(&page,"move")}
                    input {r#type:"hidden",name:"id",value:"{runner.id}"}
                    label {class:"repo-field",span {"Runner group"} DraftSelect {name:"group",initial_value:runner.group.map(|id|id.to_string()).unwrap_or_default(),required:true,options:page.groups.iter().map(|g|SelectOption::new(g.id.to_string(),g.name.clone())).collect::<Vec<_>>()}}
                    button {r#type:"submit",class:"btn","Move runner"}
                }}
                form {method:"post",action:page.action.clone(),{controls(&page,"runner-delete")}
                    input {r#type:"hidden",name:"id",value:"{runner.id}"}
                    label {input {r#type:"checkbox",name:"confirm",value:"yes",required:true} "Confirm removal and revoke this worker"}
                    button {r#type:"submit",class:"btn btn-danger","Remove runner"}
                }
            }}
        }
        section {class:"org-section repo-settings-section panel",h2 {"Register a runner"}
            p {"Credentials download as a file and are never stored in the page model. Registration and removal tokens expire after one hour. Keep them secret."}
            for (action,label) in [("registration-token","Download registration token"),("remove-token","Download removal token")] {form {method:"post",action:page.action.clone(),{controls(&page,action)}button {r#type:"submit",class:"btn","{label}"}}}
            p {"On the runner machine, supply the registration token through CLUSTER_JOIN_TOKEN and start codeyard serve --roles runner; it joins on its first start. To remove a runner, POST its removal token to /api/actions/runners/{{id}}/remove."}
            form {method:"post",action:page.action.clone(),class:"repo-form-body",{controls(&page,"jit")}
                h3 {"Just-in-time runner"}
                label {class:"repo-field",span {"Runner name"} DraftInput {name:"name",initial_value:value(&page,"jit",0,"name",""),required:true,maxlength:Some(100)}}
                if page.organization {label {class:"repo-field",span {"Group"} DraftSelect {name:"group",initial_value:value(&page,"jit",0,"group",&page.groups.first().map(|g|g.id.to_string()).unwrap_or_default()),required:true,options:page.groups.iter().map(|g|SelectOption::new(g.id.to_string(),g.name.clone())).collect::<Vec<_>>()}}}
                label {class:"repo-field",span {"Custom labels"} DraftInput {name:"labels",initial_value:value(&page,"jit",0,"labels","linux"),required:true}}
                label {class:"repo-field",span {"Work folder"} DraftInput {name:"work_folder",initial_value:value(&page,"jit",0,"work_folder","_work"),required:true,maxlength:Some(255)}}
                button {r#type:"submit",class:"btn btn-primary","Download JIT configuration"}
                p {"Supply encoded_jit_config through CODEYARD_RUNNER_JIT_CONFIG, then run codeyard --state-dir /path/to/runner-state cluster join followed by codeyard --state-dir /path/to/runner-state serve --roles runner. Keep the same state directory for both commands. The configuration selects the reserved runner name and work folder. A just-in-time runner is revoked after its first run."}
            }
        }
    }}
}
