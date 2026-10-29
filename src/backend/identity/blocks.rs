use crate::backend::db::{
    self,
    users::{
        accounts::{AccountCredential, AccountRestricted},
        blocks::{BlockTargetMissing, InvalidBlock},
    },
    DbPool,
};

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
    let target = db::users::accounts::get_user_by_username(pool, username)
        .await
        .map_err(|_| BlockError::Unavailable)?
        .ok_or(BlockError::Missing)?;
    db::users::blocks::change_block(pool, actor, credential, target.id, blocked)
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
