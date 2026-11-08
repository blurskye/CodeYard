use anyhow::Result;
use sqlx::PgConnection;
/// Role membership remains meaningful for reads when organization writes are suspended.
pub(crate) async fn holds_security_role(
    c: &mut PgConnection,
    actor: i64,
    org: i64,
) -> Result<bool> {
    let condition = crate::queries::organizations::roles::security_role_condition("$1", "$2");
    Ok(sqlx::query_scalar(&format!("SELECT {condition}"))
        .bind(actor)
        .bind(org)
        .fetch_one(c)
        .await?)
}

/// Writes additionally require an operational organization.
pub async fn manages_security(c: &mut PgConnection, actor: i64, org: i64) -> Result<bool> {
    let condition = crate::queries::organizations::roles::security_manager_condition("$1", "$2");
    Ok(sqlx::query_scalar(&format!("SELECT {condition}"))
        .bind(actor)
        .bind(org)
        .fetch_one(c)
        .await?)
}
