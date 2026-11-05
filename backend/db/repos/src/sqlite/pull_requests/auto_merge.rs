use crate::pull_requests::auto_merge::AutoMerge;
use crate::pull_requests::auto_merge::AutoMergeRequest;
use anyhow::Result;
use codeyard_db_core::pool::SqlitePool;

pub async fn enable(
    pool: &SqlitePool,
    request: &AutoMergeRequest,
    expected_head: Option<&str>,
) -> Result<()> {
    use codeyard_db_core::queries::discussions::DiscussionWriteRefusal as Refusal;
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let repo: i64 = sqlx::query_scalar("SELECT repo_id FROM pull_requests WHERE id=$1")
        .bind(request.pull_request_id)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or(Refusal::Denied)?;
    anyhow::ensure!(
        codeyard_db_core::queries::sqlite::discussions::lock_write_authority(
            &mut tx,
            request.enabled_by,
            repo
        )
        .await?,
        Refusal::Denied
    );
    if let Some(token) = request.token_id {
        for scope in ["contents", "pull-requests"] {
            anyhow::ensure!(
                codeyard_db_identity::sqlite::users::tokens::repository_write_on(
                    &mut tx,
                    token,
                    request.enabled_by,
                    repo,
                    scope
                )
                .await?,
                Refusal::Denied
            );
        }
    }
    let head: Option<String> = sqlx::query_scalar("SELECT head_sha FROM pull_requests WHERE id=$1 AND repo_id=$2 AND state='open' AND NOT merged")
        .bind(request.pull_request_id).bind(repo).fetch_optional(&mut *tx).await?;
    let head = head.ok_or(Refusal::Denied)?;
    anyhow::ensure!(
        expected_head.is_none_or(|expected| expected.eq_ignore_ascii_case(&head)),
        Refusal::RevisionChanged
    );
    sqlx::query(
        "INSERT INTO pull_request_auto_merges(pull_request_id,enabled_by,merge_method,commit_title,commit_message,author_email,enabled_at,token_id) VALUES(?1,?2,?3,?4,?5,?6,?7,?8) ON CONFLICT(pull_request_id) DO UPDATE SET enabled_by=excluded.enabled_by,token_id=excluded.token_id,merge_method=excluded.merge_method,commit_title=excluded.commit_title,commit_message=excluded.commit_message,author_email=excluded.author_email,enabled_at=excluded.enabled_at,generation=excluded.generation",
    )
    .bind(request.pull_request_id)
    .bind(request.enabled_by)
    .bind(&request.merge_method)
    .bind(&request.commit_title)
    .bind(&request.commit_message)
    .bind(&request.author_email)
    .bind(request.enabled_at)
    .bind(request.token_id)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(())
}

pub async fn disable(pool: &SqlitePool, pull_request_id: i64) -> Result<bool> {
    let done = sqlx::query("DELETE FROM pull_request_auto_merges WHERE pull_request_id=?1")
        .bind(pull_request_id)
        .execute(pool)
        .await?;
    Ok(done.rows_affected() > 0)
}

pub async fn retire(pool: &SqlitePool, pull_request_id: i64, generation: &str) -> Result<bool> {
    let done = sqlx::query(
        "DELETE FROM pull_request_auto_merges WHERE pull_request_id=?1 AND generation=?2",
    )
    .bind(pull_request_id)
    .bind(generation)
    .execute(pool)
    .await?;
    Ok(done.rows_affected() > 0)
}

pub async fn get(pool: &SqlitePool, pull_request_id: i64) -> Result<Option<AutoMerge>> {
    Ok(
        sqlx::query_as("SELECT * FROM pull_request_auto_merges WHERE pull_request_id=?1")
            .bind(pull_request_id)
            .fetch_optional(pool)
            .await?,
    )
}

pub async fn pending(pool: &SqlitePool, after: i64, limit: i64) -> Result<Vec<AutoMerge>> {
    Ok(sqlx::query_as(
        "SELECT * FROM pull_request_auto_merges WHERE pull_request_id>?1 ORDER BY pull_request_id LIMIT ?2",
    )
    .bind(after)
    .bind(limit)
    .fetch_all(pool)
    .await?)
}
