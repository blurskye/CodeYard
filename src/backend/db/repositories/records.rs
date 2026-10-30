//! Shared named operation surface; native transactions stay inside db.
use crate::backend::db::models::{Repository, SharedRepository};
use crate::backend::db::DbPool;
use anyhow::Result;

native_operation!(crate::backend::db::postgres::repositories::records::create_repository,
    crate::backend::db::sqlite::repositories::records::create_repository,
    fn create_repository(
    pool: &DbPool,
    owner_id: i64,
    name: &str,
    description: Option<&str>,
    is_private: bool,
    disk_path: &str,
) -> Result<Repository>);

native_operation!(crate::backend::db::postgres::repositories::records::get_repository,
    crate::backend::db::sqlite::repositories::records::get_repository,
    fn get_repository(
    pool: &DbPool,
    owner_name: &str,
    repo_name: &str,
) -> Result<Option<Repository>>);

/// Git's gist namespace shares the same transport and storage authorization as repositories.
/// Transports strip one `.git` suffix before calling, so `name` is used exactly as given: a
/// repository may itself be named `project.git`.
pub async fn get_git_repository(
    pool: &DbPool,
    owner: &str,
    name: &str,
) -> Result<Option<Repository>> {
    if owner.eq_ignore_ascii_case("gists") || owner.eq_ignore_ascii_case("gist") {
        if let Some(repo) = super::super::gists::git_repository(pool, name).await? {
            return Ok(Some(repo));
        }
        if super::super::gists::is_identifier(pool, name).await? {
            return Ok(None);
        }
    }
    get_repository(pool, owner, name).await
}

native_operation!(crate::backend::db::postgres::repositories::records::get_repository_by_id,
    crate::backend::db::sqlite::repositories::records::get_repository_by_id,
    fn get_repository_by_id(pool: &DbPool, id: i64) -> Result<Option<Repository>>);

native_operation!(crate::backend::db::postgres::repositories::records::repository_pushed_at,
    crate::backend::db::sqlite::repositories::records::repository_pushed_at,
    fn repository_pushed_at(pool: &DbPool, repo_id: i64) -> Result<Option<chrono::DateTime<chrono::Utc>>>);

native_operation!(crate::backend::db::postgres::repositories::records::get_repositories_by_ids,
    crate::backend::db::sqlite::repositories::records::get_repositories_by_ids,
    fn get_repositories_by_ids(pool: &DbPool, ids: &[i64]) -> Result<Vec<Repository>>);

native_operation!(crate::backend::db::postgres::repositories::records::repository_owner_name,
    crate::backend::db::sqlite::repositories::records::repository_owner_name,
    /// Identity namespace only; callers must first authorize/scope the repository.
    fn repository_owner_name(pool: &DbPool, repo_id: i64) -> Result<Option<String>>);

native_operation!(crate::backend::db::postgres::repositories::records::list_user_repositories,
    crate::backend::db::sqlite::repositories::records::list_user_repositories,
    fn list_user_repositories(pool: &DbPool, user_id: i64) -> Result<Vec<Repository>>);

native_operation!(crate::backend::db::postgres::repositories::records::count_user_repositories,
    crate::backend::db::sqlite::repositories::records::count_user_repositories,
    fn count_user_repositories(pool: &DbPool, user_id: i64, include_private: bool) -> Result<i64>);

native_operation!(crate::backend::db::postgres::repositories::records::list_shared_repositories,
    crate::backend::db::sqlite::repositories::records::list_shared_repositories,
    fn list_shared_repositories(pool: &DbPool, user_id: i64) -> Result<Vec<SharedRepository>>);

native_operation!(crate::backend::db::postgres::repositories::records::list_user_repository_page,
    crate::backend::db::sqlite::repositories::records::list_user_repository_page,
    fn list_user_repository_page(pool: &DbPool, user: i64, include_private: bool, text: &str, limit: i64, offset: i64) -> Result<(Vec<Repository>, i64)>);
native_operation!(crate::backend::db::postgres::repositories::records::list_shared_repository_page,
    crate::backend::db::sqlite::repositories::records::list_shared_repository_page,
    fn list_shared_repository_page(pool: &DbPool, user: i64, text: &str, limit: i64, offset: i64) -> Result<(Vec<SharedRepository>, i64)>);

