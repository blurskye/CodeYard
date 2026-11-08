use super::*;
use crate::sqlite::projects::readable;

pub async fn list_page(
    pool: &SqlitePool,
    viewer: Option<i64>,
    owner: Owner,
    after: i64,
    limit: i64,
) -> Result<Vec<Project>> {
    list_window(pool, viewer, owner, after, i64::MAX, limit).await
}

pub async fn list_window(
    pool: &SqlitePool,
    viewer: Option<i64>,
    owner: Owner,
    after: i64,
    before: i64,
    limit: i64,
) -> Result<Vec<Project>> {
    anyhow::ensure!(
        after >= 0 && before > after && (1..=101).contains(&limit),
        Refusal::Invalid
    );
    let (kind, id) = owner.parts();
    let direction = if before < i64::MAX { "DESC" } else { "ASC" };
    Ok(sqlx::query_as(sqlx::AssertSqlSafe(format!("SELECT * FROM projects p WHERE p.owner_kind=$1 AND p.owner_id=$2 AND {} AND p.id>$4 AND p.id<$5 ORDER BY p.id {direction} LIMIT $6", crate::projects::readable_condition("$3"))))
        .bind(kind).bind(id).bind(viewer).bind(after).bind(before).bind(limit).fetch_all(pool).await?)
}

pub async fn fields_page(
    pool: &SqlitePool,
    viewer: Option<i64>,
    project: i64,
    after: i64,
    limit: i64,
) -> Result<Vec<Field>> {
    fields_window(pool, viewer, project, after, i64::MAX, limit).await
}

pub async fn fields_window(
    pool: &SqlitePool,
    viewer: Option<i64>,
    project: i64,
    after: i64,
    before: i64,
    limit: i64,
) -> Result<Vec<Field>> {
    anyhow::ensure!(
        after >= 0 && before > after && (1..=101).contains(&limit),
        Refusal::Invalid
    );
    let mut tx = pool.begin().await?;
    readable(&mut tx, viewer, project).await?;
    let direction = if before < i64::MAX { "DESC" } else { "ASC" };
    let fields = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT *, (SELECT owner_id FROM projects WHERE id=project_fields.project_id) AS owner_id FROM project_fields WHERE project_id=$1 AND id>$2 AND id<$3 ORDER BY id {direction} LIMIT $4",
    )))
    .bind(project)
    .bind(after)
    .bind(before)
    .bind(limit)
    .fetch_all(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(fields)
}
