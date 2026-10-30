//! Shared named operation surface; native transactions stay inside db.
pub mod management;
use crate::backend::db::discussions::{ListFilter, StateCounts};
use crate::backend::db::models::{Issue, IssueComment};
use crate::backend::db::DbPool;
use anyhow::Result;

native_operation!(crate::backend::db::postgres::issues::issue_repositories,
    crate::backend::db::sqlite::issues::issue_repositories,
    fn issue_repositories(pool: &DbPool, ids: &[i64]) -> Result<Vec<(i64, i64)>>);

/// What a new issue starts with besides its text: a parent issue, labels, assignees, a milestone
/// and a type. The parent and labels and assignees need triage access, and the milestone and type
/// need write access, as when they are set later. The creating transaction checks all of it, so
/// the issue is saved with everything it asks for or not at all.
#[derive(Clone, Copy, Default)]
pub struct IssueMetadata<'a> {
    /// Initiating API credential, retained until the insert and metadata commit.
    pub token: Option<i64>,
    pub parent: Option<i64>,
    pub labels: &'a [i64],
    pub assignees: &'a [i64],
    pub milestone: Option<i32>,
    pub issue_type: Option<i64>,
}

native_operation!(crate::backend::db::postgres::issues::create_issue_with_metadata,
    crate::backend::db::sqlite::issues::create_issue_with_metadata,
    fn create_issue_with_metadata(
    pool: &DbPool,
    repo_id: i64,
    author_id: i64,
    title: &str,
    body: &str,
    metadata: IssueMetadata<'_>,
) -> Result<Issue>);

/// A new issue with only its text.
pub async fn create_issue(
    pool: &DbPool,
    repo_id: i64,
    author_id: i64,
    title: &str,
    body: &str,
) -> Result<Issue> {
    create_issue_with_metadata(
        pool,
        repo_id,
        author_id,
        title,
        body,
        IssueMetadata::default(),
    )
    .await
}

native_operation!(crate::backend::db::postgres::issues::issue_ids_by_numbers,
    crate::backend::db::sqlite::issues::issue_ids_by_numbers,
    fn issue_ids_by_numbers(pool: &DbPool, repo: i64, viewer: Option<i64>, numbers: &[i32]) -> Result<Vec<(i64, i32)>>);

native_operation!(crate::backend::db::postgres::issues::get_issue,
    crate::backend::db::sqlite::issues::get_issue,
    fn get_issue(pool: &DbPool, repo_id: i64, number: i32) -> Result<Option<Issue>>);

native_operation!(crate::backend::db::postgres::issues::get_issue_by_id,
    crate::backend::db::sqlite::issues::get_issue_by_id,
    fn get_issue_by_id(pool: &DbPool, id: i64) -> Result<Option<Issue>>);

native_operation!(crate::backend::db::postgres::issues::get_issues_by_ids,
    crate::backend::db::sqlite::issues::get_issues_by_ids,
    /// The issues of the repository among `ids` (at most 500), in no order.
    fn get_issues_by_ids(pool: &DbPool, repo_id: i64, ids: &[i64]) -> Result<Vec<Issue>>);

native_operation!(crate::backend::db::postgres::issues::list_repo_issues,
    crate::backend::db::sqlite::issues::list_repo_issues,
    fn list_repo_issues(
    pool: &DbPool,
    repo_id: i64,
    filter: &ListFilter<'_>,
    limit: i64,
) -> Result<Vec<IssueListEntry>>);

#[derive(Default)]
pub struct RepositoryItemOptions<'a> {
    pub since: Option<chrono::DateTime<chrono::Utc>>,
    pub any_assignee: bool,
    pub any_milestone: bool,
    pub issue_type: super::discussions::Pick,
    pub any_type: bool,
    pub mentioned: Option<&'a str>,
}

native_operation!(crate::backend::db::postgres::issues::list_repository_items,
    crate::backend::db::sqlite::issues::list_repository_items,
    fn list_repository_items(pool: &DbPool, repo_id: i64, filter: &ListFilter<'_>, options: &RepositoryItemOptions<'_>, limit: i64) -> Result<Vec<(i64, bool)>>);

pub(crate) fn repository_item_order(sort: super::discussions::ListSort) -> &'static str {
    use super::discussions::ListSort;
    match sort {
        ListSort::Newest => "i.created_at DESC",
        ListSort::Oldest => "i.created_at",
        ListSort::RecentlyUpdated => "i.updated_at DESC",
        ListSort::LeastRecentlyUpdated => "i.updated_at",
        ListSort::MostCommented => "i.comments DESC",
        ListSort::LeastCommented => "i.comments",
    }
}

