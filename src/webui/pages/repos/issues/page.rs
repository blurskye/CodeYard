//! Shared SSR/hydrated issue and PR presentation. Values are authorized by the caller.
//! This is the collaboration lane: the `Page` enum the app renders for issues and pull requests.
use super::new::chooser;
use super::{detail, list, new, BranchChoices, DetailDraft, IssueChoices, NewDraft, TemplateCard};
use crate::webui::pages::repos::{
    actions::Run,
    chrome::RepoChrome,
    commits::Commit,
    compare, labels, milestones,
    pulls::{self, Conversation, Diff, LineComment, Review, ReviewDraft, Reviewer, Thread},
};
use crate::webui::{
    components::{references::References, task_lists::TaskCount},
    encode_component,
};
use dioxus::prelude::*;
use std::collections::BTreeMap;

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
    /// What the label is for. Empty when none was written.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub description: String,
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

/// The label chip style: the label's own color as the background, with black or white text,
/// whichever reads better on it. The same chip is used on every page that shows labels.
pub fn label_style(color: &str) -> String {
    match label_hex(color) {
        Some(hex) => {
            let text = if label_is_light(&hex) {
                "#1f2328"
            } else {
                "#ffffff"
            };
            format!("border: 1px solid {hex}; background: {hex}; color: {text}")
        }
        None => "border: 1px solid var(--border); color: var(--muted-foreground)".into(),
    }
}

/// Whether black text reads better than white on a `#rrggbb` color, by perceived brightness.
fn label_is_light(hex: &str) -> bool {
    let channel = |at: usize| u32::from_str_radix(&hex[at..at + 2], 16).unwrap_or(0);
    let brightness = (channel(1) * 299 + channel(3) * 587 + channel(5) * 114) / 1000;
    brightness > 150
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
    /// The comments on the issue or pull request, for the counts in the lists and tabs.
    #[serde(default)]
    pub comment_count: i64,
    /// Whether the viewer may tick the task boxes of the body: its author or a writer.
    #[serde(default)]
    pub can_tick: bool,
    /// The task boxes in the body and how many are ticked. Lists show it; the detail page does not.
    #[serde(default)]
    pub tasks: TaskCount,
    /// The commits between the base and head of a pull request, for its Commits tab. None when Git
    /// could not count them.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub commit_count: Option<i64>,
    /// The files changed between the base and head of a pull request, for its Files changed tab.
    /// None when Git could not diff them.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file_count: Option<i64>,
    /// The workflow runs on the head commit of a pull request, for its Checks tab.
    #[serde(default)]
    pub check_count: i64,
    /// The reviewers' state of a pull request row: `approved` or `changes_requested`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub review_state: Option<String>,
    /// The checks state of a pull request row: `success`, `pending` or `failure`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub check_state: Option<String>,
    /// The reactions on the issue or pull request description.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub reactions: Vec<Reaction>,
}

/// One reaction on a description or comment: how many users gave it, and whether the viewer did.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Reaction {
    pub content: String,
    pub count: i64,
    pub mine: bool,
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
    /// The labels the list can filter by, with their colours.
    pub labels: Vec<Label>,
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
    /// A check run or a commit status has reported on the head commit.
    #[serde(default)]
    pub has_checks: bool,
    /// Checks on the head commit that did not pass: a run that failed or was cancelled, or a commit status that failed.
    #[serde(default)]
    pub failed_checks: i64,
    /// A check on the head commit is queued or running.
    #[serde(default)]
    pub checks_running: bool,
    /// The head commit has workflows, so "No checks configured" links to the Actions page.
    #[serde(default)]
    pub has_workflows: bool,
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
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub reactions: Vec<Reaction>,
    /// Set on a comment the viewer wrote: they may change its text.
    #[serde(default)]
    pub can_edit: bool,
    /// Set on a comment the viewer may delete: their own, or any when they maintain the repository.
    #[serde(default)]
    pub can_delete: bool,
    /// When the comment was last edited, after it was written. None for a comment never edited.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub edited: Option<String>,
    /// The text being edited when a form is re-rendered, such as a preview or a refused edit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub edit: Option<CommentEdit>,
}

/// An edit form that is shown again with the text the author typed.
#[derive(Clone, Debug, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct CommentEdit {
    pub body: String,
    pub preview: bool,
}

