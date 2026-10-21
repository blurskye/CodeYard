//! Shared named operation surface; native transactions stay inside db.
use crate::backend::db::discussions::{ListFilter, StateCounts};
use crate::backend::db::models::{Issue, IssueComment};
use crate::backend::db::DbPool;
use anyhow::Result;

native_operation!(crate::backend::db::postgres::issues::create_issue,
    crate::backend::db::sqlite::issues::create_issue,
    fn create_issue(
    pool: &DbPool,
    repo_id: i64,
    author_id: i64,
    title: &str,
    body: &str,
) -> Result<Issue>);

native_operation!(crate::backend::db::postgres::issues::get_issue,
    crate::backend::db::sqlite::issues::get_issue,
    fn get_issue(pool: &DbPool, repo_id: i64, number: i32) -> Result<Option<Issue>>);

native_operation!(crate::backend::db::postgres::issues::get_issue_by_id,
    crate::backend::db::sqlite::issues::get_issue_by_id,
    fn get_issue_by_id(pool: &DbPool, id: i64) -> Result<Option<Issue>>);

native_operation!(crate::backend::db::postgres::issues::list_repo_issues,
    crate::backend::db::sqlite::issues::list_repo_issues,
    fn list_repo_issues(
    pool: &DbPool,
    repo_id: i64,
    filter: &ListFilter<'_>,
    limit: i64,
) -> Result<Vec<IssueListEntry>>);

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

native_operation!(crate::backend::db::postgres::issues::add_issue_comment,
    crate::backend::db::sqlite::issues::add_issue_comment,
    fn add_issue_comment(
    pool: &DbPool,
    issue_id: i64,
    author_id: i64,
    body: &str,
) -> Result<IssueComment>);

native_operation!(crate::backend::db::postgres::issues::list_issue_comments,
    crate::backend::db::sqlite::issues::list_issue_comments,
    fn list_issue_comments(
    pool: &DbPool,
    issue_id: i64,
    limit: i64,
    offset: i64,
) -> Result<Vec<IssueCommentEntry>>);

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
