//! Commit discussions. Mutations recheck contribution or moderation authority in their transaction.
use crate::backend::db::{models::CommitComment, DbPool};
use anyhow::Result;

#[derive(Debug, thiserror::Error)]
pub enum CommentRefusal {
    #[error("Commit comment write denied")]
    Denied,
    #[error("Invalid commit comment")]
    Invalid,
    #[error("The comment changed; reload before saving")]
    Conflict,
}

pub struct CommentCreate<'a> {
    pub actor_id: i64,
    pub repo_id: i64,
    pub sha: &'a str,
    pub body: &'a str,
    pub path: Option<&'a str>,
    pub position: Option<i32>,
    pub line: Option<i32>,
}

pub struct CommentEdit<'a> {
    pub actor_id: i64,
    pub repo_id: i64,
    pub id: i64,
    pub body: &'a str,
    /// Native forms pin their displayed revision. REST PATCH intentionally replaces the body.
    pub expected_revision: Option<&'a str>,
}

#[derive(Debug)]
pub struct CommentEdited {
    pub comment: CommitComment,
    pub previous_body: String,
}

#[derive(Debug, sqlx::FromRow)]
pub struct CommentAuthor {
    pub id: i64,
    pub username: String,
    pub email: Option<String>,
    pub is_admin: bool,
    pub association: String,
}

pub fn validate_body(body: &str) -> Result<(), CommentRefusal> {
    if body.len() > 4 * 65_536
        || body.chars().count() > 65_536
        || body.trim().is_empty()
        || body.contains('\0')
    {
        return Err(CommentRefusal::Invalid);
    }
    Ok(())
}

pub(crate) fn validate_create(input: &CommentCreate<'_>) -> Result<(), CommentRefusal> {
    validate_body(input.body)?;
    if !crate::backend::git::is_full_commit_id(input.sha)
        || input
            .path
            .is_some_and(|path| path.is_empty() || path.len() > 4096 || path.contains('\0'))
        || (input.path.is_none() && (input.position.is_some() || input.line.is_some()))
        || input.position.is_some_and(|value| value <= 0)
        || input.line.is_some_and(|value| value <= 0)
    {
        return Err(CommentRefusal::Invalid);
    }
    Ok(())
}

native_operation!(crate::backend::db::postgres::repositories::commit_comments::create_comment,
    crate::backend::db::sqlite::repositories::commit_comments::create_comment,
    fn create_comment(pool: &DbPool, input: CommentCreate<'_>) -> Result<CommitComment>);
native_operation!(crate::backend::db::postgres::repositories::commit_comments::edit_comment,
    crate::backend::db::sqlite::repositories::commit_comments::edit_comment,
    fn edit_comment(pool: &DbPool, input: CommentEdit<'_>) -> Result<Option<CommentEdited>>);
native_operation!(crate::backend::db::postgres::repositories::commit_comments::delete_comment,
    crate::backend::db::sqlite::repositories::commit_comments::delete_comment,
    fn delete_comment(pool: &DbPool, actor: i64, repo: i64, id: i64, expected_revision: Option<&str>) -> Result<Option<CommitComment>>);
native_operation!(crate::backend::db::postgres::repositories::commit_comments::get_comment,
    crate::backend::db::sqlite::repositories::commit_comments::get_comment,
    fn get_comment(pool: &DbPool, repo: i64, id: i64) -> Result<Option<CommitComment>>);
native_operation!(crate::backend::db::postgres::repositories::commit_comments::list_comments,
    crate::backend::db::sqlite::repositories::commit_comments::list_comments,
    fn list_comments(pool: &DbPool, repo: i64, sha: Option<&str>, offset: i64, limit: i64) -> Result<Vec<CommitComment>>);
native_operation!(crate::backend::db::postgres::repositories::commit_comments::count_comments,
    crate::backend::db::sqlite::repositories::commit_comments::count_comments,
    fn count_comments(pool: &DbPool, repo: i64, sha: &str, through: Option<i64>) -> Result<i64>);
pub(crate) fn validate_counts(shas: &[String]) -> Result<(), CommentRefusal> {
    if shas.len() > 512
        || shas
            .iter()
            .any(|sha| !crate::backend::git::is_full_commit_id(sha))
    {
        return Err(CommentRefusal::Invalid);
    }
    Ok(())
}

native_operation!(crate::backend::db::postgres::repositories::commit_comments::comment_counts,
    crate::backend::db::sqlite::repositories::commit_comments::comment_counts,
    fn comment_counts(pool: &DbPool, repo: i64, shas: &[String]) -> Result<Vec<(String, i64)>>);
native_operation!(crate::backend::db::postgres::repositories::commit_comments::comment_authors,
    crate::backend::db::sqlite::repositories::commit_comments::comment_authors,
    fn comment_authors(pool: &DbPool, repo: i64, ids: &[i64]) -> Result<Vec<CommentAuthor>>);
