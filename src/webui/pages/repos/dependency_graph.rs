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
            }
        },
    )
}
