use anyhow::Result;
use codeyard_database::models::User;
use codeyard_database::native_operation;
use codeyard_database::queries::users::accounts::AccountCredential;
use codeyard_database::DbPool;

#[derive(Clone, Copy, Debug)]
pub enum PullCapScope {
    Organization(i64),
    Repository(i64),
}
impl PullCapScope {
    pub fn column(self) -> &'static str {
        match self {
            Self::Organization(_) => "org_id",
            Self::Repository(_) => "repo_id",
        }
    }
    pub fn id(self) -> i64 {
        match self {
            Self::Organization(id) | Self::Repository(id) => id,
        }
    }
}
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize, sqlx::FromRow)]
pub struct PullCap {
    pub enabled: bool,
    pub max_open_pull_requests: i32,
    pub include_drafts: bool,
}
impl Default for PullCap {
    fn default() -> Self {
        Self {
            enabled: false,
            max_open_pull_requests: 1,
            include_drafts: false,
        }
    }
}
#[derive(Clone, Debug, serde::Deserialize)]
pub struct CapUpdate {
    pub enabled: bool,
    pub max_open_pull_requests: Option<i32>,
    pub include_drafts: Option<bool>,
}
impl CapUpdate {
    pub fn validate(&self) -> Result<()> {
        anyhow::ensure!(
            self.max_open_pull_requests
                .is_none_or(|max| (1..=1000).contains(&max)),
            PullCapRefusal::Invalid
        );
        Ok(())
    }
    pub fn apply(&self, previous: PullCap) -> PullCap {
        PullCap {
            enabled: self.enabled,
            max_open_pull_requests: self
                .max_open_pull_requests
                .unwrap_or(previous.max_open_pull_requests),
            include_drafts: self.include_drafts.unwrap_or(previous.include_drafts),
        }
    }
}
#[derive(Debug, thiserror::Error)]
pub enum PullCapRefusal {
    #[error("Administration permission is required")]
    Denied,
    #[error("The cap must be between 1 and 1000")]
    Invalid,
    #[error("At most 100 accounts can be listed")]
    Limit,
    #[error("An account was not found")]
    MissingUser,
}
#[derive(Debug, thiserror::Error)]
#[error("The {scope} limit of {maximum} open pull requests has been reached")]
pub struct PullCapReached {
    pub scope: &'static str,
    pub maximum: i32,
}

native_operation!(crate::postgres::pull_caps::get_cap,crate::sqlite::pull_caps::get_cap,
    fn get_cap(pool: &DbPool,actor: i64,scope: PullCapScope) -> Result<PullCap>);
native_operation!(crate::postgres::pull_caps::update_cap,crate::sqlite::pull_caps::update_cap,
    fn update_cap(pool: &DbPool,actor: i64,credential: &AccountCredential<'_>,scope: PullCapScope,input: &CapUpdate) -> Result<PullCap>);
native_operation!(crate::postgres::pull_caps::bypass_users,crate::sqlite::pull_caps::bypass_users,
    fn bypass_users(pool: &DbPool,actor: i64,repo: i64) -> Result<Vec<User>>);
native_operation!(crate::postgres::pull_caps::change_bypass,crate::sqlite::pull_caps::change_bypass,
    fn change_bypass(pool: &DbPool,actor: i64,credential: &AccountCredential<'_>,repo: i64,names: &[String],adding: bool) -> Result<()>);
