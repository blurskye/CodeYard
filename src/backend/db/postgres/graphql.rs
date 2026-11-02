use crate::backend::db::graphql::RateLimit;
use anyhow::Result;
use sqlx::PgPool;

pub async fn team_locator(pool: &PgPool, id: i64) -> Result<Option<(i64, String)>> {
    Ok(sqlx::query_as("SELECT org_id,slug FROM teams WHERE id=$1")
        .bind(id)
        .fetch_optional(pool)
        .await?)
}
pub async fn node_repository(pool: &PgPool, kind: &str, id: i64) -> Result<Option<i64>> {
    let Some(sql) = crate::backend::db::graphql::node_repository_sql(kind) else {
        return Ok(None);
    };
    let mut query = sqlx::query_scalar(sql).bind(id);
    if kind == "ART" {
        query = query.bind(chrono::Utc::now().timestamp());
    }
    Ok(query.fetch_optional(pool).await?)
}
pub async fn reaction(
    pool: &PgPool,
    id: i64,
) -> Result<Option<crate::backend::db::reactions::Reaction>> {
    Ok(sqlx::query_as("SELECT * FROM reactions WHERE id=$1")
        .bind(id)
        .fetch_optional(pool)
        .await?)
}
pub async fn status(
    pool: &PgPool,
    id: i64,
) -> Result<Option<crate::backend::db::models::CommitStatus>> {
    Ok(sqlx::query_as("SELECT * FROM commit_statuses WHERE id=$1")
        .bind(id)
        .fetch_optional(pool)
        .await?)
}

pub async fn admit_query(pool: &PgPool, token: i64) -> Result<Option<RateLimit>> {
    Ok(sqlx::query_as("INSERT INTO graphql_rate_limits(token_id,used,reset_at) VALUES($1,1,NOW()+INTERVAL '1 hour') ON CONFLICT(token_id) DO UPDATE SET used=CASE WHEN graphql_rate_limits.reset_at<=NOW() THEN 1 ELSE graphql_rate_limits.used+1 END,reset_at=CASE WHEN graphql_rate_limits.reset_at<=NOW() THEN NOW()+INTERVAL '1 hour' ELSE graphql_rate_limits.reset_at END WHERE graphql_rate_limits.reset_at<=NOW() OR graphql_rate_limits.used<5000 RETURNING used,reset_at")
        .bind(token).fetch_optional(pool).await?)
}
