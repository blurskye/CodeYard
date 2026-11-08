use anyhow::Result;
use codeyard_database::native_operation;
use codeyard_database::queries::users::accounts::AccountCredential;
use codeyard_database::DbPool;

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize, sqlx::FromRow)]
pub struct SocialAccount {
    pub provider: String,
    pub url: String,
}

#[derive(Debug, thiserror::Error)]
#[error("At most four social accounts may be added")]
pub struct TooManyAccounts;

native_operation!(crate::postgres::users::social_accounts::list_social_accounts,
    crate::sqlite::users::social_accounts::list_social_accounts,
    fn list_social_accounts(pool: &DbPool, user: i64) -> Result<Vec<SocialAccount>>);

native_operation!(crate::postgres::users::social_accounts::change_social_accounts,
    crate::sqlite::users::social_accounts::change_social_accounts,
    fn change_social_accounts(pool: &DbPool, user: i64, credential: &AccountCredential<'_>, accounts: &[SocialAccount], add: bool) -> Result<Vec<SocialAccount>>);
