//! Repository invitations do not grant access until their recipient accepts.
use crate::backend::db::{
    models::{AccessLevel, User},
    DbPool,
};
use anyhow::Result;
use chrono::{DateTime, Utc};

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct Invitation {
    pub id: i64,
    pub repo_id: i64,
    pub invitee_id: i64,
    pub inviter_id: i64,
    pub permission: i16,
    pub created_at: DateTime<Utc>,
}

#[derive(sqlx::FromRow)]
pub struct InvitationSummary {
    #[sqlx(flatten)]
    pub invitation: Invitation,
    pub owner: String,
    pub repository: String,
    pub inviter: String,
}

#[derive(Debug, thiserror::Error)]
pub enum Refusal {
    #[error("Repository administration required")]
    Denied,
    #[error("Invitation or user not found")]
    NotFound,
    #[error("Repository owners cannot be collaborators")]
    Owner,
    #[error("A repository may send at most 50 invitations per day")]
    Limit,
}

pub enum Added {
    Invited(Invitation),
    Updated,
}

#[derive(sqlx::FromRow)]
pub struct Collaborator {
    #[sqlx(flatten)]
    pub user: User,
    pub permission: i16,
    pub direct: bool,
    pub organization_member: bool,
}

native_operation!(crate::backend::db::postgres::repositories::collaborators::add,
    crate::backend::db::sqlite::repositories::collaborators::add,
    fn add(pool: &DbPool, actor: i64, repo_id: i64, username: &str, permission: AccessLevel) -> Result<Added>);
native_operation!(crate::backend::db::postgres::repositories::collaborators::remove,
    crate::backend::db::sqlite::repositories::collaborators::remove,
    fn remove(pool: &DbPool, actor: i64, repo_id: i64, username: &str) -> Result<()>);
pub struct CollaboratorQuery<'a> {
    pub username: Option<&'a str>,
    pub affiliation: &'a str,
    pub permission: Option<i16>,
    pub offset: i64,
    pub limit: i64,
}

native_operation!(crate::backend::db::postgres::repositories::collaborators::list,
    crate::backend::db::sqlite::repositories::collaborators::list,
    fn list(pool: &DbPool, repo_id: i64, query: &CollaboratorQuery<'_>) -> Result<Vec<Collaborator>>);
native_operation!(crate::backend::db::postgres::repositories::collaborators::invitations,
    crate::backend::db::sqlite::repositories::collaborators::invitations,
    fn invitations(pool: &DbPool, actor: i64, repo_id: Option<i64>, offset: i64, limit: i64) -> Result<Vec<Invitation>>);
native_operation!(crate::backend::db::postgres::repositories::collaborators::recipient_invitations,
    crate::backend::db::sqlite::repositories::collaborators::recipient_invitations,
    fn recipient_invitations(pool: &DbPool, actor: i64, session: &str, offset: i64, limit: i64) -> Result<Vec<InvitationSummary>>);
native_operation!(crate::backend::db::postgres::repositories::collaborators::update_invitation,
    crate::backend::db::sqlite::repositories::collaborators::update_invitation,
    fn update_invitation(pool: &DbPool, actor: i64, repo_id: i64, id: i64, permission: Option<AccessLevel>) -> Result<Invitation>);
native_operation!(crate::backend::db::postgres::repositories::collaborators::respond,
    crate::backend::db::sqlite::repositories::collaborators::respond,
    fn respond(pool: &DbPool, actor: impl Into<crate::backend::db::users::accounts::AccountActor>, id: i64, accept: bool) -> Result<()>);
