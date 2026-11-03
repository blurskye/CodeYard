use crate::{navigation::NavLink, pages::repos::chrome::RepoChrome};
use dioxus::prelude::*;
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Row {
    pub reference: String,
    pub kind: String,
    pub actor: Option<String>,
    pub date: String,
    pub commit: String,
}
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Page {
    pub rows: Vec<Row>,
    pub previous: Option<String>,
    pub next: Option<String>,
    pub reference: String,
    pub actor: String,
    pub kind: String,
    pub direction: String,
    pub time_period: String,
}
pub fn render(repo: &RepoChrome, path: &str, page: &Page) -> Element {
    rsx! {section {class:"panel p-4",h2 {"Repository activity"}
        p {"Accepted branch changes from the editor, pull requests and Git pushes. Older changes are not reconstructed from commit authorship."}
        form {method:"get",action:path,class:"repo-form-body",
            label {class:"repo-field",span {"Ref"} input {name:"ref",value:page.reference.clone(),placeholder:"refs/heads/main"}}
            label {class:"repo-field",span {"Actor"} input {name:"actor",value:page.actor.clone()}}
            label {class:"repo-field",span {"Activity type"} select {name:"activity_type",
                option {value:"",selected:page.kind.is_empty(),"All activity"}
                for(kind,label)in [("push","Push"),("force_push","Force push"),("branch_creation","Branch creation"),("branch_deletion","Branch deletion"),("pr_merge","Pull request merge")] {option {value:kind,selected:page.kind==kind,"{label}"}}
            }}
            label {class:"repo-field",span {"Order"} select {name:"direction",option {value:"desc",selected:page.direction!="asc","Newest first"} option {value:"asc",selected:page.direction=="asc","Oldest first"}}}
            label {class:"repo-field",span {"Time period"} select {name:"time_period",
                option {value:"",selected:page.time_period.is_empty(),"All recorded activity"}
                for(value,label)in [("day","Day"),("week","Week"),("month","Month"),("quarter","Quarter"),("year","Year")] {option {value:value,selected:page.time_period==value,"{label}"}}
            }}
            button {r#type:"submit",class:"btn btn-secondary","Filter activity"}
        }
        if page.rows.is_empty() {p {"No accepted branch changes match these filters."}}
        for row in &page.rows {article {class:"repo-settings-row",
            code {"{row.reference}"} span {"{row.kind}"}
            if let Some(actor)=&row.actor {NavLink {href:crate::profile_path(actor),"{actor}"}} else {span {"Deleted account"}}
            time {"{row.date}"}
            NavLink {href:format!("{}/commit/{}",repo.base(),row.commit),"{row.commit}"}
        }}
        nav {class:"pagination",aria_label:"Activity pages",
            if let Some(href)=&page.previous {NavLink {href:href.clone(),"Previous"}}
            if let Some(href)=&page.next {NavLink {href:href.clone(),"Next"}}
        }
    }}
}