/// Order of a public repository list. The limit applies after this order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RepositorySort {
    /// Newest first.
    Recent,
    /// Most stars first, then newest first.
    Stars,
    /// Most forks first, then newest first.
    Forks,
    /// Most recently changed first.
    Updated,
}

/// Which public repositories an Explore search keeps.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RepositoryKind {
    #[default]
    All,
    /// Repositories that are not forks.
    Sources,
    Forks,
    Archived,
}

/// Narrowing for an Explore search. An empty language or topic keeps every repository's language
/// or topics.
#[derive(Debug, Clone, Copy, Default)]
pub struct RepositoryFilter<'a> {
    pub language: &'a str,
    pub topic: &'a str,
    pub kind: RepositoryKind,
}

impl RepositoryKind {
    /// The condition on `repositories` columns that keeps this kind.
    pub fn condition(self) -> &'static str {
        match self {
            Self::All => "TRUE",
            Self::Sources => "NOT is_fork",
            Self::Forks => "is_fork",
            Self::Archived => "is_archived",
        }
    }
}

native_operation!(crate::backend::db::postgres::repositories::records::public_languages,
    crate::backend::db::sqlite::repositories::records::public_languages,
    fn public_languages(pool: &DbPool, limit: i64) -> Result<Vec<String>>);

native_operation!(crate::backend::db::postgres::repositories::records::list_public_repositories,
    crate::backend::db::sqlite::repositories::records::list_public_repositories,
    fn list_public_repositories(
    pool: &DbPool,
    sort: RepositorySort,
    limit: i64,
) -> Result<Vec<Repository>>);

native_operation!(crate::backend::db::postgres::repositories::records::search_public_repositories,
    crate::backend::db::sqlite::repositories::records::search_public_repositories,
    fn search_public_repositories(
    pool: &DbPool,
    text: &str,
    filter: RepositoryFilter<'_>,
    sort: RepositorySort,
    limit: i64,
    offset: i64,
) -> Result<Vec<Repository>>);

/// Stable policy refusals; unrecognized database failures remain unknown errors.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum RepositorySettingsRefusal {
    #[error("Repository settings denied")]
    Denied,
    #[error("Repository visibility is locked")]
    VisibilityLocked,
    #[error("Invalid repository name")]
    InvalidName,
    #[error("Repository name is already reserved")]
    NameConflict,
    #[error("At least one merge method must stay allowed")]
    NoMergeMethod,
    #[error("Invalid repository topics")]
    InvalidTopics,
    #[error("Invalid repository website")]
    InvalidWebsite,
}

pub(crate) fn validate_repository_alias(name: &str) -> Result<()> {
    anyhow::ensure!(
        crate::webui::valid_repository_name(name),
        RepositorySettingsRefusal::InvalidName
    );
    Ok(())
}

native_operation!(crate::backend::db::postgres::repositories::records::update_repository,
    crate::backend::db::sqlite::repositories::records::update_repository,
    /// Saves the description, visibility and merge methods in one transaction. A refusal or a
    /// failed write leaves all three unchanged.
    fn update_repository(
    pool: &DbPool,
    actor_id: impl Into<crate::backend::db::users::accounts::AccountActor>,
    repo_id: i64,
    description: Option<&str>,
    is_private: impl Into<VisibilityUpdate>,
    merge_methods: MergeMethodUpdate,
) -> Result<()>);

native_operation!(crate::backend::db::postgres::repositories::records::repository_wiki,
    crate::backend::db::sqlite::repositories::records::repository_wiki,
    fn repository_wiki(pool: &DbPool, repo_id: i64) -> Result<bool>);

native_operation!(crate::backend::db::postgres::repositories::records::set_repository_wiki,
    crate::backend::db::sqlite::repositories::records::set_repository_wiki,
    fn set_repository_wiki(pool: &DbPool, actor: i64, repo_id: i64, enabled: bool) -> Result<()>);

#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
#[serde(bound(deserialize = "T: serde::Deserialize<'de>"))]
pub struct RepositoryChanges<T> {
    pub name: Option<T>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "nullable_change"
    )]
    pub description: Option<Option<T>>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "nullable_change"
    )]
    pub website: Option<Option<T>>,
    pub is_private: Option<bool>,
    pub visibility_expected: Option<bool>,
    pub archived: Option<bool>,
    pub is_template: Option<bool>,
    pub merge_methods: MergeMethodUpdate,
}

