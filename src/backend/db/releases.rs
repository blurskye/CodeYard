//! Releases and their assets. Writers create and change them; readers see published releases,
//! and writers also see drafts (the caller decides which list it asks for).
use crate::backend::db::models::{Release, ReleaseAsset, ReleaseRow};
use crate::backend::db::DbPool;
use anyhow::Result;
use uuid::Uuid;

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
}

/// An uploaded file being attached to a release, with the details the asset row keeps.
#[derive(Debug, Clone)]
pub struct NewReleaseAsset<'a> {
    pub file_id: Uuid,
    pub name: &'a str,
    pub label: Option<&'a str>,
    pub content_type: &'a str,
    pub size: i64,
}

/// The fields a writer sets on a release. The tag and its target are fixed once the release exists.
#[derive(Debug, Clone, Default)]
pub struct ReleaseFields {
    pub name: Option<String>,
    pub body: Option<String>,
    pub is_draft: bool,
    pub is_prerelease: bool,
    /// Asks for the latest release. Drafts and pre-releases never are, and a new latest release
    /// takes the flag from the one that had it.
    pub make_latest: bool,
}

native_operation!(crate::backend::db::postgres::releases::create_release,
    crate::backend::db::sqlite::releases::create_release,
    fn create_release(pool: &DbPool, actor_id: i64, repo_id: i64, tag: &str, target: &str, fields: &ReleaseFields) -> Result<Release>);

native_operation!(crate::backend::db::postgres::releases::update_release,
    crate::backend::db::sqlite::releases::update_release,
    fn update_release(pool: &DbPool, actor_id: i64, repo_id: i64, release_id: i64, fields: &ReleaseFields) -> Result<Release>);

// Deletes the release and its asset rows. Returns the files the assets used, for cleanup.
native_operation!(crate::backend::db::postgres::releases::delete_release,
    crate::backend::db::sqlite::releases::delete_release,
    fn delete_release(pool: &DbPool, actor_id: i64, repo_id: i64, release_id: i64) -> Result<Vec<Uuid>>);

native_operation!(crate::backend::db::postgres::releases::add_release_asset,
    crate::backend::db::sqlite::releases::add_release_asset,
    fn add_release_asset(pool: &DbPool, actor_id: i64, repo_id: i64, release_id: i64, asset: &NewReleaseAsset<'_>) -> Result<ReleaseAsset>);

// Deletes one asset row. Returns the file it used, for cleanup.
native_operation!(crate::backend::db::postgres::releases::delete_release_asset,
    crate::backend::db::sqlite::releases::delete_release_asset,
    fn delete_release_asset(pool: &DbPool, actor_id: i64, repo_id: i64, release_id: i64, asset_id: i64) -> Result<Uuid>);

native_operation!(crate::backend::db::postgres::releases::get_release_by_tag,
    crate::backend::db::sqlite::releases::get_release_by_tag,
    fn get_release_by_tag(pool: &DbPool, repo_id: i64, tag: &str) -> Result<Option<ReleaseRow>>);

native_operation!(crate::backend::db::postgres::releases::get_release,
    crate::backend::db::sqlite::releases::get_release,
    fn get_release(pool: &DbPool, repo_id: i64, release_id: i64) -> Result<Option<ReleaseRow>>);

// Newest first. Drafts are left out unless `include_drafts` is set.
native_operation!(crate::backend::db::postgres::releases::list_releases,
    crate::backend::db::sqlite::releases::list_releases,
    fn list_releases(pool: &DbPool, repo_id: i64, include_drafts: bool, limit: i64, offset: i64) -> Result<Vec<ReleaseRow>>);

native_operation!(crate::backend::db::postgres::releases::count_releases,
    crate::backend::db::sqlite::releases::count_releases,
    fn count_releases(pool: &DbPool, repo_id: i64, include_drafts: bool) -> Result<i64>);

// The release marked latest, when it is published.
native_operation!(crate::backend::db::postgres::releases::latest_release,
    crate::backend::db::sqlite::releases::latest_release,
    fn latest_release(pool: &DbPool, repo_id: i64) -> Result<Option<Release>>);

native_operation!(crate::backend::db::postgres::releases::list_release_assets,
    crate::backend::db::sqlite::releases::list_release_assets,
    fn list_release_assets(pool: &DbPool, release_id: i64) -> Result<Vec<ReleaseAsset>>);

native_operation!(crate::backend::db::postgres::releases::count_asset_download,
    crate::backend::db::sqlite::releases::count_asset_download,
    fn count_asset_download(pool: &DbPool, asset_id: i64) -> Result<()>);
