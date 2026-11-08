use crate::discussions::ListFilter;
use crate::discussions::StateCounts;
use anyhow::Result;
use codeyard_db_core::models::PullRequest;
use codeyard_db_core::models::Repository;
use codeyard_db_core::native_operation;
use codeyard_db_core::DbPool;

native_operation!(crate::postgres::pull_requests::records::commit_association_candidates,
    crate::sqlite::pull_requests::records::commit_association_candidates,
    fn commit_association_candidates(pool: &DbPool, repo: i64, before: i64, limit: i64, merged_only: bool) -> Result<Vec<PullRequest>>);

#[derive(Debug, thiserror::Error)]
#[error("duplicate_open_pull_request")]
pub struct DuplicateOpenPullRequest {
    pub number: i32,
}

#[derive(sqlx::FromRow)]
pub struct StackMetadata {
    pub id: i64,
    pub repo_id: i64,
    pub number: i32,
    pub base_branch: String,
    pub created_at: chrono::DateTime<chrono::Utc>,
}

native_operation!(crate::postgres::pull_requests::records::list_pull_request_stacks,
    crate::sqlite::pull_requests::records::list_pull_request_stacks,
    fn list_pull_request_stacks(pool: &DbPool, repo_id: i64, pull_number: Option<i32>, limit: i64, offset: i64) -> Result<Vec<StackMetadata>>);
native_operation!(crate::postgres::pull_requests::records::get_pull_request_stack,
    crate::sqlite::pull_requests::records::get_pull_request_stack,
    fn get_pull_request_stack(pool: &DbPool, repo_id: i64, number: i32) -> Result<Option<StackMetadata>>);
native_operation!(crate::postgres::pull_requests::records::unstack_named_stack,
    crate::sqlite::pull_requests::records::unstack_named_stack,
    fn unstack_named_stack(pool: &DbPool, actor_id: i64, repo_id: i64, stack_id: i64) -> Result<Option<Vec<PullRequest>>>);
native_operation!(crate::postgres::pull_requests::records::join_pull_request_stack_batch,
    crate::sqlite::pull_requests::records::join_pull_request_stack_batch,
    fn join_pull_request_stack_batch(pool: &DbPool, actor_id: i64, repo_id: i64, root: i64, pulls: &[i64], creating: bool) -> Result<StackJoin>);

#[derive(Debug, thiserror::Error)]
pub enum IssueConversionRefusal {
    #[error("Issue not found or already converted")]
    Missing,
    #[error("The issue state or metadata cannot be converted")]
    Invalid,
    #[error("Unlock the issue before converting it")]
    Locked,
    #[error("This discussion number already belongs to a pull request")]
    NumberInUse,
}

pub struct NewPullRequest<'a> {
    pub repo_id: i64,
    pub head_repo_id: Option<i64>,
    /// The requesting actor; conversion retains the existing issue's authorship.
    pub author_id: i64,
    pub issue: Option<i32>,
    pub title: &'a str,
    pub body: &'a str,
    pub head_branch: &'a str,
    pub head_sha: &'a str,
    pub base_branch: &'a str,
    pub base_sha: &'a str,
    pub is_draft: bool,
    pub maintainer_can_modify: bool,
}

/// Only a personal fork's owner can delegate writes; the same-repository flag grants no new access.
pub fn can_grant_maintainer_edit(
    actor: i64,
    base: i64,
    head: &codeyard_db_core::models::Repository,
) -> bool {
    head.id == base || (head.owner_kind == "user" && head.owner_user_id == Some(actor))
}

pub(crate) fn retarget_dismisses_reviews(patterns: &[String], branch: &str) -> Result<bool> {
    let mut budget = codeyard_model::workflow_glob::WorkflowMatchBudget::new();
    for pattern in patterns {
        if codeyard_model::workflow_glob::branch_pattern_matches(pattern, branch, &mut budget)
            .map_err(|_| anyhow::anyhow!("branch protection could not be checked"))?
        {
            return Ok(true);
        }
    }
    Ok(false)
}

/// The commits a pull request records for its branches. Comparing them with the
/// branches' tips shows whether a branch has moved.
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct PullRequestRevisions {
    pub id: i64,
    pub repo_id: i64,
    pub number: i32,
    pub author_id: i64,
    pub head_publisher_id: Option<i64>,
    pub head_repo_id: Option<i64>,
    pub head_branch: String,
    pub head_sha: String,
    pub base_branch: String,
    pub base_sha: String,
    pub stack_id: Option<i64>,
}

