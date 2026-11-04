use anyhow::Result;
use codeyard_db_core::models::File;
use codeyard_db_core::models::Release;
use codeyard_db_core::models::ReleaseAsset;
use codeyard_db_core::models::ReleaseRow;
use codeyard_db_core::native_operation;
use codeyard_db_core::DbPool;
use uuid::Uuid;

pub mod notes;

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum ReleaseRefusal {
    #[error("Release mutation denied")]
    Denied,
    #[error("Release not found")]
    NotFound,
    #[error("A release with this tag already exists")]
    TagTaken,
    #[error("An asset with this name already exists")]
    AssetNameTaken,
    #[error("The release is immutable")]
    Immutable,
    #[error("The tag name is reserved by an immutable release")]
    TagReserved,
    #[error("Release changed")]
    Conflict,
    #[error("A release can contain at most 1000 assets")]
    TooManyAssets,
}

/// GitHub's legacy election favors the highest semantic version, then publication age.
pub(crate) fn legacy_latest(rows: &[(i64, String, chrono::DateTime<chrono::Utc>)]) -> Option<i64> {
    rows.iter()
        .max_by_key(|(id, tag, created)| {
            (
                semver::Version::parse(tag.strip_prefix('v').unwrap_or(tag)).ok(),
                *created,
                *id,
            )
        })
        .map(|(id, _, _)| *id)
}

/// The most assets one release holds; immutable publication signs at most this many.
pub const MAX_RELEASE_ASSETS: i64 = 1000;

pub enum ReleaseSelection<'a> {
    Page { limit: i64, offset: i64 },
    Id(i64),
    Tag(&'a str),
}

pub struct ReleaseSnapshot {
    pub repository: codeyard_db_core::models::Repository,
    pub releases: Vec<ReleaseRow>,
    pub assets: Vec<ReleaseAsset>,
    pub total: i64,
}

native_operation!(crate::postgres::releases::read::read_release_snapshot,
    crate::sqlite::releases::read::read_release_snapshot,
    fn read_release_snapshot(pool: &DbPool, viewer: Option<i64>, repo_id: i64, selection: ReleaseSelection<'_>) -> Result<ReleaseSnapshot>);

/// An uploaded file being attached to a release, with the details the asset row keeps.
#[derive(Debug, Clone)]
pub struct NewReleaseAsset<'a> {
    pub file_id: Uuid,
    pub name: &'a str,
    pub label: Option<&'a str>,
    pub content_type: &'a str,
    pub size: i64,
}

#[derive(Debug, Default)]
pub struct AssetChanges {
    pub name: Option<String>,
    pub label: Option<Option<String>>,
}

native_operation!(crate::postgres::releases::update_release_asset,
    crate::sqlite::releases::update_release_asset,
    fn update_release_asset(pool: &DbPool, actor: i64, repo_id: i64, asset_id: i64, changes: &AssetChanges) -> Result<ReleaseAsset>);

#[derive(Debug, Clone, Copy, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LatestSelection {
    #[default]
    Keep,
    Promote,
    Legacy,
}
impl From<bool> for LatestSelection {
    fn from(value: bool) -> Self {
        if value {
            Self::Promote
        } else {
            Self::Keep
        }
    }
}

/// The fields a writer sets on a release.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct ReleaseFields {
    pub name: Option<String>,
    pub body: Option<String>,
    pub is_draft: bool,
    pub is_prerelease: bool,
    /// Asks for the latest release. Drafts and pre-releases never are, and a new latest release
    /// takes the flag from the one that had it.
    pub make_latest: LatestSelection,
}

/// Only supplied fields change; omitted values are read under the write lock.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct ReleaseChanges {
    pub expected_revision: Option<String>,
    pub tag: Option<String>,
    pub target: Option<String>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "crate::repositories::records::nullable_change"
    )]
    pub name: Option<Option<String>>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "crate::repositories::records::nullable_change"
    )]
    pub body: Option<Option<String>>,
    pub is_draft: Option<bool>,
    pub is_prerelease: Option<bool>,
    pub make_latest: Option<LatestSelection>,
    /// The draft state the edit was prepared for. Publishing runs tag checks that a text edit
    /// skips, so the write refuses the edit when the stored state no longer matches.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_draft: Option<bool>,
    /// Publishing a draft whose tag is missing creates the tag, which this refuses. It is the
    /// caller's capability, so it is never stored.
    #[serde(skip)]
    pub new_tag_denied: bool,
}

impl From<&ReleaseFields> for ReleaseChanges {
    fn from(fields: &ReleaseFields) -> Self {
        Self {
            expected_revision: None,
            tag: None,
            target: None,
            name: Some(fields.name.clone()),
            body: Some(fields.body.clone()),
            is_draft: Some(fields.is_draft),
            is_prerelease: Some(fields.is_prerelease),
            make_latest: Some(fields.make_latest),
            expected_draft: None,
            new_tag_denied: false,
        }
    }
}

