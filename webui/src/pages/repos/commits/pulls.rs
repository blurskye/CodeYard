use crate::{navigation::NavLink, pages::repos::chrome::RepoChrome};
use dioxus::prelude::*;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RelatedPull {
    pub number: i32,
    pub title: String,
    pub state: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RelatedPulls {
    pub rows: Vec<RelatedPull>,
    pub previous: Option<String>,
    pub next: Option<String>,
    pub error: Option<String>,
}

impl Default for RelatedPulls {
    fn default() -> Self {
        Self {
            rows: Vec::new(),
            previous: None,
            next: None,
            error: Some("Pull request associations have not been loaded.".into()),
        }
    }
}

pub fn related_pulls(repo: &RepoChrome, pulls: &RelatedPulls) -> Element {
    let base = repo.base();
    rsx! {
        section { id: "related-pull-requests", class: "panel", "aria-label": "Associated pull requests",
            h3 { "Associated pull requests" }
            if let Some(error) = &pulls.error {
                p { role: "status", "{error}" }
            } else if pulls.rows.is_empty() {
                p { "No associated pull requests." }
            } else {
                ul {
                    for pull in &pulls.rows {
                        li { key: "{pull.number}",
                            NavLink { href: format!("{base}/pulls/{}",pull.number), "#{pull.number} {pull.title}" }
                            " " span { class: "badge", "{pull.state}" }
                        }
                    }
                }
            }
            if pulls.previous.is_some() || pulls.next.is_some() {
                nav { "aria-label": "Associated pull request pages", class: "pagination",
                    if let Some(previous) = &pulls.previous { NavLink { href: previous.clone(), "Previous pull requests" } }
                    if let Some(next) = &pulls.next { NavLink { href: next.clone(), "Next pull requests" } }
                }
            }
        }
    }
}
