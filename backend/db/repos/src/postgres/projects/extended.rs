use super::items::ITEM;
use super::readable_at;
use super::*;
use crate::postgres::projects::items::writable_item;
use crate::postgres::projects::owner_authority;
use crate::postgres::projects::readable;
use crate::postgres::projects::write_project;
use crate::projects::valid_name;
use crate::projects::Item;
use std::collections::HashMap;

pub async fn link_team(
    pool: &PgPool,
    actor: i64,
    credential: &AccountCredential<'_>,
    project: i64,
    team: i64,
    linked: bool,
) -> Result<()> {
    let mut tx = pool.begin().await?;
    let project = write_project(&mut tx, actor, credential, project).await?;
    let org: i64 = sqlx::query_scalar("SELECT org_id FROM teams WHERE id=$1 FOR SHARE")
        .bind(team)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or(Refusal::Missing)?;
    anyhow::ensure!(
        project.owner_kind == "org" && project.owner_id == org,
        Refusal::Denied
    );
    if linked {
        sqlx::query(
            "INSERT INTO project_teams(project_id,team_id) VALUES($1,$2) ON CONFLICT DO NOTHING",
        )
        .bind(project.id)
        .bind(team)
        .execute(&mut *tx)
        .await?;
    } else {
        sqlx::query("DELETE FROM project_teams WHERE project_id=$1 AND team_id=$2")
            .bind(project.id)
            .bind(team)
            .execute(&mut *tx)
            .await?;
    }
    tx.commit().await?;
    Ok(())
}

async fn read_snapshot(pool: &PgPool) -> Result<Transaction<'_, Postgres>> {
    let mut tx = pool.begin().await?;
    sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ")
        .execute(&mut *tx)
        .await?;
    Ok(tx)
}

pub async fn by_id(pool: &PgPool, viewer: Option<i64>, id: i64) -> Result<Project> {
    by_id_at(pool, viewer, id, false).await
}

pub async fn by_id_at(
    pool: &PgPool,
    viewer: Option<i64>,
    id: i64,
    include_deleted: bool,
) -> Result<Project> {
    codeyard_db_core::queries::postgres::retry_on_conflict(|| {
        by_id_at_once(pool, viewer, id, include_deleted)
    })
    .await
}