impl ReleaseChanges {
    pub fn apply_to(&self, current: &Release) -> ReleaseFields {
        let is_draft = self.is_draft.unwrap_or(current.is_draft);
        ReleaseFields {
            name: self.name.clone().unwrap_or_else(|| current.name.clone()),
            body: self.body.clone().unwrap_or_else(|| current.body.clone()),
            is_draft,
            is_prerelease: self.is_prerelease.unwrap_or(current.is_prerelease),
            // Publishing a draft makes it the latest release unless the edit says otherwise.
            make_latest: self.make_latest.unwrap_or_else(|| {
                (current.is_latest
                    || (current.is_draft && !is_draft)
                    || (!is_draft && current.is_prerelease && self.is_prerelease == Some(false)))
                .into()
            }),
        }
    }
}

/// A native edit checks the fields it displayed under the repository write lock.
pub fn release_revision(release: &Release) -> String {
    use sha2::{Digest, Sha256};
    let fields = serde_json::to_vec(&(
        release.id,
        &release.tag_name,
        &release.target_commitish,
        &release.name,
        &release.body,
        release.is_draft,
        release.is_immutable,
        release.is_prerelease,
        release.is_latest,
    ))
    .expect("release fields serialize");
    format!("{:x}", Sha256::digest(fields))
}

native_operation!(crate::postgres::releases::create_release,
    crate::sqlite::releases::create_release,
    fn create_release(pool: &DbPool, actor_id: i64, repo_id: i64, tag: &str, target: &str, fields: &ReleaseFields) -> Result<Release>);

native_operation!(crate::postgres::releases::update_release,
    crate::sqlite::releases::update_release,
    fn update_release(pool: &DbPool, actor_id: i64, repo_id: i64, release_id: i64, fields: &ReleaseChanges) -> Result<(Release, bool)>);

native_operation!(crate::postgres::releases::create_release_with_publication,
    crate::sqlite::releases::create_release_with_publication,
    fn create_release_with_publication(pool: &DbPool, actor_id: i64, repo_id: i64, tag: &str, target: &str, fields: &ReleaseFields,publication:Option<&crate::repositories::immutable_releases::Publication>) -> Result<Release>);

native_operation!(crate::postgres::releases::update_release_with_publication,
    crate::sqlite::releases::update_release_with_publication,
    fn update_release_with_publication(pool: &DbPool, actor_id: i64, repo_id: i64, release_id: i64, fields: &ReleaseChanges,publication:Option<&crate::repositories::immutable_releases::Publication>) -> Result<(Release, bool)>);

// Removes the release, unused file metadata and queues object cleanup atomically.
// Returns the removed files for an immediate best-effort object delete.
native_operation!(crate::postgres::releases::delete_release,
    crate::sqlite::releases::delete_release,
    fn delete_release(pool: &DbPool, actor_id: i64, repo_id: i64, release_id: i64) -> Result<Vec<File>>);

pub async fn add_release_asset(
    pool: &DbPool,
    actor: i64,
    credential: &codeyard_db_core::queries::users::accounts::AccountCredential<'_>,
    repo: i64,
    release: i64,
    asset: &NewReleaseAsset<'_>,
) -> Result<ReleaseAsset> {
    add_release_asset_with_token(pool, actor, credential, repo, release, asset, None).await
}

native_operation!(crate::postgres::releases::add_release_asset,
    crate::sqlite::releases::add_release_asset,
    fn add_release_asset_with_token(pool: &DbPool, actor_id: i64, credential: &codeyard_db_core::queries::users::accounts::AccountCredential<'_>, repo_id: i64, release_id: i64, asset: &NewReleaseAsset<'_>, token: Option<i64>) -> Result<ReleaseAsset>);

// Removes an asset and its unused file, with durable object cleanup in the same transaction.
native_operation!(crate::postgres::releases::delete_release_asset,
    crate::sqlite::releases::delete_release_asset,
    fn delete_release_asset(pool: &DbPool, actor_id: i64, repo_id: i64, release_id: i64, asset_id: i64) -> Result<Option<File>>);

native_operation!(crate::postgres::releases::get_release_by_tag,
    crate::sqlite::releases::get_release_by_tag,
    fn get_release_by_tag(pool: &DbPool, repo_id: i64, tag: &str) -> Result<Option<ReleaseRow>>);

native_operation!(crate::postgres::releases::get_release,
    crate::sqlite::releases::get_release,
    fn get_release(pool: &DbPool, repo_id: i64, release_id: i64) -> Result<Option<ReleaseRow>>);

/// The discussion category a release asked for, and the number of the discussion it started.
#[derive(Clone, Copy, Debug, sqlx::FromRow)]
pub struct ReleaseDiscussion {
    pub category_id: i64,
    pub number: Option<i32>,
}

native_operation!(crate::postgres::releases::release_discussion,
    crate::sqlite::releases::release_discussion,
    fn release_discussion(pool: &DbPool, release_id: i64) -> Result<Option<ReleaseDiscussion>>);

