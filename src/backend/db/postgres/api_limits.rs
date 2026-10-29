use crate::backend::db::api_limits::{Budget, Request};
use anyhow::Result;
use sqlx::PgPool;

pub async fn budget(pool: &PgPool, request: Request<'_>) -> Result<(Budget, bool)> {
    let Request {
        key,
        resource,
        now,
        limit,
        seconds,
        consume,
    } = request;
    if consume {
        // ON CONFLICT locks even a refused row until its matching budget has been read.
        let mut tx = pool.begin().await?;
        sqlx::query("DELETE FROM github_api_limits WHERE reset_at<=$1")
            .bind(now)
            .execute(&mut *tx)
            .await?;
        let row = sqlx::query_as("INSERT INTO github_api_limits(requester,resource,used,reset_at) VALUES($1,$2,1,$3) ON CONFLICT(requester,resource) DO UPDATE SET used=github_api_limits.used+1 WHERE github_api_limits.used<$4 RETURNING used,reset_at")
            .bind(key).bind(resource).bind(now+seconds).bind(limit).fetch_optional(&mut *tx).await?;
        let result = if let Some(row) = row {
            (row, true)
        } else {
            let row = sqlx::query_as(
                "SELECT used,reset_at FROM github_api_limits WHERE requester=$1 AND resource=$2",
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
    let row = sqlx::query_as("SELECT used,reset_at FROM github_api_limits WHERE requester=$1 AND resource=$2 AND reset_at>$3")
        .bind(key).bind(resource).bind(now).fetch_optional(pool).await?;
    Ok((
        row.unwrap_or(Budget {
            used: 0,
            reset_at: now + seconds,
        }),
        true,
    ))
}
