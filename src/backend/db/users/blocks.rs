//! Blocking is private account policy, enforced at interaction effects as well as in REST.
use super::accounts::AccountCredential;
use crate::backend::db::{models::User, DbPool};
use anyhow::Result;

#[derive(Debug, thiserror::Error)]
#[error("Interactions with this account are restricted")]
pub struct InteractionBlocked;
#[derive(Debug, thiserror::Error)]
#[error("An account cannot block itself")]
pub struct InvalidBlock;
#[derive(Debug, thiserror::Error)]
#[error("The account was not found")]
pub struct BlockTargetMissing;

native_operation!(crate::backend::db::postgres::users::blocks::list_blocks,
    crate::backend::db::sqlite::users::blocks::list_blocks,
    fn list_blocks(pool: &DbPool, user: i64, limit: i64, offset: i64) -> Result<Vec<User>>);
native_operation!(crate::backend::db::postgres::users::blocks::is_blocked,
    crate::backend::db::sqlite::users::blocks::is_blocked,
    fn is_blocked(pool: &DbPool, user: i64, target: i64) -> Result<bool>);
native_operation!(crate::backend::db::postgres::users::blocks::change_block,
    crate::backend::db::sqlite::users::blocks::change_block,
    fn change_block(pool: &DbPool, user: i64, credential: &AccountCredential<'_>, target: i64, blocked: bool) -> Result<()>);
