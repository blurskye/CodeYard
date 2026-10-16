use super::{
    chrome::{chrome, csrf, icon},
    ref_path::{compare_url, file_url},
    RepoChrome,
};
use crate::webui::{
    components::{count_label, DraftInput, DraftSelect, DraftTextarea, SelectOption},
    navigation::NavLink,
};
use dioxus::prelude::*;

#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct RefRow {
    pub name: String,
    pub sha: String,
    pub message: Option<String>,
    pub is_default: bool,
}

pub fn refs(repo: &RepoChrome, rows: &[RefRow], tags: bool, branches: &[RefRow]) -> Element {
    let base = repo.base();
    let kind = if tags { "tag" } else { "branch" };
    let plural = if tags { "tags" } else { "branches" };
    let title = if tags { "Tags" } else { "Branches" };
    // Each other branch links to its comparison with the default branch.
    let default_branch = branches
        .iter()
        .find(|branch| branch.is_default)
        .map(|branch| branch.name.clone())
        .filter(|_| !tags);
    chrome(
        repo,
        rsx! {
            section { class: "repo-refs-page",
                header { class: "repo-page-heading",
                    div {
                        div { class: "title-with-count", h2 { "{title}" } span { class: "count-badge", {count_label(rows.len(), kind, plural)} } }
                        p { if tags { "Mark important points in this repository's history." } else { "View and manage lines of development." } }
                    }
                }
                div { class: if repo.can_write { "repo-refs-layout" } else { "repo-refs-layout repo-refs-layout-full" },
                    section { class: "repo-ref-list-panel panel", aria_label: title,
                        header { class: "repo-list-header",
                            strong { "{title}" }
                        }
                        if rows.is_empty() {
                            div { class: "repo-empty",
                                {icon(if tags { "tags" } else { "branches" })}
                                h3 { "No {title.to_lowercase()} yet" }
                                p { if tags { "Create a tag to mark a release or milestone." } else { "Create a branch from an existing commit." } }
                            }
                        }
                        for row in rows {
                            article { key: "{row.name}", class: "repo-ref-row",
                                div { class: "repo-ref-row-icon", {icon(if tags { "tags" } else { "branches" })} }
                                div { class: "repo-ref-row-content",
                                    div { class: "repo-ref-row-title",
                                        NavLink { href: file_url(&base, "tree", &row.name, ""), "{row.name}" }
                                        if row.is_default { span { class: "repo-label-neutral", "Default" } }
                                    }
                                    if let Some(message) = &row.message { p { "{message}" } }
                                    if !row.sha.is_empty() {
                                        p { class: "repo-ref-meta", "Updated at " NavLink { class: "repo-short-sha".to_owned(), href: format!("{base}/commit/{}", crate::webui::encode_component(&row.sha)), "{row.sha.get(..7).unwrap_or(&row.sha)}" } }
                                    }
                                }
                                div { class: "repo-ref-actions",
                                    if let Some(default) = default_branch.as_deref().filter(|_| !row.is_default) {
                                        NavLink { class: "btn btn-secondary btn-sm", href: compare_url(&base, default, &row.name), "Compare" }
                                    }
                                    if repo.can_write && !row.is_default {
                                        details { class: "repo-delete-menu",
                                            summary { class: "btn btn-danger btn-sm", "Delete" }
                                            form { method: "post", action: repo.action(&format!("delete-{kind}")), class: "repo-confirm-popover",
                                                {csrf(&repo.csrf_token)}
                                                input { r#type: "hidden", name: "name", value: row.name.clone() }
                                                strong { "Delete {kind}?" }
                                                p { "This cannot be undone." }
                                                label { class: "repo-check-label", input { r#type: "checkbox", required: true, name: "confirm", value: "yes" } " I understand" }
                                                button { class: "btn btn-danger", r#type: "submit", "Delete {kind}" }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                    if repo.can_write {
                        aside { class: "repo-ref-create", aria_label: format!("Create {kind}"),
                            CreateRefForm { key: "{base}:{kind}", repo: repo.clone(), tags, branches: branches.to_vec() }
                        }
                    }
                }
            }
        },
    )
}

#[component]
fn CreateRefForm(repo: RepoChrome, tags: bool, branches: Vec<RefRow>) -> Element {
    let kind = if tags { "tag" } else { "branch" };
    // The form posts the full commit SHA of the chosen branch. The default branch comes first
    // and starts selected.
    let options: Vec<SelectOption> = branches
        .iter()
        .map(|branch| SelectOption {
            value: branch.sha.clone(),
            label: format!(
                "{} ({})",
                branch.name,
                branch.sha.get(..7).unwrap_or(&branch.sha)
            ),
        })
        .collect();
    let initial = options
        .first()
        .map(|option| option.value.clone())
        .unwrap_or_default();
    rsx! {
        form { method: "post", action: repo.action(&format!("create-{kind}")), class: "repo-settings-section panel",
            {csrf(&repo.csrf_token)}
            header { class: "repo-section-heading",
                div { h3 { "Create a {kind}" } p { "Start from an existing branch." } }
            }
            div { class: "repo-form-body",
                label { class: "repo-field", span { "Name" } DraftInput { name: "name", required: true, maxlength: Some(255), initial_value: String::new(), placeholder: format!("New {kind} name") } }
                label { class: "repo-field", span { "Start from branch" } DraftSelect { name: "sha", required: true, initial_value: initial, options } }
                if tags { label { class: "repo-field", span { "Message " small { "Optional" } } DraftTextarea { name: "message", rows: 3, initial_value: String::new() } } }
                button { class: "btn btn-primary", r#type: "submit", "Create {kind}" }
            }
        }
    }
}
