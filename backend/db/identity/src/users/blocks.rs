use anyhow::Result;
use codeyard_db_core::models::User;
use codeyard_db_core::native_operation;
use codeyard_db_core::queries::users::accounts::AccountCredential;
use codeyard_db_core::DbPool;

#[derive(Debug, thiserror::Error)]
#[error("Interactions with this account are restricted")]
pub struct InteractionBlocked;
#[derive(Debug, thiserror::Error)]
#[error("An account cannot block itself")]
pub struct InvalidBlock;
#[derive(Debug, thiserror::Error)]
#[error("The account was not found")]
pub struct BlockTargetMissing;

native_operation!(crate::postgres::users::blocks::list_blocks,
    crate::sqlite::users::blocks::list_blocks,
    fn list_blocks(pool: &DbPool, user: i64, limit: i64, offset: i64) -> Result<Vec<User>>);
native_operation!(crate::postgres::users::blocks::is_blocked,
    crate::sqlite::users::blocks::is_blocked,
    fn is_blocked(pool: &DbPool, user: i64, target: i64) -> Result<bool>);
native_operation!(crate::postgres::users::blocks::change_block,
    crate::sqlite::users::blocks::change_block,
    fn change_block(pool: &DbPool, user: i64, credential: &AccountCredential<'_>, target: i64, blocked: bool) -> Result<()>);
