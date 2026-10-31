//! Run credentials, immutable cache entries and run artifacts.
use super::DbPool;
use anyhow::Result;
use serde::{Deserialize, Serialize};

pub(crate) const MAX_CACHE_ENTRIES: usize = 1024;
/// The site, repository and organization cache settings: size and retention days for each.
pub(crate) type CacheSettings = (Option<i32>, Option<i32>, Option<i32>, Option<i32>, Option<i32>, Option<i32>);

#[derive(Clone, sqlx::FromRow)]
pub struct RuntimeRun {
    pub id: i64,
    pub repo_id: i64,
    pub backend_id: String,
    pub branch: String,
    /// A pull request run also restores caches from the branch it targets.
    pub base_branch: Option<String>,
    pub default_branch: String,
    pub is_fork: bool,
}

#[derive(Clone, sqlx::FromRow)]
pub struct ActionObject {
    pub id: i64,
    pub repo_id: i64,
    pub run_id: Option<i64>,
    pub kind: String,
    pub scope: String,
    pub name: String,
    pub version: String,
    pub size: i64,
    pub finalized: bool,
    pub manifest: Option<String>,
    pub created_at: i64,
    pub accessed_at: i64,
    pub expires_at: i64,
}

#[derive(sqlx::FromRow)]
pub struct ArtifactSummary {
    pub id: i64,
    pub name: String,
    pub size: i64,
    pub created_at: i64,
}

#[derive(Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct BlobPart {
    pub block_id: String,
    pub object_key: String,
    pub size: i64,
}

#[derive(Clone, Copy)]
pub enum ObjectOrder {
    Created,
    Accessed,
    Size,
}

pub struct ObjectQuery<'a> {
    pub repo_id: i64,
    pub run_id: Option<i64>,
    pub kind: &'a str,
    pub name: Option<&'a str>,
    pub scope: Option<&'a str>,
    pub order: ObjectOrder,
    pub ascending: bool,
    pub offset: i64,
    pub limit: i64,
}

pub struct ObjectPage {
    pub total: i64,
    pub objects: Vec<ActionObject>,
}

#[derive(sqlx::FromRow)]
pub struct RepositoryCacheUsage {
    pub name: String,
    pub count: i64,
    pub bytes: i64,
}

pub struct OrganizationCacheUsage {
    pub total: i64,
    pub count: i64,
    pub bytes: i64,
    pub repositories: Vec<RepositoryCacheUsage>,
}

pub struct NewObject<'a> {
    pub run: &'a RuntimeRun,
    pub kind: &'a str,
    pub name: &'a str,
    pub version: &'a str,
    pub expires_at: i64,
}

/// Scope precedes restore-key order, and each key's exact match precedes its prefixes.
/// Choose only an ID before loading the winning manifest.
pub(crate) fn cache_lookup_sql(pg: bool) -> String {
    let keys = if pg { "jsonb_array_elements_text($7::jsonb) WITH ORDINALITY AS k(value,key)" } else { "json_each($7) k" };
    let sql = format!("SELECT o.id FROM actions_objects o JOIN {keys} ON substr(o.name,1,length(k.value))=k.value WHERE o.repo_id=$1 AND o.kind='cache' AND o.version=$2 AND o.finalized AND o.expires_at>$3 AND (o.scope=$4 OR o.scope=$5 OR o.scope=$6) ORDER BY CASE o.scope WHEN $4 THEN 0 WHEN $6 THEN 1 ELSE 2 END,CAST(k.key AS BIGINT),CASE WHEN o.name=k.value THEN 0 ELSE 1 END,o.created_at DESC,o.id DESC LIMIT 1");
    if pg { sql } else { sql.replace('$', "?") }
}

native_operation!(crate::backend::db::postgres::action_objects::issue_token,
    crate::backend::db::sqlite::action_objects::issue_token,
    fn issue_token(pool: &DbPool, run_id: i64, worker: &str, hash: &str, backend_id: &str) -> Result<()>);
native_operation!(crate::backend::db::postgres::action_objects::authenticate,
    crate::backend::db::sqlite::action_objects::authenticate,
    fn authenticate(pool: &DbPool, hash: &str) -> Result<Option<RuntimeRun>>);
