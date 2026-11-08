use anyhow::Result;
use sqlx::SqliteConnection;
/// Role membership remains meaningful for reads when organization writes are suspended.
pub(crate) async fn holds_security_role(
    c: &mut SqliteConnection,
    actor: i64,
    org: i64,
) -> Result<bool> {
    let condition = crate::queries::organizations::roles::security_role_condition("?1", "?2");
    Ok(
        sqlx::query_scalar(sqlx::AssertSqlSafe(format!("SELECT {condition}")))
            .bind(actor)
            .bind(org)
            .fetch_one(c)
            .await?,
    )
}

/// Whether `actor` holds the organization's security-manager role, directly or through a team.
pub async fn manages_security(c: &mut SqliteConnection, actor: i64, org: i64) -> Result<bool> {
    let condition = crate::queries::organizations::roles::security_manager_condition("?1", "?2");
    Ok(
        sqlx::query_scalar(sqlx::AssertSqlSafe(format!("SELECT {condition}")))
            .bind(actor)
            .bind(org)
            .fetch_one(c)
            .await?,
    )
}