native_operation!(crate::postgres::pull_requests::records::list_open_pull_request_revisions,
    crate::sqlite::pull_requests::records::list_open_pull_request_revisions,
    fn list_open_pull_request_revisions(
    pool: &DbPool,
    repo_id: i64,
) -> Result<Vec<PullRequestRevisions>>);

native_operation!(crate::postgres::pull_requests::records::list_open_pull_request_revisions_on_branches,
    crate::sqlite::pull_requests::records::list_open_pull_request_revisions_on_branches,
    fn list_open_pull_request_revisions_on_branches(
    pool: &DbPool,
    repo_id: i64,
    branches: &[&str],
) -> Result<Vec<PullRequestRevisions>>);

#[derive(Debug, sqlx::FromRow)]
pub struct BranchPullRequest {
    pub head_branch: String,
    pub number: i32,
    pub title: String,
}

native_operation!(crate::postgres::pull_requests::records::find_open_pull_request_from_branches,
    crate::sqlite::pull_requests::records::find_open_pull_request_from_branches,
    fn find_open_pull_request_from_branches(pool: &DbPool, repo_id: i64, base: &str, head: &str) -> Result<Option<BranchPullRequest>>);
native_operation!(crate::postgres::pull_requests::records::merged_above,
    crate::sqlite::pull_requests::records::merged_above,
    /// Whether a pull request stacked on `head_branch` was merged in the same merge commit.
    fn merged_above(pool: &DbPool, repo_id: i64, merge_sha: &str, head_branch: &str) -> Result<bool>);

native_operation!(crate::postgres::pull_requests::records::merged_trunk,
    crate::sqlite::pull_requests::records::merged_trunk,
    /// The base branch and base commit of the bottom layer merged in `merge_sha`: the branch the
    /// merge commit landed on and its tip before the merge. A stack lands on its trunk, while
    /// each upper layer keeps naming the layer below it.
    fn merged_trunk(pool: &DbPool, repo_id: i64, merge_sha: &str) -> Result<Option<(String, String)>>);

native_operation!(crate::postgres::pull_requests::records::list_open_pull_requests_from_branches,
    crate::sqlite::pull_requests::records::list_open_pull_requests_from_branches,
    fn list_open_pull_requests_from_branches(
    pool: &DbPool,
    repo_id: i64,
    base: Option<&str>,
) -> Result<Vec<BranchPullRequest>>);

native_operation!(crate::postgres::pull_requests::records::move_open_pull_request_head,
    crate::sqlite::pull_requests::records::move_open_pull_request_head,
    fn move_open_pull_request_head(
    pool: &DbPool,
    pull_request_id: i64,
    old_head: &str,
    new_head: &str,
    head_repo: Option<i64>,
    head_branch: &str,
    pusher: Option<i64>,
) -> Result<bool>);

// Moves base_sha from `old_base` to `new_base`, only while the pull request is open, still has
// `old_base` and still targets `base_branch`. A refresh that read an earlier base branch cannot
// move the sha of a pull request that has been re-based since. `fork` keeps the base the layer was
// built on, when the caller knows it.
native_operation!(crate::postgres::pull_requests::records::move_open_pull_request_base,
    crate::sqlite::pull_requests::records::move_open_pull_request_base,
    fn move_open_pull_request_base(
    pool: &DbPool,
    pull_request_id: i64,
    old_base: &str,
    new_base: &str,
    base_branch: &str,
    fork: Option<&str>,
) -> Result<bool>);

// Drops the recorded fork point of an open pull request whose head now contains its base, as after
// a restack, so its diff starts at the base again. Only while the fork, base and head are still
// the ones the caller checked.
native_operation!(crate::postgres::pull_requests::records::clear_fork_point,
    crate::sqlite::pull_requests::records::clear_fork_point,
    fn clear_fork_point(
    pool: &DbPool,
    pull_request_id: i64,
    fork: &str,
    base: &str,
    head: &str,
) -> Result<bool>);

/// What came of asking to take a pull request out of draft.
#[derive(Debug)]
pub enum ReadyOutcome {
    /// It is ready for review now; this is the pull request as it is.
    Ready(Box<PullRequest>),
    Missing,
    /// Only its author and people with write access may.
    Denied,
    NotOpen,
    NotDraft,
    Changed,
}

