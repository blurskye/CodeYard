//! Gist metadata, live account authority and durable Git-publication admission.
use super::{
    models::Repository,
    users::accounts::AccountCredential,
    DbPool,
};
use anyhow::Result;
use chrono::{DateTime, Utc};

#[derive(Clone, Debug, sqlx::FromRow)]
pub struct Gist {
    pub repo_id: i64,
    pub id: String,
    pub owner_id: i64,
    pub forked_from_id: Option<i64>,
    pub description: String,
    pub is_public: bool,
    pub ready: bool,
    pub comments: i64,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}
#[derive(Clone, Copy, Debug)]
pub enum ListScope {
    Public,
    Owner(i64, bool),
    Starred(i64),
    Forks(i64),
}
pub struct GistList {
    pub scope: ListScope,
    pub since: Option<DateTime<Utc>>,
    pub limit: i64,
    pub offset: i64,
}
pub struct NewGist<'a> {
    pub repo_id: i64,
    pub id: &'a str,
    pub description: &'a str,
    pub is_public: bool,
}
pub struct Admission<'a> {
    pub repo_id: i64,
    pub disk_path: &'a str,
    pub storage_node_id: Option<Option<i64>>,
    pub base: Option<&'a str>,
    pub sha: &'a str,
    pub description: Option<&'a str>,
}
#[derive(Clone, Debug, sqlx::FromRow)]
pub struct PendingWrite {
    pub sha: String,
    pub base: Option<String>,
    pub storage_node_id: Option<i64>,
}
#[derive(Clone, Debug, sqlx::FromRow)]
pub struct Comment {
    pub id: i64,
    pub repo_id: i64,
    pub user_id: Option<i64>,
    pub body: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}
#[derive(Debug, thiserror::Error)]
pub enum GistRefusal {
    #[error("Gist not found")]
    Missing,
    #[error("Gist permission denied")]
    Denied,
    #[error("Gist changed; reload before saving")]
    Conflict,
    #[error("Invalid gist input")]
    Invalid,
}

native_operation!(crate::backend::db::postgres::gists::is_identifier,crate::backend::db::sqlite::gists::is_identifier,
    fn is_identifier(pool:&DbPool,id:&str)->Result<bool>);
native_operation!(crate::backend::db::postgres::gists::orphaned_for_owner,crate::backend::db::sqlite::gists::orphaned_for_owner,
    fn orphaned_for_owner(pool:&DbPool,owner:i64)->Result<Vec<i64>>);
native_operation!(crate::backend::db::postgres::gists::unready_for_owner,crate::backend::db::sqlite::gists::unready_for_owner,
    fn unready_for_owner(pool:&DbPool,owner:i64)->Result<Vec<String>>);
native_operation!(crate::backend::db::postgres::gists::get,crate::backend::db::sqlite::gists::get,
    fn get(pool:&DbPool,id:&str) -> Result<Option<Gist>>);
native_operation!(crate::backend::db::postgres::gists::by_repository,crate::backend::db::sqlite::gists::by_repository,
    fn by_repository(pool:&DbPool,repo:i64) -> Result<Option<Gist>>);
native_operation!(crate::backend::db::postgres::gists::git_repository,crate::backend::db::sqlite::gists::git_repository,
    fn git_repository(pool:&DbPool,id:&str) -> Result<Option<Repository>>);
native_operation!(crate::backend::db::postgres::gists::list,crate::backend::db::sqlite::gists::list,
    fn list(pool:&DbPool,input:&GistList) -> Result<Vec<Gist>>);
native_operation!(crate::backend::db::postgres::gists::list_for_session,crate::backend::db::sqlite::gists::list_for_session,
    fn list_for_session(pool:&DbPool,input:&GistList,session:&str) -> Result<Vec<Gist>>);
native_operation!(crate::backend::db::postgres::gists::count_owned,crate::backend::db::sqlite::gists::count_owned,
    fn count_owned(pool:&DbPool,owner:i64,secret:bool,privacy:Option<bool>)->Result<i64>);