pub(crate) fn nullable_change<'de, D, T>(
    deserializer: D,
) -> std::result::Result<Option<Option<T>>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: serde::Deserialize<'de>,
{
    <Option<T> as serde::Deserialize>::deserialize(deserializer).map(Some)
}

impl RepositoryChanges<&str> {
    pub fn owned(&self) -> RepositoryChanges<String> {
        RepositoryChanges {
            name: self.name.map(str::to_owned),
            description: self.description.map(|value| value.map(str::to_owned)),
            website: self.website.map(|value| value.map(str::to_owned)),
            is_private: self.is_private,
            visibility_expected: self.visibility_expected,
            archived: self.archived,
            is_template: self.is_template,
            merge_methods: self.merge_methods,
        }
    }
}
impl RepositoryChanges<String> {
    pub fn borrowed(&self) -> RepositoryChanges<&str> {
        RepositoryChanges {
            name: self.name.as_deref(),
            description: self.description.as_ref().map(|value| value.as_deref()),
            website: self.website.as_ref().map(|value| value.as_deref()),
            is_private: self.is_private,
            visibility_expected: self.visibility_expected,
            archived: self.archived,
            is_template: self.is_template,
            merge_methods: self.merge_methods,
        }
    }
}

impl<T> RepositoryChanges<T> {
    pub fn changes_metadata(&self) -> bool {
        self.name.is_some()
            || self.description.is_some()
            || self.website.is_some()
            || self.is_private.is_some()
            || self.is_template.is_some()
            || self.merge_methods.merge.is_some()
            || self.merge_methods.squash.is_some()
            || self.merge_methods.rebase.is_some()
    }
}

native_operation!(crate::backend::db::postgres::repositories::records::patch_repository,
    crate::backend::db::sqlite::repositories::records::patch_repository,
    fn patch_repository(pool: &DbPool, actor: impl Into<crate::backend::db::users::accounts::AccountActor>, repo_id: i64, changes: &RepositoryChanges<&str>) -> Result<Repository>);

native_operation!(crate::backend::db::postgres::repositories::records::patch_repository_and_default,
    crate::backend::db::sqlite::repositories::records::patch_repository_and_default,
    /// Called under the repository Git lock, after HEAD was changed successfully.
    fn patch_repository_and_default(pool:&DbPool,actor:impl Into<crate::backend::db::users::accounts::AccountActor>,repo_id:i64,changes:&RepositoryChanges<&str>,name:&str,expected_owner:Option<&str>)->Result<Repository>);

/// Unconfirmed native saves may keep visibility, but must not change it concurrently.
pub struct VisibilityUpdate {
    pub is_private: bool,
    pub expected: Option<bool>,
}

impl From<bool> for VisibilityUpdate {
    fn from(is_private: bool) -> Self {
        Self {
            is_private,
            expected: None,
        }
    }
}

/// A change to the merge methods that pull requests may use. A method left as `None` keeps
/// its stored value.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct MergeMethodUpdate {
    pub merge: Option<bool>,
    pub squash: Option<bool>,
    pub rebase: Option<bool>,
}

native_operation!(crate::backend::db::postgres::repositories::records::set_merge_methods,
    crate::backend::db::sqlite::repositories::records::set_merge_methods,
    fn set_merge_methods(
    pool: &DbPool,
    actor_id: impl Into<crate::backend::db::users::accounts::AccountActor>,
    repo_id: i64,
    update: MergeMethodUpdate,
) -> Result<()>);

native_operation!(crate::backend::db::postgres::repositories::records::restore_repository_default_branch,
    crate::backend::db::sqlite::repositories::records::restore_repository_default_branch,
    fn restore_repository_default_branch(pool: &DbPool, repo_id: i64, expected: &str, previous: &str) -> Result<bool>);

native_operation!(crate::backend::db::postgres::repositories::records::set_repository_default_branch,
    crate::backend::db::sqlite::repositories::records::set_repository_default_branch,
    fn set_repository_default_branch(pool: &DbPool, actor_id: i64, repo_id: i64, branch: &str) -> Result<bool>);

native_operation!(crate::backend::db::postgres::repositories::records::set_repository_archived,
    crate::backend::db::sqlite::repositories::records::set_repository_archived,
    fn set_repository_archived(pool: &DbPool, actor_id: impl Into<crate::backend::db::users::accounts::AccountActor>, repo_id: i64, archived: bool) -> Result<bool>);

