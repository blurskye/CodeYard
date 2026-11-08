use codeyard_database::queries::users::accounts::AccountCredential;
use codeyard_types::credentials::AccountRestricted;

use codeyard_database::DbPool;
use codeyard_db_identity::users::blocks::BlockTargetMissing;
use codeyard_db_identity::users::blocks::InvalidBlock;

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum BlockError {
    #[error("User not found")]
    Missing,
    #[error("An account cannot block itself")]
    SelfBlock,
    #[error("A live unrestricted account credential is required")]
    Denied,
    #[error("Account blocking is temporarily unavailable")]
    Unavailable,
}

pub async fn change(
    pool: &DbPool,
    actor: i64,
    credential: &AccountCredential<'_>,
    username: &str,
    blocked: bool,
) -> Result<(), BlockError> {
    let target = codeyard_db_identity::users::accounts::get_user_by_username(pool, username)
        .await
        .map_err(|_| BlockError::Unavailable)?
        .ok_or(BlockError::Missing)?;
    codeyard_db_identity::users::blocks::change_block(pool, actor, credential, target.id, blocked)
        .await
        .map_err(|error| {
            if error.is::<AccountRestricted>() {
                BlockError::Denied
            } else if error.is::<InvalidBlock>() {
                BlockError::SelfBlock
            } else if error.is::<BlockTargetMissing>() {
                BlockError::Missing
            } else {
                BlockError::Unavailable
            }
        })
}
