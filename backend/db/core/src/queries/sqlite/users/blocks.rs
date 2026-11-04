use anyhow::Result;
use sqlx::SqliteConnection;
pub async fn repository_interaction_blocked(
    connection: &mut SqliteConnection,
    actor: i64,
    repo: i64,
) -> Result<bool> {
    Ok(sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM user_blocks b JOIN repositories r ON (r.owner_user_id=b.user_id AND b.blocked_id=?1) OR (r.owner_user_id=b.blocked_id AND b.user_id=?1) WHERE r.id=?2 AND r.owner_kind='user') OR EXISTS(SELECT 1 FROM organization_blocks b JOIN repositories r ON r.owner_org_id=b.org_id WHERE r.id=?2 AND r.owner_kind='org' AND b.user_id=?1)").bind(actor).bind(repo).fetch_one(connection).await?)
}
