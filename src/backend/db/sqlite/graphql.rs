use crate::backend::db::graphql::RateLimit;
use anyhow::Result;
use sqlx::SqlitePool;

pub async fn linked_branch(pool: &SqlitePool, id: i64) -> Result<Option<crate::backend::db::issues::management::LinkedBranch>> {
    Ok(sqlx::query_as("SELECT id,repo_id,name FROM issue_linked_branches WHERE id=?1").bind(id).fetch_optional(pool).await?)
}
pub async fn ruleset_owner(pool: &SqlitePool, id: i64) -> Result<Option<(Option<i64>, Option<i64>)>> {
    Ok(sqlx::query_as("SELECT owner_repo_id,owner_org_id FROM repository_rulesets WHERE id=?1").bind(id).fetch_optional(pool).await?)
}
pub async fn issue_type_repositories(pool: &SqlitePool, id: i64, after: i64, viewer: i64) -> Result<Vec<i64>> {
    Ok(sqlx::query_scalar(&format!("SELECT r.id FROM repositories r JOIN issue_attributes a ON a.owner_user_id=r.owner_user_id OR a.owner_org_id=r.owner_org_id LEFT JOIN users u ON u.id=?3 WHERE a.id=?1 AND a.kind='type' AND r.id>?2 AND ({}) ORDER BY r.id LIMIT 100", super::repositories::access::readable_condition()))
        .bind(id).bind(after).bind(viewer).fetch_all(pool).await?)
}

pub async fn team_locator(pool: &SqlitePool, id: i64) -> Result<Option<(i64, String)>> {
    Ok(sqlx::query_as("SELECT org_id,slug FROM teams WHERE id=$1")
        .bind(id)
        .fetch_optional(pool)
        .await?)
}
pub async fn node_repository(pool: &SqlitePool, kind: &str, id: i64) -> Result<Option<i64>> {
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
    pool: &SqlitePool,
    id: i64,
) -> Result<Option<crate::backend::db::reactions::Reaction>> {
    Ok(sqlx::query_as("SELECT * FROM reactions WHERE id=?1")
        .bind(id)
        .fetch_optional(pool)
        .await?)
}
pub async fn status(
    pool: &SqlitePool,
    id: i64,
) -> Result<Option<crate::backend::db::models::CommitStatus>> {
    Ok(sqlx::query_as("SELECT * FROM commit_statuses WHERE id=?1")
        .bind(id)
        .fetch_optional(pool)
        .await?)
}

pub async fn admit_query(pool: &SqlitePool, token: i64) -> Result<Option<RateLimit>> {
    Ok(sqlx::query_as("INSERT INTO graphql_rate_limits(token_id,used,reset_at) VALUES(?1,1,strftime('%Y-%m-%dT%H:%M:%f000Z','now','+1 hour')) ON CONFLICT(token_id) DO UPDATE SET used=CASE WHEN graphql_rate_limits.reset_at<=strftime('%Y-%m-%dT%H:%M:%f000Z','now') THEN 1 ELSE graphql_rate_limits.used+1 END,reset_at=CASE WHEN graphql_rate_limits.reset_at<=strftime('%Y-%m-%dT%H:%M:%f000Z','now') THEN strftime('%Y-%m-%dT%H:%M:%f000Z','now','+1 hour') ELSE graphql_rate_limits.reset_at END WHERE graphql_rate_limits.reset_at<=strftime('%Y-%m-%dT%H:%M:%f000Z','now') OR graphql_rate_limits.used<5000 RETURNING used,reset_at")
        .bind(token).fetch_optional(pool).await?)
}
