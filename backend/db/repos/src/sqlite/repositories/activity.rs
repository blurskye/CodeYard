use crate::repositories::activity::Activity;
use crate::repositories::activity::Filter;
use crate::repositories::activity::Kind;
use crate::repositories::activity::RefActivity;
use anyhow::Result;
use codeyard_database::pool::SqlitePool;
use uuid::Uuid;

pub async fn record(
    pool: &SqlitePool,
    repo: i64,
    actor: i64,
    operation: &Uuid,
    changes: &[RefActivity<'_>],
) -> Result<()> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let exists: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM repositories WHERE id=?1)")
        .bind(repo)
        .fetch_one(&mut *tx)
        .await?;
    if !exists {
        tx.commit().await?;
        return Ok(());
    }
    let actor: Option<i64> = sqlx::query_scalar("SELECT id FROM users WHERE id=?1")
        .bind(actor)
        .fetch_optional(&mut *tx)
        .await?;
    for change in changes {
        sqlx::query("INSERT INTO repository_ref_activities (repo_id,actor_id,operation_id,reference,before_sha,after_sha,activity_type) VALUES (?1,?2,?3,?4,?5,?6,CASE WHEN ?7 IN ('push','force_push') AND EXISTS(SELECT 1 FROM pull_requests p WHERE p.repo_id=?1 AND p.merged AND p.merge_commit_sha=?6 AND 'refs/heads/'||p.base_branch=?4) THEN 'pr_merge' ELSE ?7 END) ON CONFLICT(repo_id,operation_id,reference) DO NOTHING")
            .bind(repo).bind(actor).bind(operation.to_string()).bind(change.reference).bind(change.before).bind(change.after).bind(change.kind.as_str()).execute(&mut *tx).await?;
    }
    tx.commit().await?;
    Ok(())
}
pub async fn list(
    pool: &SqlitePool,
    repo: i64,
    filter: &Filter<'_>,
    limit: i64,
    viewer: Option<i64>,
) -> Result<Vec<Activity>> {
    Ok(
        sqlx::query_as::<_, Activity>(&crate::repositories::activity::list_sql(
            false,
            filter,
            &codeyard_database::queries::sqlite::repositories::access::readable_condition(),
        ))
        .bind(repo)
        .bind(filter.boundary)
        .bind(filter.reference)
        .bind(filter.actor)
        .bind(filter.kind.map(Kind::as_str))
        .bind(
            filter
                .since
                .map(codeyard_database::queries::sqlite::time::encode),
        )
        .bind(limit.clamp(1, 101))
        .bind(viewer)
        .fetch_all(pool)
        .await?,
    )
}
