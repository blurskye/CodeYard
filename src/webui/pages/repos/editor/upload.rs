//! The upload page: pick files, name the commit, and add the files to the branch in one commit.
//! The file input is a native control, so the form works without JavaScript and accepts a drop.
use crate::webui::pages::repos::{
    chrome::{chrome, csrf, icon, repository_id, RepoChrome},
    commits::commit_target,
    ref_path::file_url,
    tree::breadcrumbs,
};
use crate::webui::{
    components::{count_label, format_bytes, DraftInput, DraftTextarea},
    navigation::NavLink,
};
use dioxus::prelude::*;

/// The commit message used when the form's message is empty.
pub const UPLOAD_MESSAGE: &str = "Add files via upload";
const UPLOAD_INPUT_ID: &str = "repository-upload-files";

#[derive(Clone, Debug, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct UploadForm {
    pub repository_id: i64,
    pub commit_to_new_branch: bool,
    pub branch: String,
    /// The directory the files land in; empty for the root.
    pub directory: String,
    /// The branch's tip when the page was opened. A commit on a different tip is refused.
    pub expected_revision: String,
    pub message: String,
    pub description: String,
    pub error: Option<String>,
    /// The files were committed and only the follow-up work failed.
    #[serde(default)]
    pub saved: bool,
    /// The branch refuses direct commits to this user, so the commit goes to `new_branch`.
    pub protected: bool,
    /// The name for the new branch, prefilled by the server and kept as the user typed it.
    pub new_branch: String,
}

