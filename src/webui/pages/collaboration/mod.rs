//! Shared SSR/hydrated issue and PR presentation. Values are authorized by the caller.
use crate::webui::{
    components::references::References, encode_component, pages::repositories::RepoChrome,
};
use dioxus::prelude::*;

mod compare;
mod detail;
mod diff;
mod editor;
mod list;
mod milestones;
mod new;
mod review;

pub use compare::compare;
pub use detail::{detail, DetailDraft};
pub use diff::{diff_page, Diff, DiffFile, Viewed};
pub use list::list;
pub use new::{new, BranchChoices, IssueChoices, NewDraft};
pub use review::{
    Conversation, LineComment, Review, ReviewDraft, Reviewer, ReviewerStatus, Thread,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum Kind {
    Issue,
    Pull,
}

impl Kind {
    pub fn family(self) -> &'static str {
        match self {
            Self::Issue => "issues",
            Self::Pull => "pulls",
        }
    }
    pub fn singular(self) -> &'static str {
        match self {
            Self::Issue => "issue",
            Self::Pull => "pull request",
        }
    }
    pub fn title(self) -> &'static str {
        match self {
            Self::Issue => "Issues",
            Self::Pull => "Pull requests",
        }
    }
}

pub fn repo_url(owner: &str, repo: &str) -> String {
    format!("/{}/{}", encode_component(owner), encode_component(repo))
}

#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Repository {
    pub owner: String,
    pub name: String,
    pub signed_in: bool,
    pub can_write: bool,
    pub can_admin: bool,
    pub csrf: String,
    /// The signed-in viewer's username; empty when nobody is signed in.
    #[serde(default)]
    pub viewer: String,
    pub chrome: RepoChrome,
}
impl Repository {
    pub fn url(&self) -> String {
        repo_url(&self.owner, &self.name)
    }
    pub fn family_url(&self, kind: Kind) -> String {
        format!("{}/{}", self.url(), kind.family())
    }
}

/// The counts on the state tabs, after the search text.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ListCounts {
    pub open: i64,
    pub closed: i64,
    pub all: i64,
}

#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Label {
    pub id: i64,
    pub name: String,
    pub color: String,
}

/// The refusal for a merge commit into a branch that requires a linear history. The merge form
/// disables the merge commit method with this text, and the merge gives it as a blocker.
pub const MERGE_COMMIT_REFUSED: &str =
    "Merge commits are not allowed on this branch. Use squash or rebase.";

/// Whether the merge form can offer `method`. Only a merge commit is offered while the pull request
/// is stacked, and a branch that requires a linear history refuses a merge commit.
pub fn merge_method_available(compare: &Comparison, method: &str) -> bool {
    if method == "merge" {
        !compare.merge_commit_refused
    } else {
        !compare.merge_commit_only
    }
}

/// The merge method the merge form starts on: the one the submitter chose when it is still
/// available, otherwise the first one that is available. The form and the page's list of blockers
/// both use this.
pub fn preselected_merge_method<'a>(compare: &'a Comparison, submitted: &str) -> Option<&'a str> {
    let methods = &compare.merge_methods;
    methods
        .iter()
        .find(|method| method.as_str() == submitted && merge_method_available(compare, method))
        .or_else(|| {
            methods
                .iter()
                .find(|method| merge_method_available(compare, method))
        })
        .map(String::as_str)
}

/// A label color as `#rrggbb`, or None when the stored text is not six hex digits. Only this
/// value may go into a style attribute; an unchecked database color never does.
pub fn label_hex(color: &str) -> Option<String> {
    let hex = color.strip_prefix('#').unwrap_or(color);
    (hex.len() == 6 && hex.bytes().all(|b| b.is_ascii_hexdigit())).then(|| format!("#{hex}"))
}

/// The label pill style. The text keeps the theme color, so a white or black label stays
/// readable in both appearances.
pub fn label_style(color: &str) -> String {
    match label_hex(color) {
        Some(hex) => {
            let tint = format!("color-mix(in srgb, {hex} 25%, transparent)");
            format!("border: 1px solid {hex}; background: {tint}; color: var(--foreground)")
        }
        None => "border: 1px solid var(--border); color: var(--muted-foreground)".into(),
    }
}

#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Item {
    pub number: i32,
    pub title: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub body: String,
    pub state: String,
    pub author: String,
    pub created: String,
    pub updated: String,
    pub labels: Vec<Label>,
    pub comparison: Option<Comparison>,
    /// Whether the pull request is in a stack; the list marks it.
    #[serde(default)]
    pub stacked: bool,
    /// The stack a pull request page shows.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stack: Option<StackMap>,
    /// The number of the open pull request this one is based on, when it can join its stack.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stack_candidate: Option<i32>,
    /// The usernames assigned to the issue or pull request.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub assignees: Vec<String>,
    /// The milestone of an issue. Pull requests have none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub milestone: Option<Milestone>,
}