native_operation!(crate::backend::db::postgres::repositories::records::update_repo_default_branch,
    crate::backend::db::sqlite::repositories::records::update_repo_default_branch,
    fn update_repo_default_branch(pool: &DbPool, repo_id: i64, branch: &str) -> Result<()>);

native_operation!(crate::backend::db::postgres::repositories::records::advance_contribution_version,
    crate::backend::db::sqlite::repositories::records::advance_contribution_version,
    /// Advances the version that contributor proofs are keyed to. Pushes call it after the refs move.
    fn advance_contribution_version(pool: &DbPool, repo_id: i64) -> Result<()>);

native_operation!(crate::backend::db::postgres::repositories::records::update_repo_size_at,
    crate::backend::db::sqlite::repositories::records::update_repo_size_at,
    fn update_repo_size_at(pool: &DbPool, repo_id: i64, size: i64, node: Option<i64>) -> Result<()>);

native_operation!(crate::backend::db::postgres::repositories::records::update_repo_size,
    crate::backend::db::sqlite::repositories::records::update_repo_size,
    fn update_repo_size(pool: &DbPool, repo_id: i64, size: i64) -> Result<()>);

native_operation!(crate::backend::db::postgres::repositories::records::delete_repository,
    crate::backend::db::sqlite::repositories::records::delete_repository,
    fn delete_repository(pool: &DbPool, actor: impl Into<crate::backend::db::users::accounts::AccountActor>, repo_id: i64) -> Result<Repository>);

native_operation!(crate::backend::db::postgres::repositories::records::delete_unattached_advisory_fork,
    crate::backend::db::sqlite::repositories::records::delete_unattached_advisory_fork,
    fn delete_unattached_advisory_fork(pool: &DbPool, repo_id: i64) -> Result<Option<Repository>>);

native_operation!(crate::backend::db::postgres::repositories::records::delete_unwritten_repository,
    crate::backend::db::sqlite::repositories::records::delete_unwritten_repository,
    fn delete_unwritten_repository(pool: &DbPool, repo_id: i64) -> Result<Option<Repository>>);

native_operation!(crate::backend::db::postgres::repositories::records::rename_repository,
    crate::backend::db::sqlite::repositories::records::rename_repository,
    fn rename_repository(
    pool: &DbPool,
    actor_id: impl Into<crate::backend::db::users::accounts::AccountActor>,
    repo_id: i64,
    new_name: &str,
) -> Result<Repository>);

native_operation!(crate::backend::db::postgres::repositories::records::transfer_repository,
    crate::backend::db::sqlite::repositories::records::transfer_repository,
    fn transfer_repository(
    pool: &DbPool,
    actor_id: impl Into<crate::backend::db::users::accounts::AccountActor>,
    repo_id: i64,
    target: crate::backend::db::repositories::creation::RepositoryTarget,
) -> Result<Repository>);

// The primary checked actual tags under the Git lock. Keep the destination
// name stable while the ownership transaction commits.
native_operation!(crate::backend::db::postgres::repositories::records::transfer_repository_checked,
    crate::backend::db::sqlite::repositories::records::transfer_repository_checked,
    fn transfer_repository_checked(pool:&DbPool,actor_id:impl Into<crate::backend::db::users::accounts::AccountActor>,repo_id:i64,target:crate::backend::db::repositories::creation::RepositoryTarget,expected_owner:&str,expected_name:&str)->Result<Repository>);

/// Canonical namespaces for already-selected repositories. This display read does
/// not grant access; callers retain their visibility filtering before page limits.
pub async fn repository_owner_names(
    pool: &DbPool,
    repository_ids: &[i64],
) -> Result<std::collections::BTreeMap<i64, String>> {
    let mut ids = repository_ids.to_vec();
    ids.sort_unstable();
    ids.dedup();
    let mut owners = std::collections::BTreeMap::new();
    for chunk in ids.chunks(500) {
        let rows = match pool {
            DbPool::Postgres(pool) => {
                crate::backend::db::postgres::repositories::records::repository_owner_names(
                    pool, chunk,
                )
                .await?
            }
            DbPool::Sqlite(pool) => {
                crate::backend::db::sqlite::repositories::records::repository_owner_names(
                    pool, chunk,
                )
                .await?
            }
        };
        owners.extend(rows);
    }
    Ok(owners)
}