// Checks who is asking and the state of the pull request in the same transaction as
// the change, so the answer is never based on an earlier read.
native_operation!(crate::postgres::pull_requests::records::approve_pull_request_workflows,
    crate::sqlite::pull_requests::records::approve_pull_request_workflows,
    fn approve_pull_request_workflows(pool: &DbPool, pr_id: i64, actor_id: i64, head: &str) -> Result<bool>);

native_operation!(crate::postgres::pull_requests::records::mark_pull_request_ready,
    crate::sqlite::pull_requests::records::mark_pull_request_ready,
    fn mark_pull_request_ready(pool: &DbPool, pr_id: i64, actor_id: i64, expected: Option<&str>) -> Result<ReadyOutcome>);

native_operation!(crate::postgres::pull_requests::records::set_pull_request_draft,
    crate::sqlite::pull_requests::records::set_pull_request_draft,
    fn set_pull_request_draft(pool: &DbPool, pr_id: i64, actor_id: i64, expected: Option<&str>, is_draft: bool) -> Result<ReadyOutcome>);

native_operation!(crate::postgres::pull_requests::records::create_pull_request,
    crate::sqlite::pull_requests::records::create_pull_request,
    fn create_pull_request(pool: &DbPool, input: NewPullRequest<'_>) -> Result<PullRequest>);

native_operation!(read crate::postgres::pull_requests::records::get_pull_request,
    crate::sqlite::pull_requests::records::get_pull_request,
    fn get_pull_request(
    pool: &DbPool,
    repo_id: i64,
    number: i32,
) -> Result<Option<PullRequest>>);

// A converted issue keeps its discussion number and native source provenance.
native_operation!(crate::postgres::pull_requests::records::get_converted_pull_request,
    crate::sqlite::pull_requests::records::get_converted_pull_request,
    fn get_converted_pull_request(pool: &DbPool, repo_id: i64, number: i32) -> Result<Option<PullRequest>>);

native_operation!(crate::postgres::pull_requests::records::retained_revisions,
    crate::sqlite::pull_requests::records::retained_revisions,
    fn retained_revisions(pool: &DbPool, repo: i64) -> Result<Vec<String>>);
native_operation!(read crate::postgres::pull_requests::records::get_pull_request_by_id,
    crate::sqlite::pull_requests::records::get_pull_request_by_id,
    fn get_pull_request_by_id(pool: &DbPool, id: i64) -> Result<Option<PullRequest>>);

native_operation!(read crate::postgres::pull_requests::records::get_pull_requests_by_ids,
    crate::sqlite::pull_requests::records::get_pull_requests_by_ids,
    /// The pull requests of the repository among `ids` (at most 500), in no order.
    fn get_pull_requests_by_ids(pool: &DbPool, repo_id: i64, ids: &[i64]) -> Result<Vec<PullRequest>>);

native_operation!(read crate::postgres::pull_requests::records::list_repo_pull_requests,
    crate::sqlite::pull_requests::records::list_repo_pull_requests,
    fn list_repo_pull_requests(
    pool: &DbPool,
    repo_id: i64,
    filter: &ListFilter<'_>,
    limit: i64,
) -> Result<Vec<PullRequestListEntry>>);

native_operation!(read crate::postgres::pull_requests::records::count_repo_pull_requests_by_state,
    crate::sqlite::pull_requests::records::count_repo_pull_requests_by_state,
    fn count_repo_pull_requests_by_state(pool: &DbPool, repo_id: i64, filter: &ListFilter<'_>) -> Result<Vec<(String, i64)>>);

native_operation!(read crate::postgres::pull_requests::records::count_repo_pull_requests,
    crate::sqlite::pull_requests::records::count_repo_pull_requests,
    fn count_repo_pull_requests(pool: &DbPool, repo_id: i64, filter: &ListFilter<'_>) -> Result<StateCounts>);

/// The pull requests a signed-in user is involved in: assigned to them, opened by them, or waiting
/// for their review. `Involved` is all three, each pull request once.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PullRequestRelation {
    Assigned,
    Created,
    ReviewRequested,
    Involved,
}

native_operation!(crate::postgres::pull_requests::records::list_viewer_pull_requests,
    crate::sqlite::pull_requests::records::list_viewer_pull_requests,
    fn list_viewer_pull_requests(
    pool: &DbPool,
    viewer_id: i64,
    relation: PullRequestRelation,
    open: bool,
    limit: i64,
) -> Result<Vec<ViewerPullRequestEntry>>);

native_operation!(crate::postgres::pull_requests::records::count_viewer_pull_requests,
    crate::sqlite::pull_requests::records::count_viewer_pull_requests,
    fn count_viewer_pull_requests(pool: &DbPool, viewer_id: i64, relation: PullRequestRelation) -> Result<StateCounts>);

/// A new base branch for an open pull request, and the commit it points at now.
pub struct NewBase<'a> {
    pub branch: &'a str,
    pub sha: &'a str,
    /// The server chose this base while reopening a stacked pull request whose parent merged.
    /// It belongs to the reopen, so it needs no base-edit permission; the store checks it again
    /// under lock and refuses it on anything but a reopen.
    pub automatic_reopen: bool,
}

/// Follow only merged layers of this stack; a cycle never authorizes a retarget.
pub fn reopened_stack_base(pull: &PullRequest, members: &[PullRequest]) -> Option<String> {
    let stack = pull.stack_id?;
    let mut branch = pull.base_branch.clone();
    let mut seen = std::collections::HashSet::new();
    while seen.insert(branch.clone()) {
        let Some(parent) = members.iter().find(|member| {
            member.repo_id == pull.repo_id
                && member.stack_id == Some(stack)
                && member.state == "merged"
                && member.head_repo_id == Some(pull.repo_id)
                && member.head_branch == branch
        }) else {
            return (branch != pull.base_branch).then_some(branch);
        };
        branch = parent.base_branch.clone();
    }
    None
}

#[derive(Default)]
pub struct PullRequestUpdate<'a> {
    pub base: Option<NewBase<'a>>,
    pub maintainer_can_modify: Option<bool>,
}

// A pull request in a stack cannot change its base, so a base change never touches a stack.
native_operation!(crate::postgres::pull_requests::records::update_pull_request,
    crate::sqlite::pull_requests::records::update_pull_request,
    fn update_pull_request(
    pool: &DbPool,
    pr_id: i64,
    edit: crate::discussions::DiscussionEdit<'_>,
    changes: PullRequestUpdate<'_>,
) -> Result<Option<PullRequest>>);

// After `from` merged into `to`, moves the open members of stack `stack_id` that are based on
// `from` onto `to`, whose commit is `to_sha`. Only members whose head is in this repository move;
// other pull requests based on `from` are never touched. Returns the members that moved, as the
// move left them, and the numbers left on `from`: a member whose head is `to`, or whose move would
// duplicate another open pull request.
native_operation!(crate::postgres::pull_requests::records::retarget_stacked_pull_requests,
    crate::sqlite::pull_requests::records::retarget_stacked_pull_requests,
    fn retarget_stacked_pull_requests(
    pool: &DbPool,
    actor_id: i64,
    repo_id: i64,
    stack_id: i64,
    from: &str,
    to: &str,
    to_sha: &str,
) -> Result<(Vec<PullRequest>, Vec<i32>)>);

// Takes every unmerged pull request of the stack that `pull_request_id` is in out of it. The stack
// is read in the transaction that checks write authority. Their base branches stay as they are. A
// pull request in no stack changes nothing. Returns false when the actor may not write to the
// repository, or the pull request is missing.
native_operation!(crate::postgres::pull_requests::records::unstack_pull_requests,
    crate::sqlite::pull_requests::records::unstack_pull_requests,
    fn unstack_pull_requests(
    pool: &DbPool,
    actor_id: i64,
    pull_request_id: i64,
) -> Result<Option<Vec<PullRequest>>>);

// Every pull request of a stack, merged ones included, in number order.
native_operation!(crate::postgres::pull_requests::records::list_stack_members,
    crate::sqlite::pull_requests::records::list_stack_members,
    fn list_stack_members(
    pool: &DbPool,
    repo_id: i64,
    stack_id: i64,
) -> Result<Vec<PullRequest>>);

// The id and stack version of every pull request of a stack, in id order. Each change to a
// pull request's place in the stack (joining, leaving, closing, reopening, retargeting, merging)
// raises its version, so two equal answers mean the stack did not change in between.
native_operation!(crate::postgres::pull_requests::records::list_stack_versions,
    crate::sqlite::pull_requests::records::list_stack_versions,
    fn list_stack_versions(
    pool: &DbPool,
    repo_id: i64,
    stack_id: i64,
) -> Result<Vec<(i64, i64)>>);

/// The most pull requests one stack holds, the pull request at its top included.
pub const MAX_STACK_LAYERS: usize = 25;

/// Why a pull request cannot join a stack.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StackRefusal {
    Missing,
    Denied,
    NotOpen,
    ForkHead,
    NoParent,
    ParentNotTop,
    AlreadyStacked,
    Cycle,
    TooLarge,
    /// The rows a join read kept changing under it, so it could not decide and write in one go.
    Changed,
}

/// What asking a pull request to join a stack came to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StackJoin {
    Joined(i64),
    AlreadyJoined(i64),
    Refused(StackRefusal),
}

/// A join to write: the pull request goes into `stack_id`, on top of `parent_id`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct JoinPlan {
    pub stack_id: i64,
    pub parent_id: i64,
    pub parent_number: i32,
    /// The parent is in no stack yet, so it becomes the bottom of the new one.
    pub parent_starts_stack: bool,
}

