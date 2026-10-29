use super::*;

pub async fn list_page(
    pool: &PgPool,
    viewer: Option<i64>,
    owner: Owner,
    after: i64,
    limit: i64,
) -> Result<Vec<Project>> {
    anyhow::ensure!(after >= 0 && (1..=100).contains(&limit), Refusal::Invalid);
    let (kind, id) = owner.parts();
    Ok(sqlx::query_as(&format!("SELECT * FROM projects p WHERE p.owner_kind=$1 AND p.owner_id=$2 AND {} AND p.id>$4 ORDER BY p.id LIMIT $5", crate::backend::db::projects::readable_condition("$3")))
        .bind(kind).bind(id).bind(viewer).bind(after).bind(limit).fetch_all(pool).await?)
}

pub async fn fields_page(
    pool: &PgPool,
    viewer: Option<i64>,
    project: i64,
    after: i64,
    limit: i64,
) -> Result<Vec<Field>> {
    anyhow::ensure!(after >= 0 && (1..=100).contains(&limit), Refusal::Invalid);
    let mut tx = pool.begin().await?;
    readable(&mut tx, viewer, project).await?;
    let fields = sqlx::query_as(
        "SELECT * FROM project_fields WHERE project_id=$1 AND id>$2 ORDER BY id LIMIT $3",
    )
    .bind(project)
    .bind(after)
    .bind(limit)
    .fetch_all(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(fields)
}
