use anyhow::Result;
use sqlx::SqliteConnection;
/// Freeze the same effective retention used by artifact creation when logs become complete.
pub(super) async fn retain_completed(
    connection: &mut SqliteConnection,
    repo: i64,
    run: i64,
) -> Result<()> {
    let (private, own, org, site): (bool, String, String, Option<i32>) = sqlx::query_as("SELECT r.is_private,COALESCE(s.rest_settings,'{}'),COALESCE(o.rest_settings,'{}'),site.artifact_retention_days FROM repositories r LEFT JOIN repo_actions_settings s ON s.repo_id=r.id LEFT JOIN org_actions_settings o ON r.owner_kind='org' AND o.org_id=r.owner_org_id LEFT JOIN actions_settings site ON site.id=1 WHERE r.id=?1")
        .bind(repo).fetch_one(&mut *connection).await?;
    let days = crate::queries::action_settings::retention(
        &serde_json::from_str(&own)?,
        &serde_json::from_str(&org)?,
        site,
        private,
    );
    let expiry = chrono::Utc::now().timestamp() + i64::from(days) * 86400;
    sqlx::query("UPDATE ci_runs SET log_expires_at=?2 WHERE id=?1 AND repo_id=?3 AND completed_at IS NOT NULL AND log_version=0 AND log_expires_at IS NULL")
        .bind(run).bind(expiry).bind(repo).execute(connection).await?;
    Ok(())
}
