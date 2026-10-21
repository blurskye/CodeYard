use super::models::AuditEntry;
use anyhow::Result;
use sqlx::SqlitePool;

pub async fn count_audit_entries(pool: &SqlitePool, action: &str, actor: &str) -> Result<i64> {
    Ok(sqlx::query_scalar("SELECT COUNT(*) FROM audit_log a LEFT JOIN users u ON u.id=a.actor_id WHERE a.action REGEXP ?1 AND COALESCE(u.username,'') REGEXP ?2")
        .bind(super::search::contains(action.trim()))
        .bind(super::search::contains(actor.trim()))
        .fetch_one(pool)
        .await?)
}

pub async fn list_audit_entries(
    pool: &SqlitePool,
    action: &str,
    actor: &str,
    limit: i64,
    offset: i64,
) -> Result<Vec<AuditEntry>> {
    Ok(sqlx::query_as("SELECT a.id,u.username AS actor,a.action,a.target_type,a.target_id,COALESCE(a.metadata,'{}') AS details,a.created_at FROM audit_log a LEFT JOIN users u ON u.id=a.actor_id WHERE a.action REGEXP ?1 AND COALESCE(u.username,'') REGEXP ?2 ORDER BY a.id DESC LIMIT ?3 OFFSET ?4")
        .bind(super::search::contains(action.trim()))
        .bind(super::search::contains(actor.trim()))
        .bind(limit.clamp(1, 100))
        .bind(offset.max(0))
        .fetch_all(pool)
        .await?)
}