/// What deciding a join came to: the join to write, or the answer to give without writing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JoinCheck {
    Ready(JoinPlan),
    Done(StackJoin),
}

/// Whether joining `pull` onto `parent` closes a loop of base branches inside the stack the join
/// would form: `parent`'s stack members, with `parent` and `pull`. A member is based on the members
/// whose head is its base branch, so the loop exists when `pull` is reachable from `parent` that
/// way. Pull requests outside that set are left out, so none of them can hide the loop.
fn closes_cycle(pull: &PullRequest, parent: &PullRequest, parent_stack: &[PullRequest]) -> bool {
    let mut seen = std::collections::HashSet::from([parent.id]);
    let mut pending = vec![parent];
    while let Some(current) = pending.pop() {
        for member in parent_stack.iter().chain([parent, pull]) {
            if member.id == current.id || member.head_branch != current.base_branch {
                continue;
            }
            if member.id == pull.id {
                return true;
            }
            if seen.insert(member.id) {
                pending.push(member);
            }
        }
    }
    false
}

/// Decides whether `pull` may join a stack, from the rows a join reads. `parent` is the
/// lowest-numbered open pull request of the repository whose head is `pull`'s base branch, and
/// `parent_stack` holds the members of the stack `parent` is in (empty when it has none). The join
/// and the pull request page both use this, so their rules cannot drift.
pub fn decide_join(
    pull: &PullRequest,
    parent: Option<&PullRequest>,
    parent_stack: &[PullRequest],
) -> JoinCheck {
    if pull.state != "open" {
        return JoinCheck::Done(StackJoin::Refused(StackRefusal::NotOpen));
    }
    if pull.head_repo_id != Some(pull.repo_id) {
        return JoinCheck::Done(StackJoin::Refused(StackRefusal::ForkHead));
    }
    let Some(parent) = parent else {
        return JoinCheck::Done(StackJoin::Refused(StackRefusal::NoParent));
    };
    match (pull.stack_id, parent.stack_id) {
        (Some(stack), Some(parent_stack_id)) if stack == parent_stack_id => {
            return JoinCheck::Done(StackJoin::AlreadyJoined(stack));
        }
        (Some(_), _) => return JoinCheck::Done(StackJoin::Refused(StackRefusal::AlreadyStacked)),
        (None, _) => {}
    }
    if closes_cycle(pull, parent, parent_stack) {
        return JoinCheck::Done(StackJoin::Refused(StackRefusal::Cycle));
    }
    // Only open layers occupy the top of a stack; a closed or merged layer does not.
    if parent_stack.iter().any(|member| {
        member.id != parent.id && member.state == "open" && member.base_branch == parent.head_branch
    }) {
        return JoinCheck::Done(StackJoin::Refused(StackRefusal::ParentNotTop));
    }
    let members = if parent.stack_id.is_some() {
        parent_stack.len()
    } else {
        1
    };
    if members >= MAX_STACK_LAYERS {
        return JoinCheck::Done(StackJoin::Refused(StackRefusal::TooLarge));
    }
    JoinCheck::Ready(JoinPlan {
        stack_id: parent.stack_id.unwrap_or(parent.id),
        parent_id: parent.id,
        parent_number: parent.number,
        parent_starts_stack: parent.stack_id.is_none(),
    })
}

