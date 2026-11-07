use anyhow::Result;
use codeyard_db_core::native_operation;
use codeyard_db_core::DbPool;
use serde::Deserialize;
use serde::Serialize;

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

pub(crate) struct Actor {
    pub id: Option<i64>,
    pub repo: Option<i64>,
    pub owner: Option<(&'static str, i64)>,
    pub selected: Option<i64>,
    pub classic: bool,
}

impl Actor {
    pub const ANONYMOUS: Self = Self {
        id: None,
        repo: None,
        owner: None,
        selected: None,
        classic: false,
    };

    pub fn session(id: i64) -> Self {
        Self {
            id: Some(id),
            repo: None,
            owner: None,
            selected: None,
            classic: false,
        }
    }

    pub fn for_owner(self, kind: &str, id: Option<i64>) -> Self {
        match self.owner {
            Some((owner_kind, owner)) if owner_kind != kind || Some(owner) != id => Self::ANONYMOUS,
            _ => self,
        }
    }

    /// Scope applies to grants, not the anonymous public-read baseline.
    pub fn scope_condition(&self) -> String {
        let selection = self.selected.map_or_else(|| "TRUE".to_owned(), |token| format!("EXISTS(SELECT 1 FROM fine_grained_token_repositories s WHERE s.token_id={token} AND s.repo_id=p.repo_id)"));
        let policy = if self.classic {
            "NOT EXISTS(SELECT 1 FROM organization_token_policies policy WHERE policy.org_id=p.owner_org_id AND NOT policy.classic_allowed)"
        } else {
            "TRUE"
        };
        format!("({selection} AND {policy})")
    }
}

// Resolve identity without retaining a credential before its account lock. The caller
// then uses the shared live-session check in the same package transaction.
pub(crate) fn session_account_query(now: &str) -> String {
    let proofs =
        codeyard_db_core::cells::authorization::session_relation(now == "clock_timestamp()");
    format!("SELECT u.id FROM users u WHERE NOT u.is_banned AND NOT u.must_change_password AND ((NOT u.is_stub AND EXISTS(SELECT 1 FROM sessions s WHERE s.user_id=u.id AND s.id=$1)) OR (u.is_stub AND EXISTS(SELECT 1 FROM {proofs} f WHERE f.user_id=u.id AND f.kind='session' AND f.credential_hash=$2 AND f.expires_at>{now}))) LIMIT 2")
}

pub(crate) fn read_condition(actor: &Actor, repository_access: &str) -> String {
    let access = repository_access.replace("CASE WHEN NOT r.is_private THEN 1 ELSE 0 END", "0");
    let scope = actor.scope_condition();
    format!("((p.visibility='public' AND (p.repo_id IS NULL OR NOT r.is_private)) OR ({scope} AND (p.owner_user_id=u.id OR (p.owner_kind='user' AND u.is_admin) OR EXISTS(SELECT 1 FROM org_memberships m WHERE m.org_id=p.owner_org_id AND m.user_id=u.id AND m.role IN ('owner','admin')) OR (p.repo_id IS NOT NULL AND r.owner_kind=p.owner_kind AND COALESCE(r.owner_user_id,r.owner_org_id)=COALESCE(p.owner_user_id,p.owner_org_id) AND ({access})>=1))))")
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

/// How storing a blob counts against the uploader's package upload quota.
#[derive(Clone, Copy)]
pub enum Charge<'a> {
    /// A whole upload.
    Upload,
    /// A chunk of the named OCI upload; the chunks of one upload count as one upload.
    Chunk(&'a str),
    /// A blob assembled from chunks that were already charged.
    Assembled,
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

native_operation!(crate::postgres::packages::put_maven_file,
    crate::sqlite::packages::put_maven_file,
    fn put_maven_file(pool: &DbPool, token: i64, file: MavenFile<'_>) -> Result<()>);
native_operation!(crate::postgres::packages::maven_file,
    crate::sqlite::packages::maven_file,
    fn maven_file(pool: &DbPool, repo: i64, token: Option<Credential<'_>>, path: &str) -> Result<(Package, Blob)>);

native_operation!(crate::postgres::packages::container_namespace,
    crate::sqlite::packages::container_namespace,
    fn container_namespace(pool: &DbPool, owner: &Owner) -> Result<String>);
native_operation!(crate::postgres::packages::container_owner,
    crate::sqlite::packages::container_owner,
    fn container_owner(pool: &DbPool, namespace: &str) -> Result<Option<Owner>>);

/// Registry listings read up to 100 versions' metadata at a time, so publication bounds the
/// part they read: all of it for npm and RubyGems, and all but the stored .nuspec for NuGet.
pub const MAX_LISTED_METADATA_BYTES: usize = 64 * 1024;

pub(crate) fn listed_metadata_fits(package_type: &str, manifest: &str) -> bool {
    match package_type {
        "npm" | "rubygems" => manifest.len() <= MAX_LISTED_METADATA_BYTES,
        "nuget" => serde_json::from_str::<serde_json::Value>(manifest)
            .ok()
            .and_then(|mut catalog| {
                catalog.as_object_mut()?.remove("nuspec");
                Some(catalog.to_string().len() <= MAX_LISTED_METADATA_BYTES)
            })
            .unwrap_or(false),
        _ => true,
    }
}
pub const MAX_METADATA_RESPONSE_BYTES: usize = 8 * 1024 * 1024;

native_operation!(crate::postgres::packages::owner_page, crate::sqlite::packages::owner_page,
    fn owner_page(pool: &DbPool, token: Option<Credential<'_>>, owner: &Owner, kind: &str, before: i64) -> Result<Vec<Package>>);
native_operation!(crate::postgres::packages::version_page,
    crate::sqlite::packages::version_page,
    fn version_page(pool: &DbPool, package: i64, token: Option<Credential<'_>>, before: i64, names_only: bool) -> Result<Vec<Version>>);
native_operation!(crate::postgres::packages::finish_upload,
    crate::sqlite::packages::finish_upload,
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

native_operation!(crate::postgres::packages::nuget_downloaded,
    crate::sqlite::packages::nuget_downloaded,
    fn nuget_downloaded(pool: &DbPool, package: i64, token: Option<Credential<'_>>, version: i64) -> Result<()>);

native_operation!(crate::postgres::packages::version_by_id,
    crate::sqlite::packages::version_by_id,
    fn version_by_id(pool: &DbPool, package: i64, token: Option<Credential<'_>>, id: i64) -> Result<Version>);
native_operation!(crate::postgres::packages::version_counts,
    crate::sqlite::packages::version_counts,
    fn version_counts(pool: &DbPool, ids: &[i64]) -> Result<Vec<(i64, i64)>>);
native_operation!(crate::postgres::packages::version_tags,
    crate::sqlite::packages::version_tags,
    fn version_tags(pool: &DbPool, ids: &[i64]) -> Result<Vec<(i64, String)>>);
native_operation!(crate::postgres::packages::version_count,
    crate::sqlite::packages::version_count,
    fn version_count(pool: &DbPool, package: i64, token: Option<Credential<'_>>) -> Result<i64>);

native_operation!(crate::postgres::packages::issue_registry_token,
    crate::sqlite::packages::issue_registry_token,
    fn issue_registry_token(pool: &DbPool, hash: &str, token: Option<i64>, resources: &str, write: bool) -> Result<()>);
native_operation!(crate::postgres::packages::registry_token,
    crate::sqlite::packages::registry_token,
    fn registry_token(pool: &DbPool, hash: &str) -> Result<Option<RegistryToken>>);
native_operation!(crate::postgres::packages::registry_source,
    crate::sqlite::packages::registry_source,
    fn registry_source(pool: &DbPool, id: i64) -> Result<Option<codeyard_db_core::models::PersonalAccessToken>>);

pub fn now() -> i64 {
    chrono::Utc::now().timestamp()
}

pub fn object_key(digest: &str) -> String {
    format!("packages/sha256/{}", digest.trim_start_matches("sha256:"))
}

native_operation!(crate::postgres::packages::owner,
    crate::sqlite::packages::owner,
    fn owner(pool: &DbPool, name: &str, kind: Option<&str>) -> Result<Option<Owner>>);
native_operation!(crate::postgres::packages::find,
    crate::sqlite::packages::find,
    fn find(pool: &DbPool, owner: &Owner, kind: &str, name: &str, token: Option<Credential<'_>>, permission: Permission, deleted: bool) -> Result<Package>);
native_operation!(crate::postgres::packages::list,
    crate::sqlite::packages::list,
    fn list(pool: &DbPool, token: Option<Credential<'_>>, query: Listing<'_>) -> Result<Vec<Package>>);
native_operation!(crate::postgres::packages::count_linked,
    crate::sqlite::packages::count_linked,
    fn count_linked(pool: &DbPool, token: Option<Credential<'_>>, repo: i64) -> Result<i64>);
native_operation!(crate::postgres::packages::create,
    crate::sqlite::packages::create,
    fn create(pool: &DbPool, owner: &Owner, kind: &str, name: &str, token: i64, repo: Option<i64>) -> Result<Package>);
// A page of versions without their metadata (`manifest` is `{}`); read one version for it.
native_operation!(crate::postgres::packages::versions,
    crate::sqlite::packages::versions,
    fn versions(pool: &DbPool, package: i64, token: Option<Credential<'_>>, deleted: bool, offset: i64, limit: i64) -> Result<Vec<Version>>);
native_operation!(crate::postgres::packages::version,
    crate::sqlite::packages::version,
    fn version(pool: &DbPool, package: i64, token: Option<Credential<'_>>, reference: &str) -> Result<Version>);
native_operation!(crate::postgres::packages::rubygems_file,
    crate::sqlite::packages::rubygems_file,
    fn rubygems_file(pool: &DbPool, owner: &Owner, token: Option<Credential<'_>>, filename: &str) -> Result<Blob>);
native_operation!(crate::postgres::packages::tags,
    crate::sqlite::packages::tags,
    fn tags(pool: &DbPool, package: i64, token: Option<Credential<'_>>, version: i64) -> Result<Vec<String>>);
native_operation!(crate::postgres::packages::tag_page,
    crate::sqlite::packages::tag_page,
    fn tag_page(pool: &DbPool, package: i64, token: Option<Credential<'_>>, after: &str, limit: i64) -> Result<Vec<(String, String)>>);
native_operation!(crate::postgres::packages::set_tag,
    crate::sqlite::packages::set_tag,
    fn set_tag(pool: &DbPool, package: i64, token: i64, tag: &str, version: &str) -> Result<()>);
native_operation!(crate::postgres::packages::delete_tag,
    crate::sqlite::packages::delete_tag,
    fn delete_tag(pool: &DbPool, package: i64, token: i64, tag: &str) -> Result<()>);
native_operation!(crate::postgres::packages::publish,
    crate::sqlite::packages::publish,
    fn publish(pool: &DbPool, token: i64, new: Publish<'_>) -> Result<Version>);
native_operation!(crate::postgres::packages::set_deleted,
    crate::sqlite::packages::set_deleted,
    fn set_deleted(pool: &DbPool, package: i64, version: Option<i64>, token: Credential<'_>, restore: bool) -> Result<()>);
native_operation!(crate::postgres::packages::settings,
    crate::sqlite::packages::settings,
    fn settings(pool: &DbPool, package: i64, token: Credential<'_>, visibility: &str, repo: Option<i64>) -> Result<()>);
native_operation!(crate::postgres::packages::reserve_blob,
    crate::sqlite::packages::reserve_blob,
    fn reserve_blob(pool: &DbPool, package: i64, token: i64, blob: &Blob, charge: Charge<'_>) -> Result<bool>);
native_operation!(crate::postgres::packages::ready_blob,
    crate::sqlite::packages::ready_blob,
    fn ready_blob(pool: &DbPool, package: i64, token: i64, blob: &Blob) -> Result<()>);
native_operation!(crate::postgres::packages::blob,
    crate::sqlite::packages::blob,
    fn blob(pool: &DbPool, package: i64, token: Option<Credential<'_>>, digest: &str) -> Result<Blob>);
native_operation!(crate::postgres::packages::start_upload,
    crate::sqlite::packages::start_upload,
    fn start_upload(pool: &DbPool, package: i64, token: i64, id: &str) -> Result<Upload>);
native_operation!(crate::postgres::packages::upload,
    crate::sqlite::packages::upload,
    fn upload(pool: &DbPool, package: i64, token: i64, id: &str) -> Result<Upload>);
native_operation!(crate::postgres::packages::append_upload,
    crate::sqlite::packages::append_upload,
    fn append_upload(pool: &DbPool, package: i64, token: i64, id: &str, offset: i64, chunk: &Blob) -> Result<Upload>);
native_operation!(crate::postgres::packages::cancel_upload,
    crate::sqlite::packages::cancel_upload,
    fn cancel_upload(pool: &DbPool, package: i64, token: i64, id: &str) -> Result<()>);
native_operation!(crate::postgres::packages::garbage,
    crate::sqlite::packages::garbage,
    fn garbage(pool: &DbPool) -> Result<Vec<String>>);
/// Held while this node collects package garbage. Collectors on several nodes would act on the
/// same selection, and a late one could delete an object that was published again meanwhile.
/// Dropping it closes the connection that holds the PostgreSQL lock, which releases it.
pub struct CollectorLock {
    _connection: Option<sqlx::PgConnection>,
}

/// Takes the collector lock, or returns `None` when another node is collecting. A SQLite
/// installation has a single server, so its lock is always free.
pub async fn collector_lock(pool: &DbPool) -> Result<Option<CollectorLock>> {
    match pool {
        DbPool::Sqlite(_) => Ok(Some(CollectorLock { _connection: None })),
        DbPool::Postgres(pool) => {
            Ok(crate::postgres::packages::collector_lock(pool)
                .await?
                .map(|connection| CollectorLock {
                    _connection: Some(connection),
                }))
        }
    }
}

native_operation!(crate::postgres::packages::removed_blob,
    crate::sqlite::packages::removed_blob,
    fn removed_blob(pool: &DbPool, digest: &str) -> Result<()>);
