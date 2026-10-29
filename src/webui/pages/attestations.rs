//! Stored provenance is not a verification verdict; the same owned model renders and hydrates.
use crate::webui::{components::DraftInput, navigation::NavLink, pages::repos::chrome::RepoChrome};
use dioxus::prelude::*;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Entry {
    pub id: i64,
    pub repository: String,
    pub repository_href: String,
    pub bundle_href: String,
    pub predicate: String,
    pub subjects: Vec<String>,
    pub delete_action: Option<String>,
    pub inventory_base: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Page {
    pub title: String,
    pub base: String,
    pub repo: Option<Box<RepoChrome>>,
    pub digest: String,
    pub predicate: String,
    pub entries: Vec<Entry>,
    pub next: Option<String>,
    pub csrf_token: String,
    pub message: Option<String>,
}

pub fn render(page: Page) -> Element {
    rsx! {
        main { class: "package-page", style: "overflow-wrap:anywhere;min-width:0",
            h1 { "Attestations · {page.title}" }
            p { "Stored Sigstore bundles establish provenance only after their signatures, timestamps and signer identity have been verified. Storage does not mean verification." }
            if let Some(message) = &page.message { p { role: "alert", "{message}" } }
            form { method: "get", action: page.base.clone(), class: "org-form",
                label { r#for: "attestation-digest", "Subject digest" }
                DraftInput { id: "attestation-digest", name: "digest", initial_value: page.digest.clone(), placeholder: "sha256:…" }
                label { r#for: "attestation-predicate", "Predicate type" }
                DraftInput { id: "attestation-predicate", name: "predicate", initial_value: page.predicate.clone(), placeholder: "provenance, sbom, release or a predicate URI" }
                button { class: "btn", r#type: "submit", "Filter attestations" }
            }
            p { "Verify downloaded bundles locally using gh attestation verify ARTIFACT --bundle BUNDLE.json --repo EXPECTED_OWNER/EXPECTED_REPO with the appropriate trusted root and signer policy." }
            if page.entries.is_empty() { p { "No attestations match this view." } }
            for entry in &page.entries {
                section { key: "{entry.id}", class: "package-card",
                    h2 { "Attestation #{entry.id}" }
                    p { NavLink { href: entry.repository_href.clone(), "{entry.repository}" } }
                    p { "{entry.predicate}" }
                    ul { for digest in &entry.subjects { li { key: "{digest}",
                        code { "{digest}" }
                        // The inventory is keyed by SHA-256 digests only.
                        if let Some(base) = entry.inventory_base.as_ref().filter(|_| digest.starts_with("sha256:")) {
                            NavLink { href: format!("{base}/{digest}"), "View inventory" }
                        }
                    } } }
                    a { href: entry.bundle_href.clone(), class: "btn", "Download Sigstore bundle" }
                    if let Some(action) = &entry.delete_action {
                        form { method: "post", action: action.clone(),
                            input { r#type: "hidden", name: "csrf_token", value: page.csrf_token.clone() }
                            input { r#type: "hidden", name: "attestation_id", value: "{entry.id}" }
                            button { r#type: "submit", class: "btn btn-destructive", "Delete attestation" }
                        }
                    }
                }
            }
            if let Some(next) = &page.next { NavLink { href: next.clone(), "Next attestations" } }
        }
    }
}
