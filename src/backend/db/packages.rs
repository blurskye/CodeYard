//! Package records shared by the registries and GitHub REST.
use super::DbPool;
use anyhow::Result;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, sqlx::FromRow)]
pub struct Owner {
    pub id: i64,
    pub name: String,
    pub kind: String,
}

#[derive(Clone, Debug, sqlx::FromRow)]
pub struct Package {
    pub id: i64,
    pub owner_kind: String,
    pub owner_user_id: Option<i64>,
    pub owner_org_id: Option<i64>,
    pub name: String,
    pub package_type: String,
    pub visibility: String,
    pub repo_id: Option<i64>,
    pub created_at: i64,
    pub updated_at: i64,
    pub deleted_at: Option<i64>,
}

#[derive(Clone, Debug, sqlx::FromRow)]
pub struct Version {
    pub id: i64,
    pub package_id: i64,
    pub name: String,
    pub digest: String,
    pub media_type: String,
    pub manifest: String,
    pub downloads: i64,
    pub created_at: i64,
    pub updated_at: i64,
    pub deleted_at: Option<i64>,
}

#[derive(Clone, Debug, Serialize, Deserialize, sqlx::FromRow)]
pub struct Blob {
    pub digest: String,
    pub size: i64,
}

#[derive(sqlx::FromRow)]
pub struct RegistryToken {
    pub token_id: Option<i64>,
    pub resources: String,
    pub write: bool,
    pub expires_at: i64,
}

#[derive(sqlx::FromRow)]
pub struct Upload {
    pub id: String,
    pub package_id: i64,
    pub token_id: i64,
    #[sqlx(rename = "uploaded_bytes")]
    pub offset: i64,
    pub chunks: String,
    pub expires_at: i64,
}

pub struct Listing<'a> {
    pub owner: Option<&'a Owner>,
    pub repo: Option<i64>,
    pub kind: Option<&'a str>,
    pub visibility: Option<&'a str>,
    pub offset: i64,
    pub limit: i64,
}

pub struct Publish<'a> {
    pub package_id: i64,
    pub name: &'a str,
    pub digest: &'a str,
    pub media_type: &'a str,
    pub manifest: &'a str,
    pub tags: &'a [String],
    pub blobs: &'a [Blob],
}

#[derive(Clone, Copy)]
pub enum Credential<'a> {
    Token(i64),
    Session(&'a str),
}

impl From<i64> for Credential<'_> {
    fn from(id: i64) -> Self {
        Self::Token(id)
    }
}

pub struct MavenFile<'a> {
    pub package_id: i64,
    pub repo_id: i64,
    pub path: &'a str,
    pub version: Option<&'a str>,
    pub metadata: &'a str,
    pub mutable: bool,
    pub blob: &'a Blob,
}