native_operation!(crate::backend::db::postgres::gists::list_created,crate::backend::db::sqlite::gists::list_created,
    fn list_created(pool:&DbPool,owner:i64,secret:bool,privacy:Option<bool>,limit:i64,offset:i64)->Result<Vec<Gist>>);
native_operation!(crate::backend::db::postgres::gists::reserve,crate::backend::db::sqlite::gists::reserve,
    fn reserve(pool:&DbPool,actor:i64,credential:&AccountCredential<'_>,input:&NewGist<'_>) -> Result<()>);
native_operation!(crate::backend::db::postgres::gists::admit,crate::backend::db::sqlite::gists::admit,
    fn admit(pool:&DbPool,actor:i64,credential:&AccountCredential<'_>,input:&Admission<'_>) -> Result<()>);
native_operation!(crate::backend::db::postgres::gists::pending,crate::backend::db::sqlite::gists::pending,
    fn pending(pool:&DbPool,repo:i64) -> Result<Option<PendingWrite>>);
native_operation!(crate::backend::db::postgres::gists::finish,crate::backend::db::sqlite::gists::finish,
    fn finish(pool:&DbPool,repo:i64,sha:&str,published:bool,placement:Option<Option<i64>>) -> Result<()>);
native_operation!(crate::backend::db::postgres::gists::pushed,crate::backend::db::sqlite::gists::pushed,
    fn pushed(pool:&DbPool,repo:i64,placement:Option<Option<i64>>) -> Result<()>);
native_operation!(crate::backend::db::postgres::gists::delete,crate::backend::db::sqlite::gists::delete,
    fn delete(pool:&DbPool,actor:i64,credential:&AccountCredential<'_>,repo:i64) -> Result<Repository>);
native_operation!(crate::backend::db::postgres::gists::discard,crate::backend::db::sqlite::gists::discard,
    fn discard(pool:&DbPool,repo:i64,orphan_only:bool) -> Result<Option<Repository>>);
native_operation!(crate::backend::db::postgres::gists::starred,crate::backend::db::sqlite::gists::starred,
    fn starred(pool:&DbPool,actor:i64,repo:i64) -> Result<bool>);
native_operation!(crate::backend::db::postgres::gists::star,crate::backend::db::sqlite::gists::star,
    fn star(pool:&DbPool,actor:i64,credential:&AccountCredential<'_>,repo:i64,adding:bool) -> Result<()>);
native_operation!(crate::backend::db::postgres::gists::comments,crate::backend::db::sqlite::gists::comments,
    fn comments(pool:&DbPool,repo:i64,limit:i64,offset:i64) -> Result<Vec<Comment>>);
native_operation!(crate::backend::db::postgres::gists::comment,crate::backend::db::sqlite::gists::comment,
    fn comment(pool:&DbPool,repo:i64,id:i64) -> Result<Option<Comment>>);
native_operation!(crate::backend::db::postgres::gists::create_comment,crate::backend::db::sqlite::gists::create_comment,
    fn create_comment(pool:&DbPool,actor:i64,credential:&AccountCredential<'_>,repo:i64,body:&str) -> Result<Comment>);
native_operation!(crate::backend::db::postgres::gists::change_comment,crate::backend::db::sqlite::gists::change_comment,
    fn change_comment(pool:&DbPool,actor:i64,credential:&AccountCredential<'_>,repo:i64,id:i64,body:Option<&str>) -> Result<Comment>);

native_operation!(crate::backend::db::postgres::gists::comment_position,crate::backend::db::sqlite::gists::comment_position,
    fn comment_position(pool:&DbPool,repo:i64,id:i64) -> Result<i64>);

pub fn validate_comment(body: &str) -> Result<()> {
    anyhow::ensure!(
        !body.trim().is_empty() && body.chars().count() <= 65535 && !body.contains('\0'),
        GistRefusal::Invalid
    );
    Ok(())
}