// Asks a pull request to join the stack of the open pull request its base branch names, or to
// start one with that pull request. Every rule is checked and the join written in one transaction.
native_operation!(crate::postgres::pull_requests::records::join_pull_request_stack,
    crate::sqlite::pull_requests::records::join_pull_request_stack,
    fn join_pull_request_stack(
    pool: &DbPool,
    actor_id: i64,
    pull_request_id: i64,
) -> Result<StackJoin>);

// The number of the open pull request a pull request could join right now, for the pull request
// page. It applies the same rules as the join and writes nothing.
native_operation!(crate::postgres::pull_requests::records::join_pull_request_stack_candidate,
    crate::sqlite::pull_requests::records::join_pull_request_stack_candidate,
    fn join_pull_request_stack_candidate(
    pool: &DbPool,
    pull_request_id: i64,
) -> Result<Option<i32>>);

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;

    fn pull(id: i64, head: &str, base: &str, stack_id: Option<i64>) -> PullRequest {
        PullRequest {
            id,
            repo_id: 1,
            number: id as i32,
            author_id: 1,
            head_publisher_id: Some(1),
            title: String::new(),
            body: String::new(),
            state: "open".to_owned(),
            locked: false,
            lock_reason: None,
            head_repo_id: Some(1),
            workflows_approved_sha: None,
            maintainer_can_modify: false,
            maintainer_edit_granted_by_id: None,
            stack_id,
            milestone_id: None,
            head_branch: head.to_owned(),
            head_sha: "a".repeat(40),
            base_branch: base.to_owned(),
            base_sha: "b".repeat(40),
            fork_sha: None,
            is_draft: false,
            mergeable: None,
            merged: false,
            merged_by_id: None,
            closed_by_id: None,
            merge_commit_sha: None,
            additions: 0,
            deletions: 0,
            changed_files: 0,
            created_at: Utc::now(),
            updated_at: Utc::now(),
            merged_at: None,
            closed_at: None,
        }
    }

    #[test]
    fn a_join_onto_a_loose_parent_starts_its_stack() {
        let parent = pull(1, "feature-1", "main", None);
        let joining = pull(2, "feature-2", "feature-1", None);
        let JoinCheck::Ready(plan) = decide_join(&joining, Some(&parent), &[]) else {
            panic!("a loose parent accepts a join");
        };
        assert_eq!(plan.stack_id, 1);
        assert!(plan.parent_starts_stack);
    }

    #[test]
    fn a_stack_of_the_maximum_size_refuses_one_more_layer() {
        let parent = pull(1, "feature-1", "main", Some(1));
        let mut members = vec![parent.clone()];
        members.extend(
            (2..=MAX_STACK_LAYERS as i64)
                .map(|id| pull(id, &format!("layer-{id}"), "main", Some(1))),
        );
        assert_eq!(members.len(), MAX_STACK_LAYERS);
        let joining = pull(100, "feature-100", "feature-1", None);
        assert_eq!(
            decide_join(&joining, Some(&parent), &members),
            JoinCheck::Done(StackJoin::Refused(StackRefusal::TooLarge))
        );
        members.pop();
        assert!(matches!(
            decide_join(&joining, Some(&parent), &members),
            JoinCheck::Ready(_)
        ));
    }

    #[test]
    fn a_parent_with_an_open_layer_on_its_head_is_not_the_top() {
        let parent = pull(1, "feature-1", "main", Some(1));
        let layer = pull(2, "feature-2", "feature-1", Some(1));
        let joining = pull(3, "feature-3", "feature-1", None);
        assert_eq!(
            decide_join(&joining, Some(&parent), &[parent.clone(), layer]),
            JoinCheck::Done(StackJoin::Refused(StackRefusal::ParentNotTop))
        );
    }

    #[test]
    fn a_closed_layer_on_the_parent_head_does_not_occupy_the_top() {
        let parent = pull(1, "feature-1", "main", Some(1));
        let mut closed = pull(2, "feature-2", "feature-1", Some(1));
        closed.state = "closed".to_owned();
        let joining = pull(3, "feature-3", "feature-1", None);
        assert!(matches!(
            decide_join(&joining, Some(&parent), &[parent.clone(), closed]),
            JoinCheck::Ready(_)
        ));
    }

    #[test]
    fn a_join_that_would_close_a_cycle_is_refused() {
        let parent = pull(2, "feature-2", "feature-1", None);
        let joining = pull(1, "feature-1", "feature-2", None);
        assert_eq!(
            decide_join(&joining, Some(&parent), &[]),
            JoinCheck::Done(StackJoin::Refused(StackRefusal::Cycle))
        );
    }
}