async fn by_id_at_once(
    pool: &PgPool,
    viewer: Option<i64>,
    id: i64,
    include_deleted: bool,
) -> Result<Project> {
    let mut tx = read_snapshot(pool).await?;
    readable_at(&mut tx, viewer, id, include_deleted).await?;
    let project = sqlx::query_as("SELECT * FROM projects WHERE id=$1")
        .bind(id)
        .fetch_one(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(project)
}
pub async fn field_by_id(pool: &PgPool, viewer: Option<i64>, id: i64) -> Result<Field> {
    codeyard_db_core::queries::postgres::retry_on_conflict(|| field_by_id_once(pool, viewer, id))
        .await
}

async fn field_by_id_once(pool: &PgPool, viewer: Option<i64>, id: i64) -> Result<Field> {
    let mut tx = read_snapshot(pool).await?;
    let field: Field = sqlx::query_as("SELECT * FROM project_fields WHERE id=$1")
        .bind(id)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or(Refusal::Missing)?;
    readable(&mut tx, viewer, field.project_id).await?;
    tx.commit().await?;
    Ok(field)
}
pub async fn item_by_id(pool: &PgPool, viewer: Option<i64>, id: i64) -> Result<Item> {
    item_by_id_at(pool, viewer, id, false).await
}

pub async fn item_by_id_at(
    pool: &PgPool,
    viewer: Option<i64>,
    id: i64,
    include_deleted: bool,
) -> Result<Item> {
    codeyard_db_core::queries::postgres::retry_on_conflict(|| {
        item_by_id_at_once(pool, viewer, id, include_deleted)
    })
    .await
}

async fn item_by_id_at_once(
    pool: &PgPool,
    viewer: Option<i64>,
    id: i64,
    include_deleted: bool,
) -> Result<Item> {
    let mut tx = read_snapshot(pool).await?;
    let item: Item = sqlx::query_as(&format!("{ITEM} WHERE i.id=$1"))
        .bind(id)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or(Refusal::Missing)?;
    readable_at(&mut tx, viewer, item.project_id, include_deleted).await?;
    if let Some(repo) = item.repo_id {
        crate::postgres::projects::items::repository_read(&mut tx, viewer.unwrap_or(0), repo)
            .await?;
    }
    tx.commit().await?;
    Ok(item)
}
pub async fn metadata(
    pool: &PgPool,
    actor: i64,
    credential: &AccountCredential<'_>,
    project: i64,
    readme: Option<&str>,
    template: Option<bool>,
) -> Result<Project> {
    anyhow::ensure!(
        readme.is_none_or(|text| codeyard_model::issue_input::validate_body(text, false).is_ok()),
        Refusal::Invalid
    );
    let mut tx = pool.begin().await?;
    write_project(&mut tx, actor, credential, project).await?;
    let result = sqlx::query_as("UPDATE projects SET readme=COALESCE($2,readme),template=COALESCE($3,template),updated_at=$4 WHERE id=$1 RETURNING *").bind(project).bind(readme).bind(template).bind(chrono::Utc::now()).fetch_one(&mut *tx).await?;
    tx.commit().await?;
    Ok(result)
}
pub async fn archive_item(
    pool: &PgPool,
    actor: i64,
    credential: &AccountCredential<'_>,
    project: i64,
    item: i64,
    archived: bool,
) -> Result<Item> {
    let mut tx = pool.begin().await?;
    writable_item(&mut tx, actor, credential, project, item).await?;
    sqlx::query("UPDATE project_items SET archived=$2,archived_at=CASE WHEN $2 THEN COALESCE(archived_at,$3) ELSE NULL END,updated_at=$3 WHERE id=$1")
        .bind(item)
        .bind(archived)
        .bind(chrono::Utc::now())
        .execute(&mut *tx)
        .await?;
    let result = sqlx::query_as(&format!("{ITEM} WHERE i.id=$1"))
        .bind(item)
        .fetch_one(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(result)
}
pub async fn edit_draft(
    pool: &PgPool,
    actor: i64,
    credential: &AccountCredential<'_>,
    item: i64,
    title: Option<&str>,
    body: Option<&str>,
) -> Result<Item> {
    anyhow::ensure!(
        title.is_none_or(valid_name) && body.is_none_or(|b| b.len() <= 65536),
        Refusal::Invalid
    );
    let mut tx = pool.begin().await?;
    let project: i64 = sqlx::query_scalar(
        "SELECT project_id FROM project_items WHERE id=$1 AND draft_title IS NOT NULL",
    )
    .bind(item)
    .fetch_optional(&mut *tx)
    .await?
    .ok_or(Refusal::Missing)?;
    writable_item(&mut tx, actor, credential, project, item).await?;
    sqlx::query("UPDATE project_items SET draft_title=COALESCE($2,draft_title),draft_body=COALESCE($3,draft_body),updated_at=$4 WHERE id=$1").bind(item).bind(title).bind(body).bind(chrono::Utc::now()).execute(&mut *tx).await?;
    let result = sqlx::query_as(&format!("{ITEM} WHERE i.id=$1"))
        .bind(item)
        .fetch_one(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(result)
}
pub async fn reorder_item(
    pool: &PgPool,
    actor: i64,
    credential: &AccountCredential<'_>,
    project: i64,
    item: i64,
    after: Option<i64>,
) -> Result<Item> {
    anyhow::ensure!(after != Some(item), Refusal::Invalid);
    let mut tx = pool.begin().await?;
    writable_item(&mut tx, actor, credential, project, item).await?;
    if let Some(after) = after {
        writable_item(&mut tx, actor, credential, project, after).await?;
    }
    let mut ids: Vec<i64> =
        sqlx::query_scalar("SELECT id FROM project_items WHERE project_id=$1 ORDER BY position,id")
            .bind(project)
            .fetch_all(&mut *tx)
            .await?;
    ids.retain(|id| *id != item);
    let at = after
        .map(|id| {
            ids.iter()
                .position(|other| *other == id)
                .map(|n| n + 1)
                .ok_or(Refusal::Missing)
        })
        .transpose()?
        .unwrap_or(0);
    ids.insert(at, item);
    sqlx::query("UPDATE project_items i SET position=t.ord-1 FROM UNNEST($2::bigint[]) WITH ORDINALITY AS t(id,ord) WHERE i.id=t.id AND i.project_id=$1 AND i.position<>t.ord-1")
        .bind(project)
        .bind(&ids)
        .execute(&mut *tx)
        .await?;
    let result = sqlx::query_as(&format!("{ITEM} WHERE i.id=$1"))
        .bind(item)
        .fetch_one(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(result)
}
/// Like GitHub, copy configuration and optionally drafts, not linked issue/PR items.
pub async fn copy(
    pool: &PgPool,
    actor: i64,
    credential: &AccountCredential<'_>,
    source: i64,
    owner: Owner,
    title: &str,
    drafts: bool,
) -> Result<Project> {
    validate_project(title, None)?;
    let mut tx = pool.begin().await?;
    owner_authority(&mut tx, actor, owner, true).await?;
    let (kind, id) = owner.parts();
    anyhow::ensure!(
        codeyard_db_core::queries::postgres::users::transitions::credential_live(
            &mut tx, actor, credential, true
        )
        .await?,
        Refusal::Denied
    );
    readable(&mut tx, Some(actor), source).await?;
    let original: Project =
        sqlx::query_as("SELECT * FROM projects WHERE id=$1 AND deleted_at IS NULL")
            .bind(source)
            .fetch_one(&mut *tx)
            .await?;
    let now = chrono::Utc::now();
    let result: Project = sqlx::query_as("INSERT INTO projects(owner_user_id,owner_org_id,number,creator_id,title,description,readme,public,created_at,updated_at) SELECT $1,$2,COALESCE(MAX(number),0)+1,$3,$4,$5,$6,false,$7,$7 FROM projects WHERE owner_kind=$8 AND owner_id=$9 RETURNING *").bind(if kind=="user" {Some(id)} else {None}).bind(if kind=="org" {Some(id)} else {None}).bind(actor).bind(title).bind(&original.description).bind(&original.readme).bind(now).bind(kind).bind(id).fetch_one(&mut *tx).await?;
    let fields: Vec<Field> =
        sqlx::query_as("SELECT * FROM project_fields WHERE project_id=$1 ORDER BY id")
            .bind(source)
            .fetch_all(&mut *tx)
            .await?;
    let mut mapping = HashMap::new();
    for field in fields {
        let id: i64 = sqlx::query_scalar("INSERT INTO project_fields(project_id,name,data_type,configuration,created_at,updated_at) VALUES($1,$2,$3,$4,$5,$5) RETURNING id").bind(result.id).bind(field.name).bind(field.data_type).bind(field.configuration).bind(now).fetch_one(&mut *tx).await?;
        mapping.insert(field.id, id);
    }
    let views: Vec<crate::projects::View> =
        sqlx::query_as("SELECT * FROM project_views WHERE project_id=$1 ORDER BY number")
            .bind(source)
            .fetch_all(&mut *tx)
            .await?;
    for view in views {
        let mut config: NewView = serde_json::from_str(&view.configuration)?;
        for id in config
            .visible_fields
            .iter_mut()
            .chain(config.group_by.iter_mut())
            .chain(config.vertical_group_by.iter_mut())
            .chain(config.sort_by.iter_mut().map(|(id, _)| id))
        {
            *id = *mapping.get(id).ok_or(Refusal::Invalid)?;
        }
        sqlx::query("INSERT INTO project_views(project_id,number,creator_id,configuration,created_at,updated_at) VALUES($1,$2,$3,$4,$5,$5)").bind(result.id).bind(view.number).bind(actor).bind(serde_json::to_string(&config)?).bind(now).execute(&mut *tx).await?;
    }
    if drafts {
        let items: Vec<Item> = sqlx::query_as(&format!(
            "{ITEM} WHERE i.project_id=$1 AND i.draft_title IS NOT NULL ORDER BY i.position,i.id"
        ))
        .bind(source)
        .fetch_all(&mut *tx)
        .await?;
        for item in items {
            let values: serde_json::Map<String, serde_json::Value> =
                serde_json::from_str(&item.field_values)?;
            let values: serde_json::Map<String, serde_json::Value> = values
                .into_iter()
                .filter_map(|(id, v)| {
                    mapping
                        .get(&id.parse::<i64>().ok()?)
                        .map(|id| (id.to_string(), v))
                })
                .collect();
            sqlx::query("INSERT INTO project_items(project_id,creator_id,draft_title,draft_body,field_values,archived,archived_at,position,created_at,updated_at) VALUES($1,$2,$3,$4,$5,$6,CASE WHEN $6 THEN $8 ELSE NULL END,$7,$8,$8)").bind(result.id).bind(actor).bind(item.draft_title).bind(item.draft_body).bind(serde_json::to_string(&values)?).bind(item.archived).bind(item.position).bind(now).execute(&mut *tx).await?;
        }
    }
    tx.commit().await?;
    Ok(result)
}
