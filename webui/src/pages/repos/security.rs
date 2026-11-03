mod checks;
pub use checks::{annotation_block, check_list, Annotation, Check};

use super::chrome::{chrome, csrf, repository_id, RepoChrome};
use crate::{components::Markdown, navigation::NavLink};
use dioxus::prelude::*;
use std::collections::BTreeMap;

#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Alert {
    pub number: i64,
    pub rule: String,
    pub message: String,
    pub tool: String,
    pub severity: String,
    pub state: String,
    pub branch: String,
    pub path: String,
    pub line: i64,
    pub help: String,
    pub snippet: String,
    pub code_url: String,
}

#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Page {
    pub alerts: Vec<Alert>,
    pub selected: Option<Alert>,
    pub filters: BTreeMap<String, String>,
    #[serde(default)]
    pub page_number: usize,
    #[serde(default)]
    pub has_next: bool,
}

pub fn render(repo: &RepoChrome, page: Page) -> Element {
    let base = format!("{}/security/code-scanning", repo.base());
    let page_url = |number: usize| {
        let mut filters = page.filters.clone();
        filters.insert("page".into(), number.to_string());
        format!(
            "{base}?{}",
            url::form_urlencoded::Serializer::new(String::new())
                .extend_pairs(filters)
                .finish()
        )
    };
    let listing_url = page
        .selected
        .as_ref()
        .map(|alert| format!("{base}?branch={}", crate::encode_component(&alert.branch)))
        .unwrap_or_else(|| base.clone());
    let body = rsx! {
        section { class: "repo-security",
            h1 { "Security" }
            NavLink { href:listing_url, "Code scanning" }
            if let Some(alert)=&page.selected {
                h2 { "{alert.rule} #{alert.number}" }
                p { span { class:"badge", "{alert.state}" } " · {alert.severity} · {alert.tool}" }
                p { "{alert.message}" }
                p { if !alert.code_url.is_empty() {NavLink { href:alert.code_url.clone(), "{alert.path}:{alert.line}" }} else {"{alert.path}:{alert.line}"} " on {alert.branch}" }
                if !alert.snippet.is_empty() { pre { class:"repo-security-snippet", code { "{alert.snippet}" } } } else {p { "A source snippet is unavailable for this artifact at the scanned revision." }}
                Markdown { content:alert.help.clone() }
                if !repo.is_archived {
                    form { method:"post", action:format!("{base}/{}/dismiss",alert.number), class:"repo-security-dismiss",
                        {csrf(&repo.csrf_token)}
                        {repository_id(repo.id)}
                        input { r#type:"hidden", name:"branch", value:alert.branch.clone() }
                        if alert.state=="dismissed" {
                            input { r#type:"hidden", name:"state", value:"open" }
                            button { r#type:"submit", class:"btn btn-secondary", "Reopen alert" }
                        } else {
                            input { r#type:"hidden", name:"state", value:"dismissed" }
                            label { "Dismiss reason"
                                select { name:"reason", class:"form-select",
                                    option { value:"false positive", "False positive" }
                                    option { value:"won't fix", "Won't fix" }
                                    option { value:"used in tests", "Used in tests" }
                                }
                            }
                            label { "Comment" textarea { name:"comment", maxlength:280, class:"form-control", rows:3 } }
                            button { r#type:"submit", class:"btn btn-secondary", "Dismiss alert" }
                        }
                    }
                }
            } else {
                h2 { "Code scanning alerts" }
                form { method:"get", action:base.clone(), class:"repo-security-filters",
                    for (key,label) in [("tool","Tool"),("branch","Branch"),("severity","Severity"),("state","State")] {
                        label { "{label}" input { name:key, value:page.filters.get(key).cloned().unwrap_or_default(), class:"form-control" } }
                    }
                    button { r#type:"submit", class:"btn btn-secondary", "Filter" }
                }
                if page.alerts.is_empty() { p { "No code scanning alerts match these filters. Upload SARIF from an Actions workflow to scan this repository." } }
                for alert in &page.alerts {
                    article { class:"repo-security-alert",
                        h3 { NavLink { href:format!("{base}/{}?branch={}",alert.number,crate::encode_component(&alert.branch)), "{alert.rule} #{alert.number}" } }
                        p { "{alert.message}" }
                        small { "{alert.severity} · {alert.state} · {alert.tool} · {alert.path}:{alert.line} · {alert.branch}" }
                    }
                }
                if page.page_number>1 || page.has_next {
                    nav { aria_label:"Code scanning pages",
                        if page.page_number>1 {NavLink { href:page_url(page.page_number-1), "Previous" }}
                        if page.has_next {NavLink { href:page_url(page.page_number+1), "Next" }}
                    }
                }
            }
        }
    };
    chrome(repo, body)
}
