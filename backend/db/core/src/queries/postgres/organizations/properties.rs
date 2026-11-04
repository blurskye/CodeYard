use crate::queries::organizations::properties::effective_values;
use crate::queries::organizations::properties::Definition;
use crate::queries::organizations::properties::DefinitionRow;
use crate::queries::organizations::properties::ValueInput;
use crate::queries::organizations::OrganizationOutcome;
use crate::queries::postgres::organizations::authority::begin_read;
use crate::queries::postgres::organizations::authority::require_read_org;
use anyhow::Result;
use sqlx::PgConnection;
use sqlx::PgPool;
use OrganizationOutcome::Applied;
pub async fn definition(
    pool: &PgPool,
    actor: i64,
    org: i64,
    name: &str,
) -> Result<OrganizationOutcome<Option<Definition>>> {
    let mut tx = begin_read(pool).await?;
    require_read_org!(tx, actor, org);
    let result = sqlx::query_as::<_, DefinitionRow>("SELECT * FROM organization_repository_properties WHERE org_id=$1 AND LOWER(property_name)=LOWER($2)")
        .bind(org)
        .bind(name)
        .fetch_optional(&mut *tx)
        .await?
        .map(DefinitionRow::decode)
        .transpose()?;
    tx.commit().await?;
    Ok(Applied(result))
}

pub async fn values_on(
    c: &mut PgConnection,
    repo: i64,
    definitions: &[Definition],
) -> Result<Vec<ValueInput>> {
    if definitions.is_empty() {
        return Ok(Vec::new());
    }
    let ids: Vec<i64> = definitions.iter().map(|definition| definition.id).collect();
    let rows: std::collections::HashMap<i64, String> = sqlx::query_as::<_, (i64, String)>(
        "SELECT definition_id,value FROM repository_property_values WHERE repo_id=$1 AND definition_id=ANY($2)",
    )
    .bind(repo)
    .bind(&ids)
    .fetch_all(c)
    .await?
    .into_iter()
    .collect();
    crate::queries::organizations::properties::effective_values(definitions, &rows)
}