/// A milestone as its pages and the milestone choices show it.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Milestone {
    pub number: i32,
    pub title: String,
    pub description: Option<String>,
    /// The due day as YYYY-MM-DD.
    pub due_date: Option<String>,
    pub state: String,
    /// Open issues and pull requests in the milestone.
    pub open_items: i64,
    /// Closed issues and merged or closed pull requests in the milestone.
    pub closed_items: i64,
}

impl Milestone {
    /// The share of issues that are closed, from 0 to 100. A milestone with no issues has 0.
    pub fn progress(&self) -> i64 {
        let total = self.open_items + self.closed_items;
        if total == 0 {
            0
        } else {
            self.closed_items * 100 / total
        }
    }
}

/// The milestone form as typed, kept when a submission is refused.
#[derive(Clone, Debug, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct MilestoneDraft {
    pub title: String,
    pub description: String,
    pub due_date: String,
}

/// The choices of the list filters and the values they have selected. An empty value means
/// any; `none` means no assignee or no milestone.
#[derive(Clone, Debug, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ListFilters {
    /// The label names the list can filter by.
    pub labels: Vec<String>,
    /// The labels the list must all carry, in the order they were chosen.
    pub selected_labels: Vec<String>,
    pub assignees: Vec<String>,
    pub assignee: String,
    pub milestones: Vec<Milestone>,
    pub milestone: String,
    pub authors: Vec<String>,
    pub author: String,
    pub sort: String,
}
/// The stack a pull request page shows: its layers from the bottom up.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct StackMap {
    /// The branch the bottom layer is based on.
    pub trunk: String,
    /// The one-based place of the viewed pull request among the layers.
    pub position: usize,
    /// Some open layer is not on top of its base, so Rebase stack would move it.
    #[serde(default)]
    pub restack: bool,
    pub layers: Vec<StackLayer>,
}
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct StackLayer {
    pub number: i32,
    pub title: String,
    pub state: String,
    pub draft: bool,
    pub current: bool,
}
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Comparison {
    pub head: String,
    pub head_label: String,
    pub head_url: Option<String>,
    pub can_run_workflows: bool,
    pub base: String,
    pub head_sha: String,
    pub base_sha: String,
    pub draft: bool,
    /// The viewer may take this draft out of draft (presentation only).
    pub can_ready: bool,
    /// Open pull requests are stacked above or below this one, so only a merge commit is offered. Squash and rebase are then refused.
    pub merge_commit_only: bool,
    /// The branch this merge lands in requires a linear history, so the merge commit method is refused.
    pub merge_commit_refused: bool,
    /// The commit a merged pull request landed as on its base branch.
    pub merged_commit: Option<String>,
    /// The branch this merge lands in: the trunk of the stack for a layer, otherwise the base. The
    /// revision check still reads `base`.
    pub merge_target: String,
    /// The pull requests that merge together with this one, bottom first, this one last.
    pub merge_with: Vec<i32>,
    /// What blocks a merge right now, for the merge panel to list. Only writers get them, and
    /// the merge still checks everything again.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub blockers: Vec<String>,
    /// The viewer may approve or request changes: a writer who is not the author (presentation only).
    pub can_vote: bool,
    /// The merge methods the repository allows, in form order: `merge`, `squash`, `rebase`.
    pub merge_methods: Vec<String>,
}
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Comment {
    pub id: i64,
    pub author: String,
    pub body: String,
    pub created: String,
    pub location: Option<String>,
    /// Set on a root comment on a file, which is a review conversation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub conversation: Option<Conversation>,
}

