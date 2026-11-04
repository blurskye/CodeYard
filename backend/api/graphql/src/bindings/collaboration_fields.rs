//! Issue/PR collaboration fields used by api/query_builder.go in cli/cli.
use super::*;
use crate::bindings::Object;
use async_graphql::ID;

macro_rules! collaboration_fields {
    ($ty:literal) => {
        fields!($ty;
            author_association() -> gql::CommentAuthorAssociation;
            closed() -> bool; updated_at() -> gql::DateTime;
            repository() -> gql::Repository; milestone() -> Option<gql::Milestone>;
            assignees(after: Option<String>, before: Option<String>, first: Option<i32>, last: Option<i32>) -> gql::UserConnection;
            project_cards(after: Option<String>, archived_states: Option<Vec<Option<gql::ProjectCardArchivedState>>>, before: Option<String>, first: Option<i32>, last: Option<i32>) -> gql::ProjectCardConnection;
            project_items(after: Option<String>, before: Option<String>, first: Option<i32>, include_archived: Option<bool>, last: Option<i32>) -> Option<gql::ProjectV2ItemConnection>;
            reaction_groups() -> Option<Vec<gql::ReactionGroup>>;
        );
    };
}

impl gql::IssueResolver for Object {
    issue_fields!("Issue");
    collaboration_fields!("Issue");
    fields!("Issue";
        linked_branches(after: Option<String>, before: Option<String>, first: Option<i32>, last: Option<i32>) -> gql::LinkedBranchConnection;
        is_pinned()->Option<bool>;sub_issues_summary()->gql::SubIssuesSummary;
        sub_issues(after:Option<String>,before:Option<String>,first:Option<i32>,last:Option<i32>)->gql::IssueConnection;
        blocked_by(after:Option<String>,before:Option<String>,first:Option<i32>,last:Option<i32>,order_by:Option<gql::Input<gql::IssueDependencyOrder>>)->gql::IssueConnection;
        blocking(after:Option<String>,before:Option<String>,first:Option<i32>,last:Option<i32>,order_by:Option<gql::Input<gql::IssueDependencyOrder>>)->gql::IssueConnection;
        closed_by_pull_requests_references(after:Option<String>,before:Option<String>,exclude_user_linked:Option<bool>,first:Option<i32>,include_closed_prs:Option<bool>,last:Option<i32>,order_by_state:Option<bool>,user_linked_only:Option<bool>)->Option<gql::PullRequestConnection>;
        state() -> gql::IssueState; locked() -> bool; active_lock_reason()->Option<gql::LockReason>;
        state_reason(enable_duplicate: Option<bool>) -> Option<gql::IssueStateReason>;
        assigned_actors(after: Option<String>, before: Option<String>, first: Option<i32>, last: Option<i32>) -> gql::AssigneeConnection;
        issue_type() -> Option<gql::IssueType>; parent() -> Option<gql::Issue>;
    );
}
impl gql::PullRequestResolver for Object {
    issue_fields!("PullRequest");
    collaboration_fields!("PullRequest");
    fields!("PullRequest";
        closing_issues_references(after:Option<String>,before:Option<String>,exclude_user_linked:Option<bool>,first:Option<i32>,last:Option<i32>,order_by:Option<gql::Input<gql::IssueOrder>>,user_linked_only:Option<bool>)->Option<gql::IssueConnection>;
        reviews(after:Option<String>,author:Option<String>,before:Option<String>,first:Option<i32>,last:Option<i32>,states:Option<Vec<gql::PullRequestReviewState>>)->Option<gql::PullRequestReviewConnection>;
        latest_reviews(after:Option<String>,before:Option<String>,first:Option<i32>,last:Option<i32>)->Option<gql::PullRequestReviewConnection>;
        review_requests(after:Option<String>,before:Option<String>,first:Option<i32>,last:Option<i32>)->Option<gql::ReviewRequestConnection>;
        state() -> gql::PullRequestState; maintainer_can_modify()->bool; locked()->bool; active_lock_reason()->Option<gql::LockReason>; base_ref_name() -> String; head_ref_name() -> String;
        is_draft() -> bool; merged() -> bool; merged_at() -> Option<gql::DateTime>;
        files(after: Option<String>, before: Option<String>, first: Option<i32>, last: Option<i32>) -> Option<gql::PullRequestChangedFileConnection>;
        commits(after: Option<String>, before: Option<String>, first: Option<i32>, last: Option<i32>) -> gql::PullRequestCommitConnection;
        additions() -> i32; deletions() -> i32; changed_files() -> i32;
        mergeable() -> gql::MergeableState; merge_state_status() -> gql::MergeStateStatus;
        review_decision() -> Option<gql::PullRequestReviewDecision>;
        base_ref() -> Option<gql::Ref>;
        is_in_merge_queue() -> bool; is_merge_queue_enabled() -> bool;
        base_ref_oid() -> gql::GitObjectID; head_ref_oid() -> gql::GitObjectID;
        full_database_id() -> Option<gql::BigInt>; is_cross_repository() -> bool;
        head_repository() -> Option<gql::Repository>; head_repository_owner() -> Option<gql::RepositoryOwner>;
        merged_by() -> Option<gql::Actor>; merge_commit() -> Option<gql::Commit>;
        potential_merge_commit() -> Option<gql::Commit>; auto_merge_request() -> Option<gql::AutoMergeRequest>;
        viewer_merge_headline_text(merge_type: Option<gql::PullRequestMergeMethod>) -> String;
        viewer_merge_body_text(merge_type: Option<gql::PullRequestMergeMethod>) -> String;

    );
}
impl gql::IssueCommentResolver for Object {
    fields!("IssueComment";
        id() -> ID; database_id() -> Option<i32>; body() -> String; author() -> Option<gql::Actor>;
        created_at() -> gql::DateTime; updated_at() -> gql::DateTime;
        author_association() -> gql::CommentAuthorAssociation;
        includes_created_edit() -> bool; is_minimized() -> bool; minimized_reason() -> Option<String>;
        reaction_groups() -> Option<Vec<gql::ReactionGroup>>; url() -> gql::URI; viewer_did_author() -> bool;
    );
}
impl gql::SubIssuesSummaryResolver for Object {
    fields!("SubIssuesSummary";total()->i32;completed()->i32;percent_completed()->i32;);
}
impl gql::MilestoneResolver for Object {
    fields!("Milestone";
        id() -> ID; number() -> i32; title() -> String; description() -> Option<String>;
        state() -> gql::MilestoneState; created_at() -> gql::DateTime; updated_at() -> gql::DateTime;
        closed_at() -> Option<gql::DateTime>; due_on() -> Option<gql::DateTime>; url() -> gql::URI;
        closed()->bool;creator()->Option<gql::Actor>;progress_percentage()->f64;repository()->gql::Repository;
    );
}
impl gql::ReactionGroupResolver for Object {
    fields!("ReactionGroup";
        content() -> gql::ReactionContent; viewer_has_reacted() -> bool;
        users(after: Option<String>, before: Option<String>, first: Option<i32>, last: Option<i32>) -> gql::ReactingUserConnection;
    );
}
impl gql::ReactingUserConnectionResolver for Object {
    fields!("ReactingUserConnection"; total_count() -> i32;);
}
impl gql::ProjectCardResolver for Object {}
impl gql::IssueTypeResolver for Object {
    fields!("IssueType"; id() -> ID; name() -> String; description() -> Option<String>; color() -> gql::IssueTypeColor;);
}
connection!(
    ProjectCardConnectionResolver,
    ProjectCardEdgeResolver,
    "ProjectCardConnection",
    "ProjectCardEdge",
    ProjectCard,
    ProjectCardEdge
);
connection!(
    AssigneeConnectionResolver,
    AssigneeEdgeResolver,
    "AssigneeConnection",
    "AssigneeEdge",
    Assignee,
    AssigneeEdge
);
