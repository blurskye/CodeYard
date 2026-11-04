use crate::checks::Annotation;
use crate::checks::PageSnapshot;
use crate::checks::Refusal;
use crate::checks::Run;
use crate::checks::Suite;
use anyhow::ensure;
use anyhow::Result;
use codeyard_db_core::queries::postgres::repositories::access::USER_REPO_ACCESS_SQL;
use sqlx::types::Json;
use sqlx::PgPool;

pub async fn page_snapshot(
    pool: &PgPool,
    viewer: Option<i64>,
    session: Option<&str>,
    repo: i64,
    id: i64,
    offset: i64,
) -> Result<PageSnapshot> {
    ensure!(offset >= 0, Refusal::Invalid);
    let mut tx = pool.begin().await?;
    sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ, READ ONLY")
        .execute(&mut *tx)
        .await?;
    let allowed: bool = sqlx::query_scalar(&format!(
        "SELECT EXISTS(SELECT 1 FROM repositories r LEFT JOIN users u ON u.id=$1 AND NOT u.is_banned AND NOT u.must_change_password AND EXISTS(SELECT 1 FROM sessions s WHERE s.id=$2 AND s.user_id=u.id AND s.expires_at>clock_timestamp()) WHERE r.id=$3 AND NOT r.is_gist AND (NOT r.is_private OR (r.owner_kind='user' AND u.is_admin) OR ({USER_REPO_ACCESS_SQL})>=1))"
    )).bind(viewer).bind(session).bind(repo).fetch_one(&mut *tx).await?;
    ensure!(allowed, Refusal::Missing);
    let run = sqlx::query_as::<_, Run>("SELECT * FROM check_runs WHERE repo_id=$1 AND id=$2")
        .bind(repo)
        .bind(id)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or(Refusal::Missing)?;
    let suite = sqlx::query_as::<_, Suite>("SELECT * FROM check_suites WHERE repo_id=$1 AND id=$2")
        .bind(repo)
        .bind(run.suite_id)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or(Refusal::Missing)?;
    let annotations = sqlx::query_scalar::<_, Json<Annotation>>(
        "SELECT data FROM check_annotations WHERE run_id=$1 ORDER BY id LIMIT $2 OFFSET $3",
    )
    .bind(id)
    .bind((crate::checks::ANNOTATIONS_PER_PAGE + 1) as i64)
    .bind(offset)
    .fetch_all(&mut *tx)
    .await?;
    let total = sqlx::query_scalar("SELECT COUNT(*) FROM check_annotations WHERE run_id=$1")
        .bind(id)
        .fetch_one(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(PageSnapshot {
        run,
        suite,
        annotations,
        total,
    })
}
