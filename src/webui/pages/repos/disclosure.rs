//! Private reports: only their reporter and current repository administrators receive this model.
use super::chrome::{chrome, csrf, repository_id, RepoChrome};
use crate::webui::{
    components::{references::References, DraftInput, DraftTextarea, Markdown},
    navigation::NavLink,
};
use dioxus::prelude::*;

#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ReportRow {
    pub id: i64,
    pub title: String,
    pub reporter: String,
    pub date: String,
    pub state: String,
}
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Message {
    pub author: String,
    pub date: String,
    pub body: String,
}
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum Page {
    Index {
        enabled: bool,
        rows: Vec<ReportRow>,
        page: usize,
        more: bool,
        error: Option<String>,
        title: String,
        body: String,
    },
    Thread {
        id: i64,
        title: String,
        report: Message,
        state: String,
        replies: Vec<Message>,
        page: usize,
        more: bool,
        error: Option<String>,
        reply: String,
        references: Box<References>,
    },
}
fn pagination(base: &str, page: usize, more: bool) -> Element {
    rsx! {nav {class:"pagination",aria_label:"Security report pages",
        if page>1 {NavLink {href:format!("{base}?page={}",page-1),"Previous"}}
        if more {NavLink {href:format!("{base}?page={}",page+1),"Next"}}
    }}
}
fn message(message: &Message, references: &References) -> Element {
    rsx! {article {class:"panel security-message",header {strong {"{message.author}"} " · " time {"{message.date}"}} Markdown {content:message.body.clone(),references:references.clone()}}}
}
pub fn security(repo: &RepoChrome, page: &Page) -> Element {
    let item = match page {
        Page::Index { .. } => "index".into(),
        Page::Thread { id, .. } => id.to_string(),
    };
    let key = format!("{}:{}:{item}", repo.viewer_id, repo.id);
    rsx! {for key in [key] {SecurityView {key:"{key}",repo:repo.clone(),page:page.clone()}}}
}
#[component]
fn SecurityView(repo: RepoChrome, page: Page) -> Element {
    let repo = &repo;
    let page = &page;
    let base = format!("{}/security", repo.base());
    chrome(
        repo,
        rsx! {section {class:"security-page",
            header {class:"repo-page-heading",h2 {"Security"} p {"Private reports are visible only to their reporter and current repository administrators. They never appear in issues or public activity."}}
            NavLink {href:format!("{base}/advisories"),"Security advisories"}
            if repo.can_admin {NavLink {href:format!("{base}/dependabot"),"Dependabot alerts"}}
            NavLink {href:format!("{}/dependency-graph",repo.base()),"Dependency graph"}
            match page {
                Page::Index {enabled,rows,page,more,error,title,body}=>rsx! {
                    if repo.can_admin&&!repo.is_archived {
                        form {method:"post",action:repo.action("private-reporting"),{csrf(&repo.csrf_token)} {repository_id(repo.id)}
                            button {name:"enabled",value:(!*enabled).to_string(),if *enabled {"Disable private reporting"}else{"Enable private reporting"}}
                        }
                    }
                    if let Some(error)=error {p {class:"form-error",role:"alert","{error}"}}
                    if *enabled {
                        if repo.signed_in && !repo.is_archived {
                            section {class:"panel",h3 {"Report a vulnerability privately"}
                                form {method:"post",action:base.clone(),class:"repo-form-body",
                                    {csrf(&repo.csrf_token)} {repository_id(repo.id)}
                                    label {class:"repo-field",span {"Title"} DraftInput {name:"title",required:true,maxlength:Some(255),initial_value:title.clone()}}
                                    label {class:"repo-field",span {"Details, reproduction steps and impact"} DraftTextarea {name:"body",required:true,rows:12,maxlength:Some(65536),initial_value:body.clone()}}
                                    button {r#type:"submit",class:"btn btn-primary","Send private report"}
                                }
                            }
                        } else if !repo.signed_in {p {NavLink {href:"/login","Sign in"} " to report a vulnerability privately."}}
                    } else {p {"New private reports are not enabled for this repository. Existing reports remain private."}}
                    if repo.signed_in {
                        section {class:"panel",h3 {if repo.can_admin {"Private security reports"}else {"Your private security reports"}}
                            if rows.is_empty() {p {"No reports."}}
                            for row in rows {article {class:"repo-settings-row",NavLink {href:format!("{base}/reports/{}",row.id),"{row.title}"} span {"{row.state} · {row.reporter} · {row.date}"}}}
                        }
                        {pagination(&base,*page,*more)}
                    }
                },
                Page::Thread {id,title,report,state,replies,page,more,error,reply,references}=>{
                    let thread=format!("{base}/reports/{id}");
                    rsx! {
                        NavLink {href:base.clone(),"Back to security reports"}
                        h3 {"{title}"} p {"Private report #{id} · {state}"}
                        if let Some(error)=error {p {class:"form-error",role:"alert","{error}"}}
                        {message(report,references)}
                        for item in replies {{message(item,references)}}
                        {pagination(&thread,*page,*more)}
                        if !repo.is_archived {
                            form {method:"post",action:thread,class:"panel repo-form-body",
                                {csrf(&repo.csrf_token)} {repository_id(repo.id)} input {r#type:"hidden",name:"report_id",value:id.to_string()}
                                if state=="open" {
                                    label {class:"repo-field",span {"Private reply"} DraftTextarea {name:"body",rows:6,maxlength:Some(65536),initial_value:reply.clone()}}
                                    button {r#type:"submit",class:"btn btn-primary",name:"intent",value:"reply","Send reply"}
                                    button {r#type:"submit",class:"btn btn-secondary",name:"intent",value:"close","Close report"}
                                } else {
                                    if !reply.is_empty() {label {class:"repo-field",span {"Unsent reply (reopen before sending)"} DraftTextarea {name:"body",rows:6,maxlength:Some(65536),initial_value:reply.clone()}}}
                                    button {r#type:"submit",class:"btn btn-secondary",name:"intent",value:"reopen","Reopen report"}
                                }
                            }
                        }
                    }
                },
            }
        }},
    )
}
