use super::chrome::{chrome, csrf, repository_id, RepoChrome};
use crate::{
    components::{DraftInput, DraftSelect, DraftTextarea, Markdown, SelectOption},
    navigation::NavLink,
};
use dioxus::prelude::*;
#[derive(Clone, Debug, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Draft {
    pub summary: String,
    pub description: String,
    pub severity: String,
    pub vector: String,
    pub cve: String,
    pub package: String,
    pub ecosystem: String,
    pub range: String,
    #[serde(default)]
    pub vulnerabilities_json: String,
}
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Row {
    pub ghsa: String,
    pub summary: String,
    pub state: String,
    pub severity: String,
}
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Vulnerability {
    pub ecosystem: String,
    pub name: String,
    pub range: String,
    pub patched: String,
}
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Detail {
    pub row: Row,
    pub description: String,
    pub vulnerabilities: Vec<Vulnerability>,
    pub cve: Option<String>,
    pub cve_requested: bool,
    pub fork_url: Option<String>,
    pub report_url: Option<String>,
}
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Page {
    #[serde(default)]
    pub can_administer: bool,
    pub rows: Vec<Row>,
    pub detail: Option<Detail>,
    pub draft: Draft,
    pub error: Option<String>,
    pub more: bool,
    pub page: usize,
}
fn fields(draft: &Draft, include_package: bool) -> Element {
    let severity_options = std::iter::once(SelectOption::new("", "Unspecified"))
        .chain(
            ["low", "medium", "high", "critical"]
                .into_iter()
                .map(|level| SelectOption::new(level, level)),
        )
        .collect::<Vec<_>>();
    let ecosystem_options = [
        "npm", "pip", "rust", "go", "maven", "nuget", "composer", "rubygems", "actions", "other",
    ]
    .into_iter()
    .map(|ecosystem| SelectOption::new(ecosystem, ecosystem))
    .collect::<Vec<_>>();
    rsx! {
        label {"Summary" DraftInput {name:"summary",initial_value:draft.summary.clone(),required:true,maxlength:Some(255)}}
        label {"Description" DraftTextarea {name:"description",initial_value:draft.description.clone(),required:true,maxlength:Some(65536)}}
        label {"Severity" DraftSelect {name:"severity",initial_value:draft.severity.clone(),options:severity_options}}
        label {"CVSS 3 base vector (instead of severity)" DraftInput {name:"vector",initial_value:draft.vector.clone(),maxlength:Some(256)}}
        label {"Assigned CVE (leave blank until assigned)" DraftInput {name:"cve",initial_value:draft.cve.clone(),maxlength:Some(32)}}
        if include_package {
            label {"Package ecosystem" DraftSelect {name:"ecosystem",initial_value:draft.ecosystem.clone(),options:ecosystem_options}}
            label {"Package name" DraftInput {name:"package",initial_value:draft.package.clone(),maxlength:Some(255)}}
            label {"Vulnerable versions" DraftInput {name:"range",initial_value:draft.range.clone(),maxlength:Some(1024)}}
        } else {
            label {"Affected packages (JSON)" DraftTextarea {name:"vulnerabilities_json",initial_value:draft.vulnerabilities_json.clone(),required:true,rows:10}}
            p {"Complete each package's ecosystem, name and vulnerable_version_range before publishing. Keep patched_versions and vulnerable_functions when editing existing entries."}
        }
    }
}
pub fn render(repo: &RepoChrome, page: &Page) -> Element {
    let item = page
        .detail
        .as_ref()
        .map(|detail| detail.row.ghsa.as_str())
        .unwrap_or("new");
    let key = format!("{}:{}:{item}", repo.viewer_id, repo.id);
    rsx! {for key in [key] {AdvisoryView {key:"{key}",repo:repo.clone(),page:page.clone()}}}
}
#[component]
fn AdvisoryView(repo: RepoChrome, page: Page) -> Element {
    let repo = &repo;
    let page = &page;
    let base = format!("{}/security/advisories", repo.base());
    chrome(
        repo,
        rsx! {
            section {class:"repo-settings-page",style:"min-width:0;overflow-wrap:anywhere",
                header {class:"repo-page-heading",h2 {"Security advisories"} NavLink {href:format!("{}/security",repo.base()),"Private vulnerability reports"} NavLink {href:format!("{}/dependency-graph",repo.base()),"Dependency graph"}}
                if let Some(error)=&page.error {p {role:"alert","{error}"}}
                if let Some(detail)=&page.detail {
                    h3 {"{detail.row.summary}"} p {code {"{detail.row.ghsa}"} " · {detail.row.state} · {detail.row.severity}"}
                    Markdown {content:detail.description.clone()}
                    if let Some(cve)=&detail.cve {p {"CVE: {cve}"}}
                    if detail.cve_requested {p {role:"status","CVE request recorded for administrator follow-up. No CVE registrar is connected; an identifier has not been issued by this server."}}
                    for vulnerability in &detail.vulnerabilities {article {class:"panel",h4 {"{vulnerability.ecosystem}: {vulnerability.name}"} p {"Affected: {vulnerability.range}"} p {"Patched: {vulnerability.patched}"}}}
                    if let Some(url)=&detail.report_url {NavLink {href:url.clone(),"Original confidential report"}}
                    if let Some(url)=&detail.fork_url {NavLink {href:url.clone(),"Private remediation fork"}}
                    if page.can_administer&&!repo.is_archived {
                        form {method:"post",action:repo.action("advisory"),{csrf(&repo.csrf_token)} {repository_id(repo.id)} input {r#type:"hidden",name:"ghsa",value:detail.row.ghsa.clone()}
                            // An advisory with no packages yet takes one through plain fields; once it has
                            // some, the JSON list edits all of them without dropping any.
                            {fields(&page.draft,detail.vulnerabilities.is_empty())}
                            button {r#type:"submit",name:"op",value:"save","Save advisory"}
                        }
                        form {method:"post",action:repo.action("advisory"),{csrf(&repo.csrf_token)} {repository_id(repo.id)} input {r#type:"hidden",name:"ghsa",value:detail.row.ghsa.clone()}
                            if detail.row.state=="triage"||detail.row.state=="closed" {button {name:"op",value:"draft","Accept as draft"}}
                            if detail.row.state=="draft" {button {name:"op",value:"publish","Publish advisory"} if detail.cve.is_none()&&!detail.cve_requested {button {name:"op",value:"cve","Record CVE request"}} if detail.fork_url.is_none() {button {name:"op",value:"fork","Create private remediation fork"}}}
                            if detail.row.state!="published"&&detail.row.state!="closed" {button {name:"op",value:"close","Close advisory"}}
                        }
                    }
                }else{
                    if page.rows.is_empty() {p {"No advisories are visible to this account."}}
                    for row in &page.rows {article {class:"panel",h3 {NavLink {href:format!("{base}/{}",row.ghsa),"{row.summary}"}} p {code {"{row.ghsa}"} " · {row.state} · {row.severity}"}}}
                    nav {aria_label:"Advisory pages",if page.page>1 {NavLink {href:format!("{base}?page={}",page.page-1),"Previous"}} if page.more {NavLink {href:format!("{base}?page={}",page.page+1),"Next"}}}
                    if page.can_administer&&!repo.is_archived {h3 {"Create draft advisory"} form {method:"post",action:repo.action("advisory"),{csrf(&repo.csrf_token)} {repository_id(repo.id)} {fields(&page.draft,true)} button {name:"op",value:"create","Create confidential draft"}}}
                }
            }
        },
    )
}
