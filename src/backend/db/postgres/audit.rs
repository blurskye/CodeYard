use super::models::AuditEntry;
use anyhow::Result;
use sqlx::PgPool;

pub async fn count_audit_entries(pool: &PgPool, action: &str, actor: &str) -> Result<i64> {
    Ok(sqlx::query_scalar("SELECT COUNT(*) FROM audit_log a LEFT JOIN users u ON u.id=a.actor_id WHERE strpos(lower(a.action),$1)>0 AND strpos(lower(COALESCE(u.username,'')),$2)>0")
        .bind(action.trim().to_lowercase())
        .bind(actor.trim().to_lowercase())
        .fetch_one(pool)
        .await?)
}

pub async fn list_audit_entries(
    pool: &PgPool,
    action: &str,
    actor: &str,
    limit: i64,
    offset: i64,
) -> Result<Vec<AuditEntry>> {
    Ok(sqlx::query_as("SELECT a.id,u.username AS actor,a.action,a.target_type,a.target_id,COALESCE(a.metadata,'{}'::jsonb)::TEXT AS details,a.created_at FROM audit_log a LEFT JOIN users u ON u.id=a.actor_id WHERE strpos(lower(a.action),$1)>0 AND strpos(lower(COALESCE(u.username,'')),$2)>0 ORDER BY a.id DESC LIMIT $3 OFFSET $4")
        .bind(action.trim().to_lowercase())
        .bind(actor.trim().to_lowercase())
        .bind(limit.clamp(1, 100))
        .bind(offset.max(0))
        .fetch_all(pool)
        .await?)
}
