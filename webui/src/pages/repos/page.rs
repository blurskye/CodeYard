//! Request-specific repository HTML. Data here is safe presentation data, never persistence rows.
//! This module is the repository lane: the `Page` enum the app renders, dispatching to each route.
use crate::pages::repos::{
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
        #[serde(default)]
        discussion: Box<super::commits::comments::Discussion>,
        #[serde(default)]
        related_pulls: Box<super::commits::pulls::RelatedPulls>,
        /// The side-by-side view is shown instead of the unified one.
        #[serde(default)]
        split: bool,
    },
    BranchMerge {
        repo: RepoChrome,
        form: Box<super::merge::MergeForm>,
    },
    CommitCommentDraft {
        repo: RepoChrome,
        sha: String,
        draft: Box<super::commits::comments::Draft>,
        message: String,
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
        settings: Box<Settings>,
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
    Security {
        repo: RepoChrome,
        page: super::security::Page,
    },
    Discussions {
        repo: RepoChrome,
        page: super::discussions::Page,
    },
    Dependabot {
        repo: RepoChrome,
        page: super::dependabot::Page,
    },
    Insights {
        repo: RepoChrome,
        page: super::insights::Page,
    },
    SecretSecurity {
        repo: RepoChrome,
        page: super::secret_scanning::Page,
    },
    Pages {
        repo: RepoChrome,
        page: super::pages::Page,
    },
    Advisories {
        repo: RepoChrome,
        page: Box<super::advisories::Page>,
    },
    DisclosureSecurity {
        repo: RepoChrome,
        page: Box<super::disclosure::Page>,
    },
    DependencyGraph {
        repo: RepoChrome,
        page: super::dependency_graph::Page,
    },
    Check {
        repo: RepoChrome,
        check: Box<super::checks::Check>,
    },
    Dependencies {
        repo: RepoChrome,
        page: Box<super::dependencies::Page>,
    },
    RepositoryEvent {
        repo: RepoChrome,
        page: Box<super::actions::RepositoryEventForm>,
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
            | Page::CommitCommentDraft { repo, .. }
            | Page::BranchMerge { repo, .. }
            | Page::Refs { repo, .. }
            | Page::Editor { repo, .. }
            | Page::DeleteFile { repo, .. }
            | Page::Upload { repo, .. }
            | Page::Settings { repo, .. }
            | Page::RejectedForm { repo, .. }
            | Page::Releases { repo, .. }
            | Page::Insights { repo, .. }
            | Page::Security { repo, .. }
            | Page::Dependabot { repo, .. }
            | Page::Discussions { repo, .. }
            | Page::SecretSecurity { repo, .. }
            | Page::DependencyGraph { repo, .. }
            | Page::Dependencies { repo, .. }
            | Page::Pages { repo, .. }
            | Page::Advisories { repo, .. }
            | Page::DisclosureSecurity { repo, .. }
            | Page::Check { repo, .. }
            | Page::RepositoryEvent { repo, .. } => repo,
        }
    }
}

/// Dispatch stays hook-free; interactive controls have their own component boundaries.
pub fn render(page: Page) -> Element {
    match page {
        Page::Tree { repo, page } => tree(&repo, &page),
        Page::BranchMerge { repo, form } => super::merge::merge(&repo, &form),
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
            discussion,
            related_pulls,
        } => commit_detail(
            &repo,
            &commit,
            &diff,
            &branches,
            &tags,
            split,
            &discussion,
            &related_pulls,
        ),
        Page::CommitCommentDraft {
            repo,
            sha,
            draft,
            message,
        } => super::commits::comments::rejected(&repo, &sha, &draft, &message),
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
        Page::Insights { repo, page } => super::insights::render(&repo, page),
        Page::Security { repo, page } => super::security::render(&repo, page),
        Page::Dependabot { repo, page } => super::dependabot::render(&repo, &page),
        Page::Discussions { repo, page } => super::discussions::render(&repo, &page),
        Page::SecretSecurity { repo, page } => super::secret_scanning::render(&repo, page),
        Page::Dependencies { repo, page } => super::dependencies::render(&repo, &page),
        Page::DependencyGraph { repo, page } => super::dependency_graph::render(&repo, &page),
        Page::Pages { repo, page } => super::pages::render(&repo, &page),
        Page::Advisories { repo, page } => super::advisories::render(&repo, &page),
        Page::DisclosureSecurity { repo, page } => super::disclosure::security(&repo, &page),
        Page::Check { repo, check } => super::checks::render(&repo, &check),
        Page::RepositoryEvent { repo, page } => super::actions::repository_event(&repo, &page),
        Page::RejectedForm {
            repo,
            action,
            message,
            values,
        } => rejected_form(&repo, &action, &message, &values),
    }
}