// Marks the pull requests of one merge as merged by `merge_sha`, all or nothing. Each must still be
// open, unmerged and based on the branch it was merged into, which is read from `pulls`.
native_operation!(crate::postgres::pull_requests::records::mark_pull_requests_merged,
    crate::sqlite::pull_requests::records::mark_pull_requests_merged,
    fn mark_pull_requests_merged(
    pool: &DbPool,
    pulls: &[PullRequest],
    actor_id: i64,
    merge_sha: &str,
) -> Result<bool>);

/// Filters applied before pagination in the REST pull request list.
pub struct PullRequestFilter<'a> {
    /// This projection requires a currently readable head, before pagination.
    pub viewer_id: Option<i64>,
    pub state: Option<&'a str>,
    pub base: Option<&'a str>,
    pub head_owner: Option<&'a str>,
    pub head_branch: Option<&'a str>,
    pub sort: &'a str,
    pub ascending: bool,
    pub month_ago: chrono::DateTime<chrono::Utc>,
}

impl PullRequestFilter<'_> {
    pub fn order(&self) -> String {
        let column = match self.sort {
            "updated" => "p.updated_at".to_owned(),
            "popularity" => format!(
                "(SELECT COUNT(*) FROM pr_comments c WHERE c.pull_request_id = p.id {})",
                crate::discussions::SHOWN_PR_COMMENT
            ),
            _ => "p.created_at".to_owned(),
        };
        let direction = if self.ascending { "ASC" } else { "DESC" };
        format!("{column} {direction}, p.id {direction}")
    }
}

