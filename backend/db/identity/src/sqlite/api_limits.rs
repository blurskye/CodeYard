use crate::api_limits::Budget;
use crate::api_limits::Request;
use anyhow::Result;
use codeyard_db_core::pool::SqlitePool;

pub async fn budget(pool: &SqlitePool, request: Request<'_>) -> Result<(Budget, bool)> {
    let Request {
        key,
        resource,
        now,
        limit,
        seconds,
        consume,
    } = request;
    if consume {
        // Keep the refusal and its headers in the same window as the attempted increment.
        let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
        sqlx::query("DELETE FROM github_api_limits WHERE (requester,resource) IN (SELECT requester,resource FROM github_api_limits WHERE reset_at<=?1 ORDER BY reset_at LIMIT 100)")
            .bind(now)
            .execute(&mut *tx)
            .await?;
        let row = sqlx::query_as("INSERT INTO github_api_limits(requester,resource,used,reset_at) VALUES(?1,?2,1,?3) ON CONFLICT(requester,resource) DO UPDATE SET used=CASE WHEN github_api_limits.reset_at<=?5 THEN 1 ELSE github_api_limits.used+1 END,reset_at=CASE WHEN github_api_limits.reset_at<=?5 THEN excluded.reset_at ELSE github_api_limits.reset_at END WHERE github_api_limits.reset_at<=?5 OR github_api_limits.used<?4 RETURNING used,reset_at")
            .bind(key).bind(resource).bind(now+seconds).bind(limit).bind(now).fetch_optional(&mut *tx).await?;
        let result = if let Some(row) = row {
            (row, true)
        } else {
            let row = sqlx::query_as(
                "SELECT used,reset_at FROM github_api_limits WHERE requester=?1 AND resource=?2",
            )
            .bind(key)
            .bind(resource)
            .fetch_one(&mut *tx)
            .await?;
            (row, false)
        };
        tx.commit().await?;
        return Ok(result);
    }
    let row = sqlx::query_as("SELECT used,reset_at FROM github_api_limits WHERE requester=?1 AND resource=?2 AND reset_at>?3")
        .bind(key).bind(resource).bind(now).fetch_optional(pool).await?;
    Ok((
        row.unwrap_or(Budget {
            used: 0,
            reset_at: now + seconds,
        }),
        true,
    ))
}