/// One page of discussion comments and reviews, oldest first. `page` counts from 1.
#[derive(Clone, Debug, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct CommentPage {
    pub comments: Vec<Comment>,
    #[serde(default)]
    pub reviews: Vec<Review>,
    pub page: usize,
    pub has_more: bool,
    /// The changes (state, title, labels, assignees, milestone) that fall in this page's span.
    #[serde(default)]
    pub events: Vec<Event>,
}

/// One change to an issue or pull request, shown between its comments.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Event {
    /// `Deleted user` when the account is gone.
    pub actor: String,
    /// `closed`, `reopened`, `renamed`, `labeled`, `unlabeled`, `assigned`, `unassigned`,
    /// `milestoned`, `demilestoned` or `merged`.
    pub kind: String,
    /// The label, user, milestone or new title, or the branch a pull request merged into.
    pub detail: String,
    /// The old title of a `renamed` event.
    pub previous: String,
    pub created: String,
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
    /// The template chooser of a new issue, shown when the repository has issue templates.
    Choose {
        repository: Repository,
        templates: Vec<TemplateCard>,
        blank_allowed: bool,
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
        /// How the list is ordered; empty for the closest due date first.
        #[serde(default)]
        sort: String,
    },
    /// The repository's labels. Administrators change them on this page. `query` and `sort` are
    /// what the search and sort controls were set to. `usage` counts, by label id, the issues and
    /// pull requests each label is on.
    Labels {
        repository: Repository,
        items: Vec<Label>,
        usage: BTreeMap<i64, i64>,
        query: String,
        sort: String,
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
    /// `open_pull` is the open pull request for this base and head, if there is one.
    Compare {
        repository: Repository,
        base: String,
        head: String,
        branches: Vec<String>,
        diff: Option<Diff>,
        open_pull: Option<pulls::PullSummary>,
        /// The side-by-side view is shown instead of the unified one.
        #[serde(default)]
        split: bool,
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
        /// A review refused on submission, kept in the review form.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        review: Option<ReviewDraft>,
        /// How many comments the viewer has waiting in their pending review.
        #[serde(default)]
        pending_comments: usize,
        error: Option<String>,
        /// The side-by-side view, from `?view=split`.
        #[serde(default)]
        split: bool,
        #[serde(default)]
        references: Box<References>,
    },
    /// The Commits tab of a pull request: its commits, newest first.
    PullCommits {
        repository: Repository,
        item: Item,
        commits: Vec<Commit>,
    },
    /// The Checks tab of a pull request: the workflow runs on its head commit.
    PullChecks {
        repository: Repository,
        item: Item,
        runs: Vec<Run>,
    },
}

impl Page {
    /// The repository every collaboration page belongs to.
    pub fn repo_chrome(&self) -> &RepoChrome {
        match self {
            Page::List { repository, .. }
            | Page::Choose { repository, .. }
            | Page::New { repository, .. }
            | Page::Detail { repository, .. }
            | Page::Milestones { repository, .. }
            | Page::Labels { repository, .. }
            | Page::MilestoneNew { repository, .. }
            | Page::MilestoneDetail { repository, .. }
            | Page::Compare { repository, .. }
            | Page::Diff { repository, .. }
            | Page::PullCommits { repository, .. }
            | Page::PullChecks { repository, .. } => &repository.chrome,
        }
    }
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
        Page::Choose {
            repository,
            templates,
            blank_allowed,
        } => chooser(&repository, &templates, blank_allowed),
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
            sort,
        } => milestones::list(&repository, &state, counts, &items, &sort),
        Page::Labels {
            repository,
            items,
            usage,
            query,
            sort,
        } => labels::list(&repository, &items, &query, &sort, &usage),
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
            open_pull,
            split,
        } => compare::compare(
            &repository,
            &base,
            &head,
            &branches,
            diff.as_ref(),
            open_pull.as_ref(),
            split,
        ),
        Page::Diff {
            repository,
            item,
            diff,
            threads,
            truncated,
            composer,
            review,
            pending_comments,
            error,
            references,
            split,
        } => pulls::diff_page(
            &repository,
            &item,
            &diff,
            &threads,
            truncated,
            composer.as_ref(),
            review.as_ref(),
            pending_comments,
            error.as_deref(),
            &references,
            split,
        ),
        Page::PullCommits {
            repository,
            item,
            commits,
        } => pulls::commits_page(&repository, &item, &commits),
        Page::PullChecks {
            repository,
            item,
            runs,
        } => pulls::checks_page(&repository, &item, &runs),
    }
}
