//! Request-specific repository HTML. Data here is safe presentation data, never persistence rows.
use dioxus::prelude::*;

mod blame;
mod blob;
mod chrome;
mod commit_target;
mod commits;
mod delete_file;
mod editor;
mod find;
pub mod ref_path;
mod refs;
mod settings;
mod tree;
mod upload;

pub use blame::{blame, BlameHunk, BlamePage};
pub use blob::{blob, BlobPage};
pub use chrome::{active_tab, chrome, RepoChrome, TabStripScroll};
pub use commits::{commit_detail, commits, Commit, DiffFile};
pub use delete_file::{delete_file, DeleteFile, DELETE_REFUSED};
pub use editor::{editor, Editor, SAVED_FOLLOWUP_FAILED};
pub use find::{find, FindPage, MAX_LISTED_FILES};
pub use refs::{refs, RefRow};
pub use settings::{
    rejected_form, settings, settings_section, settings_tab, split_status_checks, BranchRule,
    Collaborator, Label, RuleDraft, Settings,
};
pub use tree::{tree, TreeEntry, TreePage, MAX_README_BYTES};
pub use upload::{upload_page, UploadForm, UPLOAD_MESSAGE};

/// Authorized presentation state shared by initial SSR and hydrated route rendering.
/// Permissions below are display hints only; every mutation is authorized again on the server.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum Page {
    Tree {
        repo: RepoChrome,
        page: TreePage,
    },
    Blob {
        repo: RepoChrome,
        page: BlobPage,
    },
    Blame {
        repo: RepoChrome,
        page: BlamePage,
    },
    Find {
        repo: RepoChrome,
        page: FindPage,
    },
    Commits {
        repo: RepoChrome,
        rows: Vec<Commit>,
        reference: String,
        page: usize,
        has_more: bool,
    },
    CommitDetail {
        repo: RepoChrome,
        commit: Commit,
        files: Vec<DiffFile>,
    },
    Refs {
        repo: RepoChrome,
        rows: Vec<RefRow>,
        tags: bool,
        /// Every branch, the default first: the commits a new branch or tag can start from.
        branches: Vec<RefRow>,
    },
    Editor {
        repo: RepoChrome,
        editor: Editor,
    },
    DeleteFile {
        repo: RepoChrome,
        file: DeleteFile,
    },
    Upload {
        repo: RepoChrome,
        form: UploadForm,
    },
    Settings {
        repo: RepoChrome,
        settings: Settings,
    },
    RejectedForm {
        repo: RepoChrome,
        action: String,
        message: String,
        values: std::collections::HashMap<String, String>,
    },
}

/// Dispatch stays hook-free; interactive controls have their own component boundaries.
pub fn render(page: Page) -> Element {
    match page {
        Page::Tree { repo, page } => tree(&repo, &page),
        Page::Blob { repo, page } => blob(&repo, &page),
        Page::Blame { repo, page } => blame(&repo, &page),
        Page::Find { repo, page } => find(&repo, &page),
        Page::Commits {
            repo,
            rows,
            reference,
            page,
            has_more,
        } => commits(&repo, &rows, &reference, page, has_more),
        Page::CommitDetail {
            repo,
            commit,
            files,
        } => commit_detail(&repo, &commit, &files),
        Page::Refs {
            repo,
            rows,
            tags,
            branches,
        } => refs(&repo, &rows, tags, &branches),
        Page::Editor {
            repo,
            editor: state,
        } => editor(&repo, &state),
        Page::DeleteFile { repo, file } => delete_file(&repo, &file),
        Page::Upload { repo, form } => upload_page(&repo, &form),
        Page::Settings {
            repo,
            settings: state,
        } => settings(&repo, &state),
        Page::RejectedForm {
            repo,
            action,
            message,
            values,
        } => rejected_form(&repo, &action, &message, &values),
    }
}

#[cfg(test)]
mod tests;