native_operation!(crate::backend::db::postgres::packages::put_maven_file,
    crate::backend::db::sqlite::packages::put_maven_file,
    fn put_maven_file(pool: &DbPool, token: i64, file: MavenFile<'_>) -> Result<()>);
native_operation!(crate::backend::db::postgres::packages::maven_file,
    crate::backend::db::sqlite::packages::maven_file,
    fn maven_file(pool: &DbPool, repo: i64, token: Option<Credential<'_>>, path: &str) -> Result<(Package, Blob)>);

pub const MAX_NPM_METADATA_BYTES: usize = 64 * 1024;
pub const MAX_METADATA_RESPONSE_BYTES: usize = 8 * 1024 * 1024;

native_operation!(crate::backend::db::postgres::packages::owner_page, crate::backend::db::sqlite::packages::owner_page,
    fn owner_page(pool: &DbPool, token: Option<Credential<'_>>, owner: &Owner, kind: &str, before: i64) -> Result<Vec<Package>>);
native_operation!(crate::backend::db::postgres::packages::version_page,
    crate::backend::db::sqlite::packages::version_page,
    fn version_page(pool: &DbPool, package: i64, token: Option<Credential<'_>>, before: i64, names_only: bool) -> Result<Vec<Version>>);
native_operation!(crate::backend::db::postgres::packages::finish_upload,
    crate::backend::db::sqlite::packages::finish_upload,
    fn finish_upload(pool: &DbPool, package: i64, token: i64, id: &str, offset: i64) -> Result<()>);

#[derive(Clone, Copy)]
pub enum Permission {
    Read,
    Write,
    Admin,
}

#[derive(Debug, thiserror::Error)]
pub enum Refusal {
    #[error("Not Found")]
    Missing,
    #[error("Requires authentication")]
    Unauthorized,
    #[error("Resource not accessible by token")]
    Forbidden,
    #[error("Package already exists or cannot be restored")]
    Conflict,
    #[error("Blob is missing or has the wrong size")]
    Blob,
    #[error("Upload offset does not match")]
    Offset,
}

native_operation!(crate::backend::db::postgres::packages::nuget_downloaded,
    crate::backend::db::sqlite::packages::nuget_downloaded,
    fn nuget_downloaded(pool: &DbPool, package: i64, token: Option<Credential<'_>>, version: i64) -> Result<()>);

native_operation!(crate::backend::db::postgres::packages::version_by_id,
    crate::backend::db::sqlite::packages::version_by_id,
    fn version_by_id(pool: &DbPool, package: i64, token: Option<Credential<'_>>, id: i64) -> Result<Version>);
native_operation!(crate::backend::db::postgres::packages::version_count,
    crate::backend::db::sqlite::packages::version_count,
    fn version_count(pool: &DbPool, package: i64, token: Option<Credential<'_>>) -> Result<i64>);

native_operation!(crate::backend::db::postgres::packages::issue_registry_token,
    crate::backend::db::sqlite::packages::issue_registry_token,
    fn issue_registry_token(pool: &DbPool, hash: &str, token: Option<i64>, resources: &str, write: bool) -> Result<()>);
native_operation!(crate::backend::db::postgres::packages::registry_token,
    crate::backend::db::sqlite::packages::registry_token,
    fn registry_token(pool: &DbPool, hash: &str) -> Result<Option<RegistryToken>>);
native_operation!(crate::backend::db::postgres::packages::registry_source,
    crate::backend::db::sqlite::packages::registry_source,
    fn registry_source(pool: &DbPool, id: i64) -> Result<Option<super::models::PersonalAccessToken>>);

pub fn now() -> i64 {
    chrono::Utc::now().timestamp()
}

pub fn object_key(digest: &str) -> String {
    format!("packages/sha256/{}", digest.trim_start_matches("sha256:"))
}

native_operation!(crate::backend::db::postgres::packages::owner,
    crate::backend::db::sqlite::packages::owner,
    fn owner(pool: &DbPool, name: &str, kind: Option<&str>) -> Result<Option<Owner>>);
native_operation!(crate::backend::db::postgres::packages::find,
    crate::backend::db::sqlite::packages::find,
    fn find(pool: &DbPool, owner: &Owner, kind: &str, name: &str, token: Option<Credential<'_>>, permission: Permission, deleted: bool) -> Result<Package>);
native_operation!(crate::backend::db::postgres::packages::list,
    crate::backend::db::sqlite::packages::list,
    fn list(pool: &DbPool, token: Option<Credential<'_>>, query: Listing<'_>) -> Result<Vec<Package>>);
native_operation!(crate::backend::db::postgres::packages::count_linked,
    crate::backend::db::sqlite::packages::count_linked,
    fn count_linked(pool: &DbPool, token: Option<Credential<'_>>, repo: i64) -> Result<i64>);
native_operation!(crate::backend::db::postgres::packages::create,
    crate::backend::db::sqlite::packages::create,
    fn create(pool: &DbPool, owner: &Owner, kind: &str, name: &str, token: i64, repo: Option<i64>) -> Result<Package>);
native_operation!(crate::backend::db::postgres::packages::versions,
    crate::backend::db::sqlite::packages::versions,
    fn versions(pool: &DbPool, package: i64, token: Option<Credential<'_>>, deleted: bool, offset: i64, limit: i64) -> Result<Vec<Version>>);
native_operation!(crate::backend::db::postgres::packages::version,
    crate::backend::db::sqlite::packages::version,
    fn version(pool: &DbPool, package: i64, token: Option<Credential<'_>>, reference: &str) -> Result<Version>);
native_operation!(crate::backend::db::postgres::packages::rubygems_file,
    crate::backend::db::sqlite::packages::rubygems_file,
    fn rubygems_file(pool: &DbPool, owner: &Owner, token: Option<Credential<'_>>, filename: &str) -> Result<Blob>);
native_operation!(crate::backend::db::postgres::packages::tags,
    crate::backend::db::sqlite::packages::tags,
    fn tags(pool: &DbPool, package: i64, token: Option<Credential<'_>>, version: i64) -> Result<Vec<String>>);
native_operation!(crate::backend::db::postgres::packages::tag_page,
    crate::backend::db::sqlite::packages::tag_page,
    fn tag_page(pool: &DbPool, package: i64, token: Option<Credential<'_>>, after: &str, limit: i64) -> Result<Vec<String>>);
native_operation!(crate::backend::db::postgres::packages::publish,
    crate::backend::db::sqlite::packages::publish,
    fn publish(pool: &DbPool, token: i64, new: Publish<'_>) -> Result<Version>);
native_operation!(crate::backend::db::postgres::packages::set_deleted,
    crate::backend::db::sqlite::packages::set_deleted,
    fn set_deleted(pool: &DbPool, package: i64, version: Option<i64>, token: Credential<'_>, restore: bool) -> Result<()>);
native_operation!(crate::backend::db::postgres::packages::settings,
    crate::backend::db::sqlite::packages::settings,
    fn settings(pool: &DbPool, package: i64, token: Credential<'_>, visibility: &str, repo: Option<i64>) -> Result<()>);
native_operation!(crate::backend::db::postgres::packages::reserve_blob,
    crate::backend::db::sqlite::packages::reserve_blob,
    fn reserve_blob(pool: &DbPool, package: i64, token: i64, blob: &Blob) -> Result<bool>);
native_operation!(crate::backend::db::postgres::packages::ready_blob,
    crate::backend::db::sqlite::packages::ready_blob,
    fn ready_blob(pool: &DbPool, package: i64, token: i64, blob: &Blob) -> Result<()>);
native_operation!(crate::backend::db::postgres::packages::blob,
    crate::backend::db::sqlite::packages::blob,
    fn blob(pool: &DbPool, package: i64, token: Option<Credential<'_>>, digest: &str) -> Result<Blob>);
native_operation!(crate::backend::db::postgres::packages::start_upload,
    crate::backend::db::sqlite::packages::start_upload,
    fn start_upload(pool: &DbPool, package: i64, token: i64, id: &str) -> Result<Upload>);
native_operation!(crate::backend::db::postgres::packages::upload,
    crate::backend::db::sqlite::packages::upload,
    fn upload(pool: &DbPool, package: i64, token: i64, id: &str) -> Result<Upload>);
native_operation!(crate::backend::db::postgres::packages::append_upload,
    crate::backend::db::sqlite::packages::append_upload,
    fn append_upload(pool: &DbPool, package: i64, token: i64, id: &str, offset: i64, chunk: &Blob) -> Result<Upload>);
native_operation!(crate::backend::db::postgres::packages::cancel_upload,
    crate::backend::db::sqlite::packages::cancel_upload,
    fn cancel_upload(pool: &DbPool, package: i64, token: i64, id: &str) -> Result<()>);
native_operation!(crate::backend::db::postgres::packages::garbage,
    crate::backend::db::sqlite::packages::garbage,
    fn garbage(pool: &DbPool) -> Result<Vec<String>>);
/// Held while this node collects package garbage. Collectors on several nodes would act on the
/// same selection, and a late one could delete an object that was published again meanwhile.
/// Dropping it closes the connection that holds the PostgreSQL lock, which releases it.
pub struct CollectorLock(Option<sqlx::pool::PoolConnection<sqlx::Postgres>>);

impl Drop for CollectorLock {
    fn drop(&mut self) {
        if let Some(connection) = self.0.take() {
            drop(connection.detach());
        }
    }
}

/// Takes the collector lock, or returns `None` when another node is collecting. A SQLite
/// installation has a single server, so its lock is always free.
pub async fn collector_lock(pool: &DbPool) -> Result<Option<CollectorLock>> {
    match pool {
        DbPool::Sqlite(_) => Ok(Some(CollectorLock(None))),
        DbPool::Postgres(pool) => Ok(super::postgres::packages::collector_lock(pool)
            .await?
            .map(|connection| CollectorLock(Some(connection)))),
    }
}

native_operation!(crate::backend::db::postgres::packages::removed_blob,
    crate::backend::db::sqlite::packages::removed_blob,
    fn removed_blob(pool: &DbPool, digest: &str) -> Result<()>);
