//! Native Pages administration; there is deliberately no claim that sites are being hosted.
use super::chrome::{chrome, csrf, repository_id, RepoChrome};
use crate::webui::{
    components::{DraftCheckbox, DraftInput, DraftSelect, SelectOption},
    navigation::NavLink,
};
use dioxus::prelude::*;
#[derive(Clone, Debug, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Settings {
    pub build_type: String,
    pub branch: String,
    pub path: String,
    pub cname: String,
    pub https_enforced: bool,
    pub public: bool,
}
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Build {
    pub commit: String,
    pub status: String,
    pub error: String,
}
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Page {
    pub exists: bool,
    pub settings: Settings,
    pub builds: Vec<Build>,
    pub error: Option<String>,
}
pub fn render(repo: &RepoChrome, page: &Page) -> Element {
    let key = format!("{}:{}", repo.viewer_id, repo.id);
    // Keys reconcile in a fragment, not a lone dynamic component slot.
    rsx! {for key in [key] {PagesView {key:"{key}",repo:repo.clone(),page:page.clone()}}}
}
#[component]
fn PagesView(repo: RepoChrome, page: Page) -> Element {
    let repo = &repo;
    let page = &page;
    let settings = &page.settings;
    chrome(
        repo,
        rsx! {
            section {class:"repo-settings-page",
                header {class:"repo-page-heading",h2 {"Pages"} NavLink {href:format!("{}/settings",repo.base()),"Repository settings"}}
                p {class:"repo-archived-banner",role:"status","This server does not host Pages sites. Settings and attempts are recorded; builds and deployments fail because no publisher or OIDC verifier is configured."}
                if let Some(error)=&page.error {p {role:"alert","{error}"}}
                if repo.can_admin&&!repo.is_archived {
                    form {method:"post",action:repo.action("pages"),
                        {csrf(&repo.csrf_token)} {repository_id(repo.id)}
                        label {"Build type" DraftSelect {name:"build_type",initial_value:settings.build_type.clone(),options:vec![SelectOption::new("legacy","Branch"),SelectOption::new("workflow","Workflow artifact")]}}
                        label {"Source branch" DraftInput {name:"branch",initial_value:settings.branch.clone(),maxlength:Some(255)}}
                        label {"Source path" DraftSelect {name:"path",initial_value:settings.path.clone(),options:vec![SelectOption::new("/","/"),SelectOption::new("/docs","/docs")]}}
                        label {"Custom domain" DraftInput {name:"cname",initial_value:settings.cname.clone(),maxlength:Some(253)}}
                        label {DraftCheckbox {name:"https_enforced",initial_checked:settings.https_enforced} "Require HTTPS when publishing is available"}
                        label {DraftCheckbox {name:"public",initial_checked:settings.public} "Public site when publishing is available"}
                        button {r#type:"submit",name:"op",value:"save","Save Pages settings"}
                        if page.exists {button {r#type:"submit",name:"op",value:"delete","Delete Pages site"}}
                    }
                    if page.exists&&settings.build_type=="legacy" {
                        form {method:"post",action:repo.action("pages"),{csrf(&repo.csrf_token)} {repository_id(repo.id)} button {r#type:"submit",name:"op",value:"build","Request build"}}
                    }
                }
                h3 {"Build history"}
                if page.builds.is_empty() {p {"No builds have been requested."}}
                for build in &page.builds {article {class:"panel",style:"overflow-wrap:anywhere;min-width:0",p {code {"{build.commit}"} " · {build.status}"} p {"{build.error}"}}}
            }
        },
    )
}
