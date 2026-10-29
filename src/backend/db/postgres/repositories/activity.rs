use crate::backend::db::repositories::activity::{Activity, Filter, Kind, RefActivity};
use anyhow::Result;
use sqlx::PgPool;
use uuid::Uuid;

pub async fn record(
    pool: &PgPool,
    repo: i64,
    actor: i64,
    operation: &Uuid,
    changes: &[RefActivity<'_>],
) -> Result<()> {
    let mut tx = pool.begin().await?;
    // Accepted-write bookkeeping does not reauthorize the actor. Lock identity before the
    // repository to avoid reversing account-deletion lock order; deleted actors become null.
    let actor: Option<i64> = sqlx::query_scalar("SELECT id FROM users WHERE id=$1 FOR KEY SHARE")
        .bind(actor)
        .fetch_optional(&mut *tx)
        .await?;
    let exists: Option<i64> =
        sqlx::query_scalar("SELECT id FROM repositories WHERE id=$1 FOR UPDATE")
            .bind(repo)
            .fetch_optional(&mut *tx)
            .await?;
    if exists.is_none() {
        tx.commit().await?;
        return Ok(());
    }
    for change in changes {
        sqlx::query("INSERT INTO repository_ref_activities (repo_id,actor_id,operation_id,reference,before_sha,after_sha,activity_type) VALUES ($1,$2,$3,$4,$5,$6,CASE WHEN $7 IN ('push','force_push') AND EXISTS(SELECT 1 FROM pull_requests p WHERE p.repo_id=$1 AND p.merged AND p.merge_commit_sha=$6 AND 'refs/heads/'||p.base_branch=$4) THEN 'pr_merge' ELSE $7 END) ON CONFLICT(repo_id,operation_id,reference) DO NOTHING")
            .bind(repo).bind(actor).bind(operation.to_string()).bind(change.reference).bind(change.before).bind(change.after).bind(change.kind.as_str()).execute(&mut *tx).await?;
    }
    tx.commit().await?;
    Ok(())
}
pub async fn list(
    pool: &PgPool,
    repo: i64,
    filter: &Filter<'_>,
    limit: i64,
    viewer: Option<i64>,
) -> Result<Vec<Activity>> {
    Ok(sqlx::query_as::<_,Activity>(&format!("SELECT a.*,actor.username AS actor FROM repository_ref_activities a JOIN repositories r ON r.id=a.repo_id LEFT JOIN users actor ON actor.id=a.actor_id LEFT JOIN users u ON u.id=$9 WHERE a.repo_id=$1 AND ({}) AND ($2::BIGINT IS NULL OR ($3 AND a.id<$2) OR (NOT $3 AND a.id>$2)) AND ($4::TEXT IS NULL OR a.reference=$4) AND ($5::TEXT IS NULL OR lower(actor.username)=lower($5)) AND ($6::TEXT IS NULL OR a.activity_type=$6) AND ($7::TIMESTAMPTZ IS NULL OR a.created_at>=$7) ORDER BY CASE WHEN $3 THEN a.id END DESC,CASE WHEN NOT $3 THEN a.id END ASC LIMIT $8", super::access::readable_condition()))
        .bind(repo).bind(filter.boundary).bind(filter.descending).bind(filter.reference).bind(filter.actor).bind(filter.kind.map(Kind::as_str)).bind(filter.since.as_ref()).bind(limit.clamp(1,101)).bind(viewer).fetch_all(pool).await?)
}
