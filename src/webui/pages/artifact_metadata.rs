//! Read-only publisher inventory. Provenance association is not a signature-verification verdict.
use crate::webui::{
    components::{DraftInput, DraftSelect, SelectOption},
    navigation::NavLink,
};
use dioxus::prelude::*;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Entry {
    pub id: i64,
    pub repository: String,
    pub repository_href: String,
    pub fields: Vec<(String, String)>,
    pub tags: Vec<(String, String)>,
    pub omitted_tags: usize,
    pub risks: Vec<String>,
    pub bundle: Option<String>,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Page {
    pub organization: String,
    pub organization_href: String,
    pub base: String,
    pub digest: String,
    pub kind: String,
    pub entries: Vec<Entry>,
    pub next: Option<String>,
}

pub fn render(page: Page) -> Element {
    rsx! {
        main {class:"package-page",style:"overflow-wrap:anywhere;min-width:0",
            h1 {"Artifact inventory · {page.organization}"}
            p {NavLink {href:page.organization_href,"View organization"}}
            p {"Deployment and storage records are publisher-provided inventory, not proof of deployment or a signature-verification verdict. Only repositories you may currently read are included."}
            form {method:"get",action:page.base.clone(),class:"org-form",
                label {r#for:"artifact-metadata-digest","Artifact digest"}
                DraftInput {id:"artifact-metadata-digest",name:"digest",initial_value:page.digest.clone(),placeholder:"sha256:…"}
                label {r#for:"artifact-metadata-kind","Record type"}
                DraftSelect {id:"artifact-metadata-kind",name:"kind",initial_value:page.kind.clone(),options:vec![SelectOption::new("deployment","Deployments"),SelectOption::new("storage","Storage")]}
                button {r#type:"submit",class:"btn","Find artifact"}
            }
            if !page.digest.is_empty() {
                h2 {code {"{page.digest}"}}
                if page.entries.is_empty() {p {"No readable inventory records match this artifact."}}
                for entry in &page.entries {
                    section {key:"{entry.id}",class:"package-card",
                        h2 {"Inventory record #{entry.id}"}
                        p {NavLink {href:entry.repository_href.clone(),"{entry.repository}"}}
                        dl {for (name,value) in &entry.fields {
                            dt {key:"label-{name}","{name}"}
                            dd {"{value}"}
                        }}
                        if !entry.tags.is_empty() {
                            h3 {"Tags"}
                            dl {for (name,value) in &entry.tags {
                                dt {key:"tag-label-{name}","{name}"}
                                dd {"{value}"}
                            }}
                            p {"Tag values longer than 192 characters are abbreviated. Use the REST API for full values."}
                        }
                        if entry.omitted_tags>0 {p {"{entry.omitted_tags} additional tags are available through the REST API."}}
                        if !entry.risks.is_empty() {
                            h3 {"Publisher-reported runtime risks"}
                            ul {for risk in &entry.risks {li {key:"{risk}","{risk}"}}}
                        }
                        if let Some(bundle) = &entry.bundle {a {href:bundle.clone(),class:"btn","Download associated Sigstore bundle"}}
                    }
                }
                if let Some(next) = &page.next {NavLink {href:next.clone(),"Next inventory records"}}
            }
        }
    }
}
