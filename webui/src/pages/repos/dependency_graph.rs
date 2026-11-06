use super::chrome::{chrome, RepoChrome};
use dioxus::prelude::*;

#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Dependency {
    pub name: String,
    pub version: String,
    pub manifest: String,
    pub ecosystem: String,
    pub scope: String,
}

#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Page {
    pub sha: String,
    pub packages: Vec<Dependency>,
    #[serde(default)]
    pub previous: Option<String>,
    #[serde(default)]
    pub next: Option<String>,
    /// Public repositories that depend on the package this repository provides, when the
    /// deployment keeps a shared index; `None` hides the section.
    #[serde(default)]
    pub used_by: Option<UsedBy>,
}

#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct UsedBy {
    pub count: i64,
    /// `owner/name` of the most starred dependents.
    pub repositories: Vec<String>,
}

pub fn render(repo: &RepoChrome, page: &Page) -> Element {
    chrome(
        repo,
        rsx! {
            section {class:"repo-settings-page",
                header {class:"repo-page-heading", h2 {"Dependency graph"}
                    a {href:format!("{}/dependency-graph/sbom",repo.base()),download:"sbom.spdx.json","Export SBOM"}
                }
                p {"Pinned npm dependencies and submitted dependency snapshots at " code {style:"overflow-wrap:anywhere","{page.sha}"}}
                if page.packages.is_empty() {p {"No dependencies were found at this commit."}}
                for package in &page.packages {
                    article {class:"panel",style:"overflow-wrap:anywhere;min-width:0",h3 {"{package.name}"} p {code {"{package.version}"} " · {package.ecosystem} · {package.scope}"} p {"{package.manifest}"}}
                }
                if let Some(used_by) = &page.used_by {
                    section {aria_label:"Used by",
                        h3 {"Used by {used_by.count}"}
                        if used_by.repositories.is_empty() {p {"No public repository depends on this package yet."}}
                        ul {
                            for (owner, name) in used_by.repositories.iter().filter_map(|full_name| full_name.split_once('/')) {
                                li {style:"overflow-wrap:anywhere", a {href:crate::repository_path(owner, name), "{owner}/{name}"}}
                            }
                        }
                    }
                }
                nav {aria_label:"Dependency pages",
                    if let Some(previous) = &page.previous {a {href:previous.clone(), "Previous"}}
                    if let Some(next) = &page.next {a {href:next.clone(), "Next"}}
                }
            }
        },
    )
}
