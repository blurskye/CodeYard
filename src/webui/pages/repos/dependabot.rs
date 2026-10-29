use super::chrome::{chrome, csrf, repository_id, RepoChrome};
use crate::webui::{components::Markdown, navigation::NavLink};
use dioxus::prelude::*;

#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Alert {
    pub number: i64,
    pub summary: String,
    pub description: String,
    pub ghsa_id: String,
    pub package: String,
    pub version: String,
    pub manifest: String,
    pub severity: String,
    pub state: String,
    pub reason: Option<String>,
    pub comment: Option<String>,
    pub patched: Option<String>,
}
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Page {
    pub alerts: Vec<Alert>,
    pub state: String,
    pub page: usize,
    pub more: bool,
    pub can_import: bool,
}
pub fn render(repo: &RepoChrome, page: &Page) -> Element {
    let base = format!("{}/security/dependabot", repo.base());
    let body = rsx! {
        section {class:"repo-security",
            h1 {"Dependabot alerts"}
            NavLink {href:format!("{}/security",repo.base()),"Security overview"}
            p {"Alerts match the default branch’s latest lockfile and submitted dependency snapshots against advisories stored on this instance, including enabled OSV mirrors."}
            form {method:"get",action:base.clone(),class:"repo-security-filters",
                label {"State" select {name:"state",class:"form-select",value:page.state.clone(),
                    for state in ["open","dismissed","fixed","all"] {option {value:state,selected:page.state==state,"{state}"}}
                }}
                button {r#type:"submit",class:"btn btn-secondary","Filter"}
            }
            if page.alerts.is_empty() {p {"No Dependabot alerts match these filters."}}
            for alert in &page.alerts {
                article {class:"repo-security-alert",id:format!("alert-{}",alert.number),
                    h2 {"{alert.summary} #{alert.number}"}
                    p {span {class:"badge","{alert.state}"} " · {alert.severity} · {alert.ghsa_id}"}
                    p {"{alert.package} {alert.version} in {alert.manifest}"}
                    if let Some(patched)=&alert.patched {p {"First patched version: {patched}"}}
                    Markdown {content:alert.description.clone()}
                    if let Some(reason)=&alert.reason {p {"Dismissed: {reason}"}}
                    if let Some(comment)=&alert.comment {p {"{comment}"}}
                    if alert.state!="fixed" && !repo.is_archived {
                        form {method:"post",action:format!("{base}/{}/dismiss",alert.number),class:"repo-security-dismiss",
                            {csrf(&repo.csrf_token)}
                            {repository_id(repo.id)}
                            if alert.state=="dismissed" {
                                input {r#type:"hidden",name:"state",value:"open"}
                                button {r#type:"submit",class:"btn btn-secondary","Reopen alert"}
                            } else {
                                input {r#type:"hidden",name:"state",value:"dismissed"}
                                label {"Dismiss reason" select {name:"dismissed_reason",class:"form-select",
                                    for (value,label) in [("fix_started","Fix started"),("inaccurate","Inaccurate"),("no_bandwidth","No bandwidth"),("not_used","Not used"),("tolerable_risk","Tolerable risk")] {option {value:value,"{label}"}}
                                }}
                                label {"Comment" textarea {name:"dismissed_comment",maxlength:280,class:"form-control",rows:3}}
                                button {r#type:"submit",class:"btn btn-secondary","Dismiss alert"}
                            }
                        }
                    }
                }
            }
            if page.page>1 || page.more {nav {aria_label:"Dependabot pages",
                if page.page>1 {NavLink {href:format!("{base}?state={}&page={}",page.state,page.page-1),"Previous"}}
                if page.more {NavLink {href:format!("{base}?state={}&page={}",page.state,page.page+1),"Next"}}
            }}
            if page.can_import {details {
                summary {"Import a security advisory"}
                form {method:"post",action:format!("{base}/import"),class:"repo-security-dismiss",
                    {csrf(&repo.csrf_token)}
                    {repository_id(repo.id)}
                    label {"GitHub advisory JSON" textarea {name:"document",required:true,class:"form-control",rows:10}}
                    p {"Create an advisory by omitting ghsa_id, or replace an imported advisory by keeping its ghsa_id. This advisory becomes public across the instance."}
                    button {r#type:"submit",class:"btn btn-primary","Import advisory"}
                }
            }}
        }
    };
    chrome(repo, body)
}
