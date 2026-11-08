use crate::graphql::RateLimit;
use anyhow::Result;
use codeyard_db_core::pool::SqlitePool;

pub async fn linked_branch(
    pool: &SqlitePool,
    id: i64,
) -> Result<Option<crate::issues::management::LinkedBranch>> {
    Ok(
        sqlx::query_as("SELECT id,repo_id,name FROM issue_linked_branches WHERE id=?1")
            .bind(id)
            .fetch_optional(pool)
            .await?,
    )
}
pub async fn ruleset_owner(
    pool: &SqlitePool,
    id: i64,
) -> Result<Option<(Option<i64>, Option<i64>)>> {
    Ok(
        sqlx::query_as("SELECT owner_repo_id,owner_org_id FROM repository_rulesets WHERE id=?1")
            .bind(id)
            .fetch_optional(pool)
            .await?,
    )
}
pub async fn issue_type_repositories(
    pool: &SqlitePool,
    id: i64,
    after: i64,
    viewer: i64,
) -> Result<Vec<i64>> {
    Ok(sqlx::query_scalar(&format!("SELECT r.id FROM repositories r JOIN issue_attributes a ON a.owner_user_id=r.owner_user_id OR a.owner_org_id=r.owner_org_id LEFT JOIN users u ON u.id=?3 WHERE a.id=?1 AND a.kind='type' AND r.id>?2 AND ({}) ORDER BY r.id LIMIT 100", codeyard_db_core::queries::sqlite::repositories::access::readable_condition()))
        .bind(id).bind(after).bind(viewer).fetch_all(pool).await?)
}

pub async fn team_locator(pool: &SqlitePool, id: i64) -> Result<Option<(i64, String)>> {
    Ok(sqlx::query_as("SELECT org_id,slug FROM teams WHERE id=$1")
        .bind(id)
        .fetch_optional(pool)
        .await?)
}
pub async fn node_root_matches(pool: &SqlitePool, kind: &str, id: i64, root: i64) -> Result<bool> {
    let Some(sql) = crate::graphql::node_root_sql(kind) else {
        return Ok(false);
    };
    let mut query = sqlx::query_scalar(&sql).bind(id).bind(root);
    if kind == "ART" {
        query = query.bind(chrono::Utc::now().timestamp());
    }
    Ok(query.fetch_one(pool).await?)
}
pub async fn node_owner(pool: &SqlitePool, kind: &str, id: i64) -> Result<Option<i64>> {
    let Some(sql) = crate::graphql::node_owner_sql(kind) else {
        return Ok(None);
    };
    let mut query = sqlx::query_scalar(sql).bind(id);
    if kind == "ART" {
        query = query.bind(chrono::Utc::now().timestamp());
    }
    Ok(query.fetch_optional(pool).await?)
}
pub async fn reaction(pool: &SqlitePool, id: i64) -> Result<Option<crate::reactions::Reaction>> {
    Ok(sqlx::query_as("SELECT * FROM reactions WHERE id=?1")
        .bind(id)
        .fetch_optional(pool)
        .await?)
}
pub async fn status(
    pool: &SqlitePool,
    id: i64,
) -> Result<Option<codeyard_db_core::models::CommitStatus>> {
    Ok(sqlx::query_as("SELECT id,repo_id,sha,context,state,description,target_url,creator_id,created_at,created_at AS updated_at FROM commit_status_history WHERE id=?1")
        .bind(id)
        .fetch_optional(pool)
        .await?)
}

pub async fn admit_query(
    pool: &SqlitePool,
    actor: &codeyard_db_core::queries::credentials::Actor,
) -> Result<Option<RateLimit>> {
    let credential = actor
        .credential()
        .ok_or(codeyard_types::credentials::AccountRestricted)?;
    let token = actor
        .token_id()
        .ok_or(codeyard_types::credentials::AccountRestricted)?;
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    codeyard_db_identity::sqlite::users::transitions::require_read_account(
        &mut tx,
        actor.user_id,
        &credential,
    )
    .await?;
    let result = sqlx::query_as("INSERT INTO graphql_rate_limits(token_id,user_id,used,reset_at) VALUES(?1,?3,1,strftime('%Y-%m-%dT%H:%M:%f000Z','now','+1 hour')) ON CONFLICT(token_id) DO UPDATE SET used=CASE WHEN graphql_rate_limits.reset_at<=strftime('%Y-%m-%dT%H:%M:%f000Z','now') THEN 1 ELSE graphql_rate_limits.used+1 END,reset_at=CASE WHEN graphql_rate_limits.reset_at<=strftime('%Y-%m-%dT%H:%M:%f000Z','now') THEN strftime('%Y-%m-%dT%H:%M:%f000Z','now','+1 hour') ELSE graphql_rate_limits.reset_at END WHERE graphql_rate_limits.user_id=excluded.user_id AND (graphql_rate_limits.reset_at<=strftime('%Y-%m-%dT%H:%M:%f000Z','now') OR graphql_rate_limits.used<?2) RETURNING used,reset_at")
        .bind(token).bind(crate::graphql::QUERY_LIMIT).bind(actor.user_id).fetch_optional(&mut *tx).await?;
    tx.commit().await?;
    Ok(result)
}