pub fn upload_page(repo: &RepoChrome, form: &UploadForm) -> Element {
    let base = repo.base();
    let form_action = file_url(&base, "upload", &form.branch, &form.directory);
    let cancel_url = file_url(&base, "tree", &form.branch, &form.directory);
    let heading = if form.directory.is_empty() {
        format!("Upload files to the root of {}", form.branch)
    } else {
        format!("Upload files to {} on {}", form.directory, form.branch)
    };
    chrome(
        repo,
        rsx! {
            section { class: "repo-upload-page",
                header { class: "repo-page-heading",
                    div {
                        h2 { "{heading}" }
                        p { {icon("branches")} code { "{form.branch}" } }
                    }
                }
                // The root has no path to show, so the repository name would only repeat the page header.
                if !form.directory.is_empty() { {breadcrumbs(repo, &form.branch, &form.directory, false)} }
                if let Some(error) = &form.error { div { class: "repo-alert repo-alert-error", role: "alert", strong { if form.saved { "{super::SAVED_FOLLOWUP_FAILED}" } else { "Files were not uploaded" } } p { "{error}" } } }
                if !form.saved {
                form { method: "post", action: form_action, class: "repo-upload-form", "enctype": "multipart/form-data",
                    {csrf(&repo.csrf_token)}
                    {repository_id(form.repository_id)}
                    input { r#type: "hidden", name: "expected_revision", value: form.expected_revision.clone() }
                    section { class: "repo-upload-panel panel",
                        header { class: "repo-section-heading", div { h3 { "Files" } p { "Up to 100 files become one commit on the branch." } } }
                        div { class: "repo-upload-body",
                            label { class: "sr-only", r#for: UPLOAD_INPUT_ID, "Files to upload" }
                            UploadFileInput {}
                        }
                    }
                    section { class: "repo-commit-box panel",
                        header { class: "repo-section-heading", div { h3 { "Commit changes" } p { "The files are added to the branch in one commit." } } }
                        div { class: "repo-form-body",
                            {commit_target(&form.branch, &form.new_branch, form.protected, form.commit_to_new_branch)}
                            label { class: "repo-field", span { "Commit message" } DraftInput { name: "message", initial_value: form.message.clone(), maxlength: Some(4096), placeholder: UPLOAD_MESSAGE } }
                            label { class: "repo-field", span { "Extended description" } DraftTextarea { name: "description", initial_value: form.description.clone(), rows: 4, maxlength: Some(4096) } }
                            div { class: "repo-form-actions",
                                button { class: "btn btn-primary", r#type: "submit", "Commit changes" }
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

/// One file the input holds: its name and size in bytes.
#[derive(Clone, Debug, PartialEq)]
pub struct PickedFile {
    pub name: String,
    pub size: i64,
}

/// The heading of the list: "3 files, 12.0 KiB".
pub fn selection_summary(files: &[PickedFile]) -> String {
    let total: i64 = files.iter().map(|file| file.size).sum();
    format!(
        "{}, {}",
        count_label(files.len(), "file", "files"),
        format_bytes(total)
    )
}

/// The positions that stay when the file at `removed` leaves a selection of `count` files.
pub fn kept_indexes(count: usize, removed: usize) -> Vec<usize> {
    (0..count).filter(|index| *index != removed).collect()
}

/// The drop zone and the list of the files it holds. The input stays the source of truth: the list
/// is read from its FileList, and removing a file rebuilds that FileList, so the form posts exactly
/// the files that are listed. Until the page hydrates the list is empty and the native input works
/// alone.
#[component]
fn UploadFileInput() -> Element {
    let mut picked = use_signal(Vec::<PickedFile>::new);
    // Files picked before the script ran are already in the input.
    use_effect(move || picked.set(browser::picked_files()));
    let files = picked();
    let count = files.len();
    rsx! {
        div { class: "repo-upload-zone",
            input { id: UPLOAD_INPUT_ID, r#type: "file", name: "files", "multiple": true, "required": true,
                onchange: move |_| picked.set(browser::picked_files()),
            }
            span { class: "repo-upload-hint", "Drag files here or choose files" }
        }
        p { class: "repo-upload-limit", "Each file can be 25 MiB, and an upload 100 MiB in total." }
        if !files.is_empty() {
            div { class: "repo-upload-selection",
                div { class: "repo-upload-selection-head",
                    p { class: "repo-upload-summary", {selection_summary(&files)} }
                    button { r#type: "button", class: "btn",
                        onclick: move |_| {
                            browser::keep_files(&[]);
                            picked.set(browser::picked_files());
                        },
                        "Clear all" }
                }
                ul { class: "app-list repo-upload-files",
                    for (index, file) in files.iter().enumerate() {
                        li { class: "app-list-row repo-upload-file",
                            span { class: "repo-upload-file-name", title: file.name.clone(), "{file.name}" }
                            span { class: "repo-upload-file-size", {format_bytes(file.size)} }
                            button { r#type: "button", class: "btn", aria_label: format!("Remove {}", file.name),
                                onclick: move |_| {
                                    browser::keep_files(&kept_indexes(count, index));
                                    picked.set(browser::picked_files());
                                },
                                "Remove" }
                        }
                    }
                }
            }
        }
    }
}

/// The page-side work of the file list. Only the browser holds the picked files, so off the
/// browser the list stays empty and changing it does nothing.
mod browser {
    use super::PickedFile;

    /// The files the input holds now.
    pub fn picked_files() -> Vec<PickedFile> {
        #[cfg(target_arch = "wasm32")]
        {
            let Some(files) = upload_input().and_then(|input| input.files()) else {
                return Vec::new();
            };
            (0..files.length())
                .filter_map(|index| files.get(index))
                .map(|file| PickedFile {
                    name: file.name(),
                    size: file.size() as i64,
                })
                .collect()
        }
        #[cfg(not(target_arch = "wasm32"))]
        {
            Vec::new()
        }
    }

    /// Makes the input hold only the files at these positions, in their order.
    pub fn keep_files(keep: &[usize]) {
        #[cfg(target_arch = "wasm32")]
        {
            let Some(input) = upload_input() else { return };
            let Some(files) = input.files() else { return };
            let Ok(transfer) = web_sys::DataTransfer::new() else {
                return;
            };
            for &index in keep {
                if let Some(file) = files.get(index as u32) {
                    let _ = transfer.items().add_with_file(&file);
                }
            }
            input.set_files(transfer.files().as_ref());
        }
        #[cfg(not(target_arch = "wasm32"))]
        let _ = keep;
    }

    #[cfg(target_arch = "wasm32")]
    fn upload_input() -> Option<web_sys::HtmlInputElement> {
        use wasm_bindgen::JsCast;
        web_sys::window()?
            .document()?
            .get_element_by_id(super::UPLOAD_INPUT_ID)?
            .dyn_into()
            .ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn upload_page_is_a_multipart_form_that_keeps_the_refused_text() {
        let repo = RepoChrome {
            owner: "alice".into(),
            name: "project".into(),
            csrf_token: "csrf-fixture".into(),
            signed_in: true,
            can_write: true,
            current_path: "/alice/project/upload/main/docs".into(),
            ..Default::default()
        };
        let form = UploadForm {
            repository_id: 17,
            commit_to_new_branch: true,
            branch: "main".into(),
            directory: "docs".into(),
            expected_revision: "abc".into(),
            message: "Add <b>notes</b>".into(),
            description: "Kept".into(),
            error: Some("Each file can be at most 25 MiB.".into()),
            saved: false,
            protected: false,
            new_branch: String::new(),
        };
        let html =
            crate::webui::tests::in_scope(|| dioxus_ssr::render_element(upload_page(&repo, &form)));
        assert!(html.contains("Upload files to docs on main"));
        assert!(html.contains("enctype=\"multipart/form-data\""));
        assert!(html.contains("type=\"file\""));
        assert!(html.contains("name=\"files\""));
        assert!(html.contains("name=\"csrf_token\""));
        assert!(html.contains("csrf-fixture"));
        assert!(html.contains("name=\"expected_revision\""));
        assert!(html.contains("name=\"repository_id\" value=\"17\""));
        assert!(html.contains("value=\"new_branch\" checked"));
        assert!(html.contains("Drag files here or choose files"));
        assert!(html.contains("id=\"repository-upload-files\""));
        assert!(html.contains("class=\"repo-upload-zone\""));
        // Nothing is picked while the page renders, so the list is not drawn at all.
        assert!(!html.contains("class=\"repo-upload-selection\""));
        assert!(!html.contains("Clear all"));
        assert!(html.contains("Each file can be 25 MiB, and an upload 100 MiB in total."));
        assert!(!html.contains("<b>notes</b>"));
        assert!(html.contains("notes"));
        assert!(html.contains("Kept"));
        assert!(html.contains("Each file can be at most 25 MiB."));
        assert!(html.contains("Commit changes"));
        assert!(html.contains("Cancel"));
        assert!(!html.contains("<script"));
    }

    #[test]
    fn root_upload_page_has_no_breadcrumb_row() {
        let repo = RepoChrome {
            owner: "alice".into(),
            name: "project".into(),
            csrf_token: "csrf-fixture".into(),
            signed_in: true,
            can_write: true,
            ..Default::default()
        };
        let form = UploadForm {
            branch: "main".into(),
            ..Default::default()
        };
        let html =
            crate::webui::tests::in_scope(|| dioxus_ssr::render_element(upload_page(&repo, &form)));
        assert!(html.contains("Upload files to the root of main"));
        // The stylesheet is inlined in every repository page, so match the element, not its class name.
        assert!(!html.contains("class=\"repo-breadcrumbs\""));
    }

    #[test]
    fn selection_heading_counts_files_and_their_total_size() {
        let files = [
            PickedFile {
                name: "a.txt".into(),
                size: 8 * 1024,
            },
            PickedFile {
                name: "b.txt".into(),
                size: 4 * 1024,
            },
        ];
        assert_eq!(selection_summary(&files), "2 files, 12.0 KiB");
        assert_eq!(selection_summary(&files[..1]), "1 file, 8.0 KiB");
    }

    #[test]
    fn removing_a_file_keeps_the_others_in_order() {
        assert_eq!(kept_indexes(3, 0), vec![1, 2]);
        assert_eq!(kept_indexes(3, 1), vec![0, 2]);
        assert_eq!(kept_indexes(3, 2), vec![0, 1]);
        assert_eq!(kept_indexes(1, 0), Vec::<usize>::new());
        // A position past the end removes nothing.
        assert_eq!(kept_indexes(2, 5), vec![0, 1]);
    }
}