/// One page of discussion comments, oldest first, and the reviews that fall between this
/// page's first comment and the next page's. `page` counts from 1.
#[derive(Clone, Debug, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct CommentPage {
    pub comments: Vec<Comment>,
    #[serde(default)]
    pub reviews: Vec<Review>,
    /// Set when the pull request has more reviews than the page loads; the value is that load limit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reviews_limit: Option<usize>,
    pub page: usize,
    pub has_more: bool,
}
/// Owned presentation data only. Permission flags select controls; they are never
/// trusted authority for a backend mutation. The CSRF value is the same public form
/// token already present in HTML, not a session credential or persistence record.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum Page {
    List {
        repository: Repository,
        kind: Kind,
        state: String,
        /// The search text the list is filtered by; empty when there is none.
        query: String,
        counts: ListCounts,
        items: Vec<Item>,
        #[serde(default)]
        filters: ListFilters,
    },
    New {
        repository: Repository,
        kind: Kind,
        draft: NewDraft,
        branches: BranchChoices,
        /// What a writer picks on a new issue. Empty for pull requests and for readers.
        #[serde(default)]
        choices: IssueChoices,
        diff: Option<Diff>,
        error: Option<String>,
        preview: bool,
        #[serde(default)]
        references: References,
    },
    Detail {
        repository: Repository,
        kind: Kind,
        item: Item,
        comments: CommentPage,
        available_labels: Vec<Label>,
        draft: DetailDraft,
        error: Option<String>,
        // Boxed to keep this variant close in size to the others (clippy::large_enum_variant).
        #[serde(default)]
        references: Box<References>,
        #[serde(default)]
        reviewers: Vec<Reviewer>,
        /// Users who may still be assigned, offered to a writer.
        #[serde(default)]
        assignable: Vec<String>,
        /// The repository's milestones, offered to a writer on an issue.
        #[serde(default)]
        milestones: Vec<Milestone>,
    },
    Milestones {
        repository: Repository,
        state: String,
        counts: ListCounts,
        items: Vec<Milestone>,
    },
    MilestoneNew {
        repository: Repository,
        draft: MilestoneDraft,
        error: Option<String>,
    },
    /// A milestone's page: its progress, its edit form and the issues in it.
    MilestoneDetail {
        repository: Repository,
        milestone: Milestone,
        state: String,
        items: Vec<Item>,
        pull_requests: Vec<Item>,
        draft: MilestoneDraft,
        error: Option<String>,
    },
    /// Two branches of one repository. `diff` is none when they are the same branch.
    Compare {
        repository: Repository,
        base: String,
        head: String,
        branches: Vec<String>,
        diff: Option<Diff>,
    },
    Diff {
        repository: Repository,
        item: Item,
        diff: Diff,
        /// Inline conversations, placed under their lines when the diff is shown.
        threads: Vec<Thread>,
        /// The inline comment list was cut at its limit; the Discussion tab lists every comment.
        truncated: bool,
        /// A composer to open under a line, kept when a submission is refused.
        composer: Option<LineComment>,
        error: Option<String>,
        /// The side-by-side view, from `?view=split`.
        #[serde(default)]
        split: bool,
        #[serde(default)]
        references: Box<References>,
    },
}

/// Dispatch is hook-free. Hook-using enhancements live in component boundaries
/// below, so navigating between unrelated Page variants cannot reorder App hooks.
pub fn render(page: Page) -> Element {
    match page {
        Page::List {
            repository,
            kind,
            state,
            query,
            counts,
            items,
            filters,
        } => list(&repository, kind, &state, &query, counts, &items, &filters),
        Page::New {
            repository,
            kind,
            draft,
            branches,
            choices,
            diff,
            error,
            preview,
            references,
        } => new(
            &repository,
            kind,
            &draft,
            &branches,
            &choices,
            diff.as_ref(),
            error.as_deref(),
            preview,
            &references,
        ),
        Page::Detail {
            repository,
            kind,
            item,
            comments,
            available_labels,
            draft,
            error,
            references,
            reviewers,
            assignable,
            milestones,
        } => detail(
            &repository,
            kind,
            &item,
            &comments,
            &available_labels,
            &draft,
            error.as_deref(),
            &references,
            &reviewers,
            &assignable,
            &milestones,
        ),
        Page::Milestones {
            repository,
            state,
            counts,
            items,
        } => milestones::list(&repository, &state, counts, &items),
        Page::MilestoneNew {
            repository,
            draft,
            error,
        } => milestones::new(&repository, &draft, error.as_deref()),
        Page::MilestoneDetail {
            repository,
            milestone,
            state,
            items,
            pull_requests,
            draft,
            error,
        } => milestones::detail(
            &repository,
            &milestone,
            &state,
            &items,
            &pull_requests,
            &draft,
            error.as_deref(),
        ),
        Page::Compare {
            repository,
            base,
            head,
            branches,
            diff,
        } => compare(&repository, &base, &head, &branches, diff.as_ref()),
        Page::Diff {
            repository,
            item,
            diff,
            threads,
            truncated,
            composer,
            error,
            references,
            split,
        } => diff_page(
            &repository,
            &item,
            &diff,
            &threads,
            truncated,
            composer.as_ref(),
            error.as_deref(),
            &references,
            split,
        ),
    }
}

#[cfg(test)]
mod tests;