native_operation!(crate::backend::db::postgres::action_objects::create_object,
    crate::backend::db::sqlite::action_objects::create_object,
    fn create_object(pool: &DbPool, new: NewObject<'_>) -> Result<Option<ActionObject>>);
native_operation!(crate::backend::db::postgres::action_objects::own_object,
    crate::backend::db::sqlite::action_objects::own_object,
    fn own_object(pool: &DbPool, run_id: i64, kind: &str, name: &str, version: &str) -> Result<Option<ActionObject>>);
native_operation!(crate::backend::db::postgres::action_objects::grant_blob,
    crate::backend::db::sqlite::action_objects::grant_blob,
    fn grant_blob(pool: &DbPool, object_id: i64, run_id: i64, hash: &str, write: bool) -> Result<bool>);
native_operation!(crate::backend::db::postgres::action_objects::blob_grant,
    crate::backend::db::sqlite::action_objects::blob_grant,
    fn blob_grant(pool: &DbPool, object_id: i64, hash: &str, write: bool) -> Result<Option<ActionObject>>);
native_operation!(crate::backend::db::postgres::action_objects::stage_block,
    crate::backend::db::sqlite::action_objects::stage_block,
    fn stage_block(pool: &DbPool, object_id: i64, grant_hash: &str, part: &BlobPart, limit: i64) -> Result<bool>);
native_operation!(crate::backend::db::postgres::action_objects::ready_block,
    crate::backend::db::sqlite::action_objects::ready_block,
    fn ready_block(pool: &DbPool, object_id: i64, grant_hash: &str, key: &str) -> Result<bool>);
native_operation!(crate::backend::db::postgres::action_objects::reopen_blob,
    crate::backend::db::sqlite::action_objects::reopen_blob,
    fn reopen_blob(pool: &DbPool, object_id: i64, grant_hash: &str) -> Result<()>);
native_operation!(crate::backend::db::postgres::action_objects::seal_blob,
    crate::backend::db::sqlite::action_objects::seal_blob,
    fn seal_blob(pool: &DbPool, object_id: i64, grant_hash: &str, blocks: &[String]) -> Result<bool>);
native_operation!(crate::backend::db::postgres::action_objects::finalize_object,
    crate::backend::db::sqlite::action_objects::finalize_object,
    fn finalize_object(pool: &DbPool, run_id: i64, hash: &str, object_id: i64, size: i64, cache_limit: i64) -> Result<bool>);
native_operation!(crate::backend::db::postgres::action_objects::find_cache,
    crate::backend::db::sqlite::action_objects::find_cache,
    fn find_cache(pool: &DbPool, run: &RuntimeRun, keys: &[String], version: &str) -> Result<Option<ActionObject>>);
native_operation!(crate::backend::db::postgres::action_objects::list_artifacts,
    crate::backend::db::sqlite::action_objects::list_artifacts,
    fn list_artifacts(pool: &DbPool, run_id: i64, name: Option<&str>, id: Option<i64>) -> Result<Vec<ArtifactSummary>>);
native_operation!(crate::backend::db::postgres::action_objects::artifact_for_run, crate::backend::db::sqlite::action_objects::artifact_for_run,
    fn artifact_for_run(pool: &DbPool, run_id: i64, name: &str) -> Result<Option<ActionObject>>);
native_operation!(crate::backend::db::postgres::action_objects::artifact_for_reader,
    crate::backend::db::sqlite::action_objects::artifact_for_reader,
    fn artifact_for_reader(pool: &DbPool, actor: i64, session: &str, repo: i64, run: i64, id: i64) -> Result<Option<ActionObject>>);
native_operation!(crate::backend::db::postgres::action_objects::artifact_by_id,
    crate::backend::db::sqlite::action_objects::artifact_by_id,
    fn artifact_by_id(pool: &DbPool, id: i64) -> Result<Option<ActionObject>>);
native_operation!(crate::backend::db::postgres::action_objects::delete_artifact,
    crate::backend::db::sqlite::action_objects::delete_artifact,
    fn delete_artifact(pool: &DbPool, id: i64, run_id: i64, actor: Option<i64>, credential: Option<&super::ci::CiCredential>) -> Result<bool>);
native_operation!(crate::backend::db::postgres::organizations::cache::org_cache_usage,
    crate::backend::db::sqlite::organizations::cache::org_cache_usage,
    fn org_cache_usage(pool: &DbPool, actor: i64, name: &str, offset: i64, limit: i64) -> Result<super::organizations::OrganizationOutcome<OrganizationCacheUsage>>);
native_operation!(crate::backend::db::postgres::action_objects::query_objects,
    crate::backend::db::sqlite::action_objects::query_objects,
    fn query_objects(pool: &DbPool, query: &ObjectQuery<'_>) -> Result<ObjectPage>);
native_operation!(crate::backend::db::postgres::action_objects::object_by_id,
    crate::backend::db::sqlite::action_objects::object_by_id,
    fn object_by_id(pool: &DbPool, repo: i64, id: i64, kind: &str) -> Result<Option<ActionObject>>);
native_operation!(crate::backend::db::postgres::action_objects::delete_caches,
    crate::backend::db::sqlite::action_objects::delete_caches,
    fn delete_caches(pool: &DbPool, actor: i64, repo: i64, id: Option<i64>, key: Option<&str>, scope: Option<&str>) -> Result<Option<Vec<ActionObject>>>);
native_operation!(crate::backend::db::postgres::action_objects::cache_usage,
    crate::backend::db::sqlite::action_objects::cache_usage,
    fn cache_usage(pool: &DbPool, repo: i64) -> Result<(i64, i64)>);
native_operation!(crate::backend::db::postgres::action_objects::grant_artifact_download,
    crate::backend::db::sqlite::action_objects::grant_artifact_download,
    fn grant_artifact_download(pool: &DbPool, actor: impl Into<super::credentials::Actor>, repo: i64, id: i64, hash: &str) -> Result<bool>);
native_operation!(crate::backend::db::postgres::action_objects::expire_objects,
    crate::backend::db::sqlite::action_objects::expire_objects,
    fn expire_objects(pool: &DbPool) -> Result<()>);
