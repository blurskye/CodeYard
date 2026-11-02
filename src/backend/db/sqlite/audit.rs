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
    Ok(sqlx::query_as("SELECT a.id,u.username AS actor,a.action,a.target_type,a.target_id,CASE a.target_type WHEN 'user' THEN (SELECT t.username FROM users t WHERE t.id=a.target_id) WHEN 'node' THEN (SELECT n.name FROM nodes n WHERE n.id=a.target_id) WHEN 'org_membership' THEN (SELECT t.username FROM org_memberships m JOIN users t ON t.id=m.user_id WHERE m.id=a.target_id) END AS target_name,(SELECT o.name FROM organizations o WHERE o.id=a.org_id) AS org_name,COALESCE(a.metadata,'{}') AS details,a.created_at FROM audit_log a LEFT JOIN users u ON u.id=a.actor_id WHERE a.action REGEXP ?1 AND COALESCE(u.username,'') REGEXP ?2 ORDER BY a.id DESC LIMIT ?3 OFFSET ?4")
        .bind(super::search::contains(action.trim()))
        .bind(super::search::contains(actor.trim()))
        .bind(limit.clamp(1, 100))
        .bind(offset.max(0))
        .fetch_all(pool)
        .await?)
}
