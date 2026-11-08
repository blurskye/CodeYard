use crate::pull_requests::async_merge::AsyncMerge;
use crate::pull_requests::async_merge::NewAsyncMerge;
use crate::pull_requests::async_merge::PENDING_TIMEOUT_SECONDS;
use crate::pull_requests::async_merge::TIMEOUT_MESSAGE;
use anyhow::Result;
use codeyard_database::pool::PgPool;
use serde_json::Value;

pub async fn insert(pool: &PgPool, input: NewAsyncMerge<'_>) -> Result<bool> {
    Ok(sqlx::query_scalar::<_,String>("INSERT INTO pull_request_async_merges(uuid,repo_id,pull_request_id,token_id,worker_node_id,owner,name,number,request,details) VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9,$10) ON CONFLICT(pull_request_id) WHERE status='pending' DO NOTHING RETURNING uuid").bind(input.uuid).bind(input.repo_id).bind(input.pull_request_id).bind(input.token_id).bind(input.worker_node_id).bind(input.owner).bind(input.name).bind(input.number).bind(input.request).bind(input.details).fetch_optional(pool).await?.is_some())
}
pub async fn get(
    pool: &PgPool,
    repo_id: i64,
    pull_request_id: i64,
    uuid: &str,
) -> Result<Option<AsyncMerge>> {
    Ok(sqlx::query_as("SELECT * FROM pull_request_async_merges WHERE repo_id=$1 AND pull_request_id=$2 AND uuid=$3 AND expires_at>NOW()").bind(repo_id).bind(pull_request_id).bind(uuid).fetch_optional(pool).await?)
}
pub async fn finish(pool: &PgPool, uuid: &str, status: &str, details: &Value) -> Result<()> {
    sqlx::query("UPDATE pull_request_async_merges SET status=$2,details=$3,expires_at=NOW()+INTERVAL '1 day' WHERE uuid=$1 AND status='pending'").bind(uuid).bind(status).bind(details).execute(pool).await?;
    Ok(())
}
/// Requests whose pull request already merged finish as merged: the merge committed before the
/// restart cut off the result. The rest of this node's pending requests fail.
pub async fn recover(pool: &PgPool, node_id: Option<i64>, details: &Value) -> Result<u64> {
    let mut tx = pool.begin().await?;
    // Rows without a worker predate cluster adoption, so any node recovers them.
    let merged = sqlx::query("UPDATE pull_request_async_merges a SET status='merged',details=jsonb_build_object('message','Pull request #'||p.number||' merged successfully','sha',p.merge_commit_sha),expires_at=NOW()+INTERVAL '1 day' FROM pull_requests p WHERE p.id=a.pull_request_id AND p.merged AND p.merge_commit_sha IS NOT NULL AND (a.worker_node_id IS NULL OR a.worker_node_id=$1) AND a.status='pending'").bind(node_id).execute(&mut *tx).await?.rows_affected();
    let failed = sqlx::query("UPDATE pull_request_async_merges SET status='failed',details=$2,expires_at=NOW()+INTERVAL '1 day' WHERE (worker_node_id IS NULL OR worker_node_id=$1) AND status='pending'").bind(node_id).bind(details).execute(&mut *tx).await?.rows_affected();
    tx.commit().await?;
    Ok(merged + failed)
}
pub async fn prune(pool: &PgPool) -> Result<u64> {
    let mut tx = pool.begin().await?;
    let failed = sqlx::query("UPDATE pull_request_async_merges SET status='failed',details=$2,expires_at=NOW()+INTERVAL '1 day' WHERE status='pending' AND created_at<=NOW()-$1::bigint*INTERVAL '1 second'")
        .bind(PENDING_TIMEOUT_SECONDS).bind(serde_json::json!({"message":TIMEOUT_MESSAGE})).execute(&mut *tx).await?.rows_affected();
    let deleted = sqlx::query("DELETE FROM pull_request_async_merges WHERE expires_at<=NOW()")
        .execute(&mut *tx)
        .await?
        .rows_affected();
    tx.commit().await?;
    Ok(failed + deleted)
}
