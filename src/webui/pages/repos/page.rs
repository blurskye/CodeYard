//! Request-specific repository HTML. Data here is safe presentation data, never persistence rows.
//! This module is the repository lane: the `Page` enum the app renders, dispatching to each route.
use crate::webui::pages::repos::{
    blame::{blame, BlamePage},
    blob::{blob, BlobPage},
    branches::{refs, RefRow},
    chrome::RepoChrome,
    commits::{commit_detail, commits, Commit, HistoryFilter},
    editor::{delete_file, editor, upload_page, DeleteFile, Editor, UploadForm},
    find::{find, FindPage},
    pulls::Diff,
    settings::{rejected_form, settings, Settings},
    tree::{tree, TreePage},
};
use dioxus::prelude::*;

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
        /// The file or folder the history is narrowed to; empty for the whole ref.
        path: String,
        /// The author and date filters the list applies, as the form shows them.
        filter: HistoryFilter,
        page: usize,
        has_more: bool,
        branches: Vec<String>,
        tags: Vec<String>,
    },
    CommitDetail {
        repo: RepoChrome,
        commit: Commit,
        diff: Diff,
        /// The branches and tags whose tip is this commit.
        branches: Vec<String>,
        tags: Vec<String>,
        /// The side-by-side view is shown instead of the unified one.
        #[serde(default)]
        split: bool,
    },
    Refs {
        repo: RepoChrome,
        rows: Vec<RefRow>,
        tags: bool,
        /// Every branch, the default first: the commits a new branch or tag can start from.
        branches: Vec<RefRow>,
        /// The search text of the list; empty when the list is not filtered.
        query: String,
        /// The branch tab shown: `all`, `active` or `stale`.
        #[serde(default)]
        tab: String,
        /// How many of the branches have not changed for a quarter of a year.
        #[serde(default)]
        stale: usize,
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
    Releases {
        repo: RepoChrome,
        page: super::releases::Page,
    },
}

impl Page {
    /// The repository every repository page belongs to.
    pub fn repo_chrome(&self) -> &RepoChrome {
        match self {
            Page::Tree { repo, .. }
            | Page::Blob { repo, .. }
            | Page::Blame { repo, .. }
            | Page::Find { repo, .. }
            | Page::Commits { repo, .. }
            | Page::CommitDetail { repo, .. }
            | Page::Refs { repo, .. }
            | Page::Editor { repo, .. }
            | Page::DeleteFile { repo, .. }
            | Page::Upload { repo, .. }
            | Page::Settings { repo, .. }
            | Page::RejectedForm { repo, .. }
            | Page::Releases { repo, .. } => repo,
        }
    }
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
            path,
            filter,
            page,
            has_more,
            branches,
            tags,
        } => commits(
            &repo, &rows, &reference, &path, &filter, page, has_more, &branches, &tags,
        ),
        Page::CommitDetail {
            repo,
            commit,
            diff,
            branches,
            tags,
            split,
        } => commit_detail(&repo, &commit, &diff, &branches, &tags, split),
        Page::Refs {
            repo,
            rows,
            tags,
            branches,
            query,
            tab,
            stale,
        } => refs(&repo, &rows, tags, &branches, &query, &tab, stale),
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
        Page::Releases { repo, page } => super::releases::render(&repo, page),
        Page::RejectedForm {
            repo,
            action,
            message,
            values,
        } => rejected_form(&repo, &action, &message, &values),
    }
}
