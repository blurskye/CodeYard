use crate::webui::pages::repos::{
    chrome::{chrome, csrf, icon, repository_id, RepoChrome},
    commits::commit_target,
    ref_path::file_url,
};
use crate::webui::{components::DraftInput, navigation::NavLink};
use dioxus::prelude::*;

/// The confirm form that removes one file. The server fills it in, and it keeps the commit
/// fields as the user typed them when a delete is refused.
#[derive(Clone, Debug, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct DeleteFile {
    pub branch: String,
    pub path: String,
    pub expected_revision: String,
    pub message: String,
    pub error: Option<String>,
    #[serde(default)]
    pub saved: bool,
    /// The branch refuses direct commits to this user, so the commit goes to `new_branch`.
    pub protected: bool,
    pub commit_to_new_branch: bool,
    /// The name for the new branch, prefilled by the server and kept as the user typed it.
    pub new_branch: String,
}

/// The title of a page that shows a refused delete.
pub const DELETE_REFUSED: &str = "File not deleted";

pub fn delete_file(repo: &RepoChrome, file: &DeleteFile) -> Element {
    rsx! { DeleteFileView { repo: repo.clone(), file: file.clone() } }
}

// A component, so the form's inputs are built inside the renderer, as they need to be.
#[component]
fn DeleteFileView(repo: RepoChrome, file: DeleteFile) -> Element {
    let cancel_url = file_url(&repo.base(), "blob", &file.branch, &file.path);
    // A refused delete is shown at this POST-only URL; `from` lets a GET there find this page.
    let form_action = format!(
        "{}?from={}",
        repo.action("delete-file"),
        crate::webui::encode_component(&file_url(&repo.base(), "delete", &file.branch, &file.path))
    );
    chrome(
        &repo,
        rsx! {
            section { class: "repo-editor-page",
                header { class: "repo-page-heading",
                    div {
                        h2 { "Delete file" }
                        p { {icon("branches")} code { "{file.branch}" } " / " strong { "{file.path}" } }
                    }
                }
                if let Some(error) = &file.error { div { class: "repo-alert repo-alert-error", role: "alert", strong { if file.saved { "{super::SAVED_FOLLOWUP_FAILED}" } else { "{DELETE_REFUSED}" } } p { "{error}" } } }
                if !file.saved {
                form { method: "post", action: form_action, class: "repo-editor-form",
                    {csrf(&repo.csrf_token)}
                    {repository_id(repo.id)}
                    input { r#type: "hidden", name: "branch", value: file.branch.clone() }
                    input { r#type: "hidden", name: "path", value: file.path.clone() }
                    input { r#type: "hidden", name: "expected_revision", value: file.expected_revision.clone() }
                    section { class: "repo-commit-box panel",
                        header { class: "repo-section-heading", div { h3 { "Commit changes" } p { "Describe what changed so it is easy to understand in history." } } }
                        div { class: "repo-form-body",
                            {commit_target(&file.branch, &file.new_branch, file.protected, file.commit_to_new_branch)}
                            label { class: "repo-field", span { "Commit message" } DraftInput { name: "message", initial_value: file.message.clone(), maxlength: Some(4096), placeholder: format!("Delete {}", file.path) } }
                            div { class: "repo-form-actions",
                                button { class: "btn btn-danger", r#type: "submit", "Delete file" }
                                NavLink { class: "btn btn-secondary".to_owned(), href: cancel_url, "Cancel" }
                            }
                        }
                    }
                }
                }
            }
        },
    )
}