// A release keeps the category until it has started its discussion.
native_operation!(crate::postgres::releases::set_release_discussion_category,
    crate::sqlite::releases::set_release_discussion_category,
    fn set_release_discussion_category(pool: &DbPool, release_id: i64, category_id: i64) -> Result<()>);

// Links the discussion a release started; false when another request linked one first.
native_operation!(crate::postgres::releases::link_release_discussion,
    crate::sqlite::releases::link_release_discussion,
    fn link_release_discussion(pool: &DbPool, release_id: i64, discussion_id: i64) -> Result<bool>);

// Newest first. Drafts are left out unless `include_drafts` is set.
#[derive(Clone, Copy)]
pub enum ReleaseOrder {
    Published,
    CreatedAsc,
    CreatedDesc,
    NameAsc,
    NameDesc,
}
impl ReleaseOrder {
    pub fn sql(self) -> &'static str {
        match self {
            Self::Published => "COALESCE(r.published_at,r.created_at) DESC,r.id DESC",
            Self::CreatedAsc => "r.created_at,r.id",
            Self::CreatedDesc => "r.created_at DESC,r.id DESC",
            Self::NameAsc => "LOWER(COALESCE(r.name,'')),r.id",
            Self::NameDesc => "LOWER(COALESCE(r.name,'')) DESC,r.id DESC",
        }
    }
}
native_operation!(crate::postgres::releases::list_releases_ordered,
    crate::sqlite::releases::list_releases_ordered,
    fn list_releases_ordered(pool:&DbPool,repo_id:i64,include_drafts:bool,order:ReleaseOrder,limit:i64,offset:i64)->Result<Vec<ReleaseRow>>);

native_operation!(crate::postgres::releases::list_releases,
    crate::sqlite::releases::list_releases,
    fn list_releases(pool: &DbPool, repo_id: i64, include_drafts: bool, limit: i64, offset: i64) -> Result<Vec<ReleaseRow>>);

native_operation!(crate::postgres::releases::count_releases,
    crate::sqlite::releases::count_releases,
    fn count_releases(pool: &DbPool, repo_id: i64, include_drafts: bool) -> Result<i64>);

// GraphQL orders by creation, independently of when a draft was published.
native_operation!(crate::postgres::releases::list_releases_by_creation,
    crate::sqlite::releases::list_releases_by_creation,
    fn list_releases_by_creation(pool: &DbPool, repo_id: i64, include_drafts: bool, ascending: bool, limit: i64, offset: i64) -> Result<Vec<ReleaseRow>>);

// The release marked latest, when it is published.
native_operation!(crate::postgres::releases::latest_release,
    crate::sqlite::releases::latest_release,
    fn latest_release(pool: &DbPool, repo_id: i64) -> Result<Option<Release>>);

#[derive(sqlx::FromRow)]
pub struct ReleaseAssetFile {
    #[sqlx(flatten)]
    pub asset: ReleaseAsset,
    pub sha256_hex: String,
    pub uploader_id: Option<i64>,
    pub uploader_name: Option<String>,
    pub uploader_is_admin: Option<bool>,
}

native_operation!(crate::postgres::releases::list_asset_files,
    crate::sqlite::releases::list_asset_files,
    fn list_asset_files(pool: &DbPool, release_id: i64) -> Result<Vec<ReleaseAssetFile>>);

native_operation!(crate::postgres::releases::list_release_assets,
    crate::sqlite::releases::list_release_assets,
    fn list_release_assets(pool: &DbPool, release_id: i64) -> Result<Vec<ReleaseAsset>>);

native_operation!(crate::postgres::releases::get_repository_release_asset,
    crate::sqlite::releases::get_repository_release_asset,
    fn get_repository_release_asset(pool: &DbPool, repo_id: i64, asset_id: i64) -> Result<Option<ReleaseAsset>>);

native_operation!(crate::postgres::releases::download_asset,
    crate::sqlite::releases::download_asset,
    fn download_asset(pool: &DbPool, viewer: Option<i64>, repo: i64, tag: Option<&str>, name: Option<&str>, asset: Option<i64>) -> Result<Option<ReleaseAsset>>);

native_operation!(crate::postgres::releases::get_release_asset_by_name,
    crate::sqlite::releases::get_release_asset_by_name,
    fn get_release_asset_by_name(pool: &DbPool, release_id: i64, name: &str) -> Result<Option<ReleaseAsset>>);

native_operation!(crate::postgres::releases::list_releases_assets,
    crate::sqlite::releases::list_releases_assets,
    fn list_releases_assets(pool: &DbPool, release_ids: &[i64]) -> Result<Vec<ReleaseAsset>>);

native_operation!(crate::postgres::releases::count_asset_download,
    crate::sqlite::releases::count_asset_download,
    fn count_asset_download(pool: &DbPool, asset_id: i64) -> Result<()>);