native_operation!(crate::postgres::pull_requests::records::list_filtered_pull_requests,
    crate::sqlite::pull_requests::records::list_filtered_pull_requests,
    fn list_filtered_pull_requests(pool: &DbPool, repo_id: i64, filter: &PullRequestFilter<'_>, offset: i64, limit: i64) -> Result<Vec<PullRequest>>);

#[derive(Debug, Clone)]
pub struct PullRequestListEntry {
    pub pull_request: PullRequest,
    pub author: codeyard_db_identity::users::accounts::AuthorDisplay,
    pub head_repository: Option<PullRequestHeadDisplay>,
}

/// A pull request on the signed-in user's dashboard: what its row shows, and the owner and name
/// of its repository for a link to it.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct ViewerPullRequestEntry {
    pub number: i32,
    pub title: String,
    pub state: String,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub repo_owner: String,
    pub repo_name: String,
}

/// Display only; never substitutes for current repository access or placement.
#[derive(Debug, Clone)]
pub struct PullRequestHeadDisplay {
    pub id: i64,
    pub name: String,
    pub owner_name: Option<String>,
}

native_operation!(crate::postgres::pull_requests::records::create_pull_request_with_head,
    crate::sqlite::pull_requests::records::create_pull_request_with_head,
    fn create_pull_request_with_head(pool: &DbPool, input: NewPullRequest<'_>, foreign: Option<&codeyard_db_core::cells::repositories::Admission>) -> Result<PullRequest>);

pub(crate) async fn check_foreign_head(
    pool: &DbPool,
    input: &NewPullRequest<'_>,
    head: &Repository,
    foreign: Option<&codeyard_db_core::cells::repositories::Admission>,
) -> Result<()> {
    let Some(foreign) = foreign else {
        anyhow::ensure!(
            !head.is_stub,
            codeyard_types::credentials::AccountRestricted
        );
        return Ok(());
    };
    anyhow::ensure!(
        head.is_stub
            && foreign.actor.user_id == input.author_id
            && foreign.source.repository.id == head.id,
        codeyard_types::credentials::AccountRestricted
    );
    let current = foreign.read(pool).await?;
    anyhow::ensure!(
        current.repository.id == head.id
            && current.repository.home_cell == head.home_cell
            && current.repository.owner_id() == head.owner_id()
            && current.repository.forked_from_id == Some(input.repo_id),
        codeyard_types::credentials::AccountRestricted
    );
    Ok(())
}