native_operation!(crate::backend::db::postgres::issues::count_repo_issues,
    crate::backend::db::sqlite::issues::count_repo_issues,
    fn count_repo_issues(pool: &DbPool, repo_id: i64, filter: &ListFilter<'_>) -> Result<StateCounts>);

native_operation!(crate::backend::db::postgres::issues::update_issue,
    crate::backend::db::sqlite::issues::update_issue,
    fn update_issue(
    pool: &DbPool,
    issue_id: i64,
    edit: super::discussions::DiscussionEdit<'_>,
) -> Result<Option<Issue>>);

native_operation!(crate::backend::db::postgres::issues::update_issue_with_reason,
    crate::backend::db::sqlite::issues::update_issue_with_reason,
    fn update_issue_with_reason(pool: &DbPool, issue_id: i64, edit: super::discussions::DiscussionEdit<'_>, state_reason: Option<&str>) -> Result<Option<Issue>>);

native_operation!(crate::backend::db::postgres::issues::add_issue_comment,
    crate::backend::db::sqlite::issues::add_issue_comment,
    fn add_issue_comment(
    pool: &DbPool,
    issue_id: i64,
    author_id: i64,
    body: &str,
) -> Result<IssueComment>);

native_operation!(crate::backend::db::postgres::issues::list_issue_comments_ordered,
    crate::backend::db::sqlite::issues::list_issue_comments_ordered,
    fn list_issue_comments_ordered(pool: &DbPool, issue_id: i64, limit: i64, offset: i64, updated: bool, descending: bool) -> Result<Vec<IssueCommentEntry>>);
native_operation!(crate::backend::db::postgres::issues::list_issue_comments,
    crate::backend::db::sqlite::issues::list_issue_comments,
    fn list_issue_comments(
    pool: &DbPool,
    issue_id: i64,
    limit: i64,
    offset: i64,
) -> Result<Vec<IssueCommentEntry>>);

pub struct CommentFilter {
    pub number: Option<i32>,
    pub since: Option<chrono::DateTime<chrono::Utc>>,
    pub sort: super::discussions::ListSort,
    pub limit: i64,
    pub offset: i64,
}

native_operation!(crate::backend::db::postgres::issues::list_repository_comments,
    crate::backend::db::sqlite::issues::list_repository_comments,
    fn list_repository_comments(pool: &DbPool, repo_id: i64, filter: &CommentFilter) -> Result<Vec<(i64, bool)>>);

native_operation!(crate::backend::db::postgres::issues::get_issue_comment,
    crate::backend::db::sqlite::issues::get_issue_comment,
    fn get_issue_comment(pool: &DbPool, comment_id: i64) -> Result<Option<IssueComment>>);

// Changes the body of the author's comment on the issue. None when the change is refused.
native_operation!(crate::backend::db::postgres::issues::edit_issue_comment,
    crate::backend::db::sqlite::issues::edit_issue_comment,
    fn edit_issue_comment(
    pool: &DbPool,
    actor_id: i64,
    repo_id: i64,
    issue_id: i64,
    comment_id: i64,
    body: &str,
    expected_body: &str,
) -> Result<Option<IssueComment>>);

// Deletes a comment on the issue: its author, or a maintainer. False when refused.
native_operation!(crate::backend::db::postgres::issues::delete_issue_comment,
    crate::backend::db::sqlite::issues::delete_issue_comment,
    fn delete_issue_comment(
    pool: &DbPool,
    actor_id: i64,
    repo_id: i64,
    issue_id: i64,
    comment_id: i64,
) -> Result<bool>);

native_operation!(crate::backend::db::postgres::issues::issue_comment_position,
    crate::backend::db::sqlite::issues::issue_comment_position,
    fn issue_comment_position(pool: &DbPool, comment_id: i64) -> Result<i64>);

// Issues without comments have no row.
native_operation!(crate::backend::db::postgres::issues::count_issue_comments,
    crate::backend::db::sqlite::issues::count_issue_comments,
    fn count_issue_comments(pool: &DbPool, issue_ids: &[i64]) -> Result<Vec<(i64, i64)>>);

/// The issues a signed-in user is involved in: assigned to them, or opened by them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IssueRelation {
    Assigned,
    Created,
}

native_operation!(crate::backend::db::postgres::issues::list_viewer_issues,
    crate::backend::db::sqlite::issues::list_viewer_issues,
    fn list_viewer_issues(
    pool: &DbPool,
    viewer_id: i64,
    relation: IssueRelation,
    open: bool,
    limit: i64,
) -> Result<Vec<ViewerIssueEntry>>);

native_operation!(crate::backend::db::postgres::issues::count_viewer_issues,
    crate::backend::db::sqlite::issues::count_viewer_issues,
    fn count_viewer_issues(pool: &DbPool, viewer_id: i64, relation: IssueRelation) -> Result<StateCounts>);

#[derive(Debug, Clone)]
pub struct IssueListEntry {
    pub issue: Issue,
    pub author: crate::backend::db::users::accounts::AuthorDisplay,
}

#[derive(Debug, Clone)]
pub struct IssueCommentEntry {
    pub comment: IssueComment,
    pub author: crate::backend::db::users::accounts::AuthorDisplay,
}

/// An issue with the owner and name of its repository, for a link to it.
#[derive(Debug, Clone)]
pub struct ViewerIssueEntry {
    pub issue: Issue,
    pub repo_owner: String,
    pub repo_name: String,
}
