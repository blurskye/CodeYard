use crate::webui::{
    components::{DraftInput, DraftTextarea},
    navigation::NavLink,
    pages::repos::chrome::{chrome, csrf, repository_id, RepoChrome},
};
use dioxus::prelude::*;
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct RepositoryEventForm {
    pub event_type: String,
    pub client_payload: String,
    pub error: Option<String>,
}
pub fn repository_event(repo: &RepoChrome, form: &RepositoryEventForm) -> Element {
    chrome(
        repo,
        rsx! {section {class:"panel repo-form-body",
            h2 {"Send repository event"}
            p {"This sends a repository_dispatch event to matching workflows on the default branch. Actions settings and site execution approval still apply. Events with no eligible listeners are accepted without queuing runs."}
            p {"Payloads are available to workflows and repository readers. Do not include secrets."}
            if let Some(error)=&form.error {p {class:"form-error",role:"alert","{error}"}}
            form {method:"post",action:format!("{}/actions/dispatch-event",repo.base()),class:"repo-form-body",
                {csrf(&repo.csrf_token)} {repository_id(repo.id)}
                label {class:"repo-field",span {"Event type (1–100 characters)"} DraftInput {name:"event_type",initial_value:form.event_type.clone(),required:true,maxlength:Some(400)}}
                label {class:"repo-field",span {"Client payload (JSON object, up to ten top-level properties and 64 KiB; blank to omit)"} DraftTextarea {name:"client_payload",initial_value:form.client_payload.clone(),rows:12,maxlength:Some(65536)}}
                button {r#type:"submit",class:"btn btn-primary",disabled:!repo.can_write||repo.is_archived,"Send event"}
            }
            NavLink {href:format!("{}/actions?event=repository_dispatch",repo.base()),"View repository event workflow runs"}
        }},
    )
}
