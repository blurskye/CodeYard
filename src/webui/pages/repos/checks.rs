//! Published check output is readable before hydration, including every annotation page.
use super::chrome::{chrome, csrf, RepoChrome};
use crate::webui::{components::Markdown, navigation::NavLink};
use dioxus::prelude::*;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Check {
    pub id: i64,
    pub name: String,
    pub sha: String,
    pub status: String,
    pub conclusion: Option<String>,
    pub title: Option<String>,
    pub summary: Option<String>,
    pub text: Option<String>,
    pub details_url: Option<String>,
    pub annotations: Vec<Annotation>,
    pub total: i64,
    pub page: usize,
    pub has_more: bool,
    pub can_rerequest: bool,
    #[serde(default)]
    pub images: Vec<(String, String, Option<String>)>,
    #[serde(default)]
    pub actions: Vec<(String, String, String)>,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Annotation {
    pub path: String,
    pub location: String,
    pub level: String,
    pub title: Option<String>,
    pub message: String,
    pub details: Option<String>,
}

pub fn render(repo: &RepoChrome, check: &Check) -> Element {
    let base = format!("{}/runs/{}", repo.base(), check.id);
    chrome(
        repo,
        rsx! {
            section { class:"collaboration-detail-main check-run",
                header { class:"collaboration-panel-header",
                    h2 { "{check.name}" }
                    p { "{check.status}" if let Some(conclusion) = &check.conclusion { " · {conclusion}" } }
                    NavLink { href:format!("{}/commit/{}",repo.base(),check.sha), "Commit {check.sha}" }
                    if check.can_rerequest {
                        form { method:"post", action:format!("{base}/rerequest"),
                            {csrf(&repo.csrf_token)}
                            input { r#type:"hidden",name:"repository_id",value:repo.id.to_string() }
                            button { r#type:"submit", class:"btn btn-secondary", "Re-run suite" }
                        }
                    }
                    if check.can_rerequest {
                        for (label, description, identifier) in &check.actions {
                            form { method: "post", action: format!("{base}/action"),
                                {csrf(&repo.csrf_token)}
                                input { r#type: "hidden", name: "repository_id", value: "{repo.id}" }
                                input { r#type: "hidden", name: "identifier", value: identifier.clone() }
                                button { r#type: "submit", class: "btn btn-secondary", title: description.clone(), "{label}" }
                            }
                        }
                    }
                    if let Some(url) = &check.details_url { if !url.is_empty() { a { href:url.clone(),rel:"noopener noreferrer", "View details" } } }
                }
                article { class:"collaboration-panel collaboration-markdown",
                    if let Some(title) = &check.title { h3 { "{title}" } }
                    if let Some(summary) = &check.summary { Markdown { content:summary.clone() } }
                    if let Some(text) = &check.text { Markdown { content:text.clone() } }
                    for (alt, url, caption) in &check.images {
                        figure { img { src: url.clone(), alt: alt.clone(), loading: "lazy", style: "max-width:100%" }
                            if let Some(caption) = caption { figcaption { "{caption}" } }
                        }
                    }
                }
                h3 { "Annotations ({check.total})" }
                for (index,annotation) in check.annotations.iter().enumerate() {
                    article { key:"{index}",class:"collaboration-panel",
                        header { class:"collaboration-panel-header", "{annotation.level}: {annotation.path} · {annotation.location}" }
                        if let Some(title) = &annotation.title { h4 { "{title}" } }
                        p { class:"check-run-message", "{annotation.message}" }
                        if let Some(details) = &annotation.details { details { summary { "Raw details" } pre { "{details}" } } }
                    }
                }
                nav { class:"collaboration-pagination", aria_label:"Annotations pagination",
                    if check.page > 1 { NavLink { href:format!("{base}?page={}",check.page-1), "Previous" } }
                    if check.has_more { NavLink { href:format!("{base}?page={}",check.page+1), "Next" } }
                }
            }
        },
    )
}
