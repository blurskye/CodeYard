use super::*;
use crate::backend::db::projects::{valid_name, Content, FieldUpdate, Item, NewView, View};
use serde_json::Value;

pub async fn item_links(pool: &SqlitePool, viewer: Option<i64>, project: i64, items: &[i64]) -> Result<Vec<crate::backend::db::projects::ItemLink>> {
    anyhow::ensure!(items.len() <= 50, Refusal::Invalid);
    let mut tx = pool.begin().await?;
    readable(&mut tx, viewer, project).await?;
    let links = sqlx::query_as(&format!("SELECT i.id,COALESCE(owner_user.username,owner_org.name),r.name,CASE WHEN i.issue_id IS NOT NULL THEN 'issues' ELSE 'pulls' END,COALESCE(issues.number,pr.number) FROM project_items i JOIN repositories r ON r.id=i.repo_id LEFT JOIN users u ON u.id=?2 LEFT JOIN users owner_user ON owner_user.id=r.owner_user_id LEFT JOIN organizations owner_org ON owner_org.id=r.owner_org_id LEFT JOIN issues ON issues.id=i.issue_id LEFT JOIN pull_requests pr ON pr.id=i.pull_request_id WHERE i.project_id=?1 AND i.id IN (SELECT value FROM json_each(?3)) AND {}", super::super::repositories::access::readable_condition()))
        .bind(project).bind(viewer).bind(serde_json::to_string(items)?).fetch_all(&mut *tx).await?;
    tx.commit().await?;
    Ok(links)
}

pub async fn content_items(
    pool: &SqlitePool,
    viewer: Option<i64>,
    content: &Content,
    include_archived: bool,
    offset: i64,
    limit: i64,
) -> Result<(i64, Vec<Item>)> {
    anyhow::ensure!(offset >= 0 && (0..=100).contains(&limit), Refusal::Invalid);
    let (issue, pull) = match content {
        Content::Issue(id) => (Some(*id), None),
        Content::PullRequest(id) => (None, Some(*id)),
        Content::Draft { .. } => return Ok((0, vec![])),
    };
    let joins = "JOIN projects p ON p.id=i.project_id JOIN repositories r ON r.id=i.repo_id LEFT JOIN users u ON u.id=$1";
    let condition = format!("(i.issue_id=$2 OR i.pull_request_id=$3) AND ($4 OR NOT i.archived) AND ({}) AND ({})", crate::backend::db::projects::readable_condition("$1"), super::super::repositories::access::readable_condition());
    let mut tx = pool.begin().await?;
    let count: i64 = sqlx::query_scalar(&format!("SELECT COUNT(*) FROM project_items i {joins} WHERE {condition}"))
        .bind(viewer).bind(issue).bind(pull).bind(include_archived).fetch_one(&mut *tx).await?;
    let rows = sqlx::query_as(&format!("{ITEM} {joins} WHERE {condition} ORDER BY i.id LIMIT $5 OFFSET $6"))
        .bind(viewer).bind(issue).bind(pull).bind(include_archived).bind(limit).bind(offset).fetch_all(&mut *tx).await?;
    tx.commit().await?;
    Ok((count, rows))
}

pub(super) const ITEM: &str = "SELECT i.id,i.project_id,i.creator_id,COALESCE(issues.repo_id,pr.repo_id,i.repo_id) AS repo_id,i.issue_id,i.pull_request_id,i.draft_title,i.draft_body,i.field_values,i.archived,i.position,i.created_at,i.updated_at,COALESCE(issues.title,pr.title,i.draft_title) AS title,COALESCE(issues.state,pr.state,'open') AS state FROM project_items i LEFT JOIN issues ON issues.id=i.issue_id LEFT JOIN pull_requests pr ON pr.id=i.pull_request_id";

pub(super) async fn repository_read(
    tx: &mut Transaction<'_, Sqlite>,
    actor: i64,
    repo: i64,
) -> Result<()> {
    let visible: bool = sqlx::query_scalar(&format!("SELECT EXISTS(SELECT 1 FROM repositories r LEFT JOIN users u ON u.id=$1 WHERE r.id=$2 AND {})", super::super::repositories::access::readable_condition()))
        .bind(actor).bind(repo).fetch_one(&mut **tx).await?;
    anyhow::ensure!(visible, Refusal::Missing);
    Ok(())
}

pub(super) async fn writable_item(
    tx: &mut Transaction<'_, Sqlite>,
    actor: i64,
    credential: &AccountCredential<'_>,
    project: i64,
    id: i64,
) -> Result<Item> {
    let item: Item = sqlx::query_as(&format!("{ITEM} WHERE i.id=$1 AND i.project_id=$2"))
        .bind(id)
        .bind(project)
        .fetch_optional(&mut **tx)
        .await?
        .ok_or(Refusal::Missing)?;
    if let Some(repo) = item.repo_id {
        repository_read(tx, actor, repo).await?;
    }
    write_project(tx, actor, credential, project).await?;
    let item = sqlx::query_as(&format!("{ITEM} WHERE i.id=$1 AND i.project_id=$2"))
        .bind(id)
        .bind(project)
        .fetch_optional(&mut **tx)
        .await?
        .ok_or(Refusal::Missing)?;
    Ok(item)
}

pub async fn can_write(pool: &SqlitePool, actor: i64, owner: Owner) -> Result<bool> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let result = owner_authority(&mut tx, actor, owner).await;
    match result {
        Ok(()) => {
            tx.commit().await?;
            Ok(true)
        }
        Err(error)
            if matches!(
                error.downcast_ref::<Refusal>(),
                Some(Refusal::Denied | Refusal::Missing)
            ) =>
        {
            Ok(false)
        }
        Err(error) => Err(error),
    }
}

pub async fn delete_field(pool: &SqlitePool, actor: i64, credential: &AccountCredential<'_>, project: i64, field: i64) -> Result<()> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    write_project(&mut tx, actor, credential, project).await?;
    let name: Option<String> =
        sqlx::query_scalar("SELECT name FROM project_fields WHERE id=?1 AND project_id=?2")
            .bind(field)
            .bind(project)
            .fetch_optional(&mut *tx)
            .await?;
    let name = name.ok_or(Refusal::Invalid)?;
    let views: Vec<View> = sqlx::query_as("SELECT * FROM project_views WHERE project_id=$1")
        .bind(project)
        .fetch_all(&mut *tx)
        .await?;
    for view in views {
        let mut config: NewView = serde_json::from_str(&view.configuration)?;
        // Saved views drop the field with it, so it can be deleted; title protection stays below.
        config.visible_fields.retain(|id| *id != field);
        config.group_by.retain(|id| *id != field);
        config.vertical_group_by.retain(|id| *id != field);
        config.sort_by.retain(|(id, _)| *id != field);
        if let Some(filter) = &config.filter {
            config.filter = Some(crate::backend::projects::without_field_terms(
                filter, &name,
            )?);
        }
        sqlx::query("UPDATE project_views SET configuration=?2 WHERE id=?1")
            .bind(view.id)
            .bind(serde_json::to_string(&config)?)
            .execute(&mut *tx)
            .await?;
    }
    let count = sqlx::query(
        "DELETE FROM project_fields WHERE id=$1 AND project_id=$2 AND data_type <> 'title'",
    )
    .bind(field)
    .bind(project)
    .execute(&mut *tx)
    .await?
    .rows_affected();
    anyhow::ensure!(count == 1, Refusal::Invalid);
    tx.commit().await?;
    Ok(())
}

pub async fn add_item(
    pool: &SqlitePool,
    actor: i64,
    credential: &AccountCredential<'_>,
    project: i64,
    content: &Content,
) -> Result<Item> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let (issue, pull, repo, title, body) = match content {
        Content::Issue(id) | Content::PullRequest(id) => {
            let table = if matches!(content, Content::Issue(_)) {
                "issues"
            } else {
                "pull_requests"
            };
            let repo: i64 = sqlx::query_scalar(&format!("SELECT repo_id FROM {table} WHERE id=$1"))
                .bind(id)
                .fetch_optional(&mut *tx)
                .await?
                .ok_or(Refusal::Missing)?;
            repository_read(&mut tx, actor, repo).await?;
            (
                matches!(content, Content::Issue(_)).then_some(*id),
                matches!(content, Content::PullRequest(_)).then_some(*id),
                Some(repo),
                None,
                None,
            )
        }
        Content::Draft { title, body } => {
            anyhow::ensure!(
                valid_name(title) && body.as_ref().is_none_or(|b| b.len() <= 65536),
                Refusal::Invalid
            );
            (None, None, None, Some(title.as_str()), body.as_deref())
        }
    };
    write_project(&mut tx, actor, credential, project).await?;
    let now = chrono::Utc::now();
    // A new item goes after the others, also after a reorder.
    let id: i64 = sqlx::query_scalar("INSERT INTO project_items(project_id,creator_id,repo_id,issue_id,pull_request_id,draft_title,draft_body,created_at,updated_at,position) VALUES($1,$2,$3,$4,$5,$6,$7,$8,$8,(SELECT COALESCE(MAX(position)+1,0) FROM project_items WHERE project_id=$1)) ON CONFLICT DO NOTHING RETURNING id")
        .bind(project).bind(actor).bind(repo).bind(issue).bind(pull).bind(title).bind(body).bind(now).fetch_optional(&mut *tx).await?.unwrap_or(0);
    let id = if id == 0 {
        sqlx::query_scalar("SELECT id FROM project_items WHERE project_id=$1 AND (issue_id=$2 OR pull_request_id=$3)").bind(project).bind(issue).bind(pull).fetch_one(&mut *tx).await?
    } else {
        id
    };
    let item = sqlx::query_as(&format!("{ITEM} WHERE i.id=$1"))
        .bind(id)
        .fetch_one(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(item)
}

pub async fn content_items_page(pool: &SqlitePool, viewer: Option<i64>, content: &Content, include_archived: bool, offset: i64, limit: i64, count: bool) -> Result<(i64, Vec<Item>)> {
    anyhow::ensure!(offset >= 0 && (0..=100).contains(&limit), Refusal::Invalid);
    let (issue, pull) = match content { Content::Issue(id) => (Some(*id), None), Content::PullRequest(id) => (None, Some(*id)), _ => return Ok((0, Vec::new())) };
    let joins = format!(" JOIN projects p ON p.id=i.project_id LEFT JOIN repositories r ON r.id=COALESCE(issues.repo_id,pr.repo_id,i.repo_id) LEFT JOIN users u ON u.id=?3 WHERE (i.issue_id=?1 OR i.pull_request_id=?2) AND (?4 OR NOT i.archived) AND {} AND (COALESCE(issues.repo_id,pr.repo_id,i.repo_id) IS NULL OR {})", crate::backend::db::projects::readable_condition("?3"), super::super::repositories::access::readable_condition());
    let mut tx = pool.begin().await?;
    let total = if count { sqlx::query_scalar(&format!("SELECT COUNT(*) FROM ({ITEM}{joins}) visible")).bind(issue).bind(pull).bind(viewer).bind(include_archived).fetch_one(&mut *tx).await? } else { 0 };
    let rows = if limit == 0 { Vec::new() } else { sqlx::query_as(&format!("{ITEM}{joins} ORDER BY i.id LIMIT ?5 OFFSET ?6")).bind(issue).bind(pull).bind(viewer).bind(include_archived).bind(limit).bind(offset).fetch_all(&mut *tx).await? };
    tx.commit().await?;
    Ok((total, rows))
}

pub async fn items_window(pool: &SqlitePool, viewer: Option<i64>, window: &crate::backend::db::projects::ItemWindow) -> Result<(i64, Vec<Item>)> {
    anyhow::ensure!(window.offset >= 0 && (0..=100).contains(&window.limit), Refusal::Invalid);
    let mut tx = pool.begin().await?;
    readable_at(&mut tx, viewer, window.project, window.include_deleted).await?;
    let joins = format!(" LEFT JOIN repositories r ON r.id=COALESCE(issues.repo_id,pr.repo_id,i.repo_id) LEFT JOIN users u ON u.id=?2 WHERE i.project_id=?1 AND (?3 IS NULL OR i.archived=?3) AND (COALESCE(issues.repo_id,pr.repo_id,i.repo_id) IS NULL OR {})", super::super::repositories::access::readable_condition());
    let total = if window.count { sqlx::query_scalar(&format!("SELECT COUNT(*) FROM ({ITEM}{joins}) visible")).bind(window.project).bind(viewer).bind(window.archived).fetch_one(&mut *tx).await? } else { 0 };
    let direction = if window.descending { "DESC" } else { "ASC" };
    let rows = if window.limit == 0 { Vec::new() } else { sqlx::query_as(&format!("{ITEM}{joins} ORDER BY i.position {direction},i.id {direction} LIMIT ?4 OFFSET ?5")).bind(window.project).bind(viewer).bind(window.archived).bind(window.limit).bind(window.offset).fetch_all(&mut *tx).await? };
    tx.commit().await?;
    Ok((total, rows))
}

pub async fn items(pool: &SqlitePool, viewer: Option<i64>, project: i64) -> Result<Vec<Item>> {
    items_at(pool, viewer, project, false).await
}

pub async fn items_at(pool: &SqlitePool, viewer: Option<i64>, project: i64, include_deleted: bool) -> Result<Vec<Item>> {
    let mut tx = pool.begin().await?;
    readable_at(&mut tx, viewer, project, include_deleted).await?;
    let items = sqlx::query_as(&format!("{ITEM} LEFT JOIN repositories r ON r.id=COALESCE(issues.repo_id,pr.repo_id,i.repo_id) LEFT JOIN users u ON u.id=?2 WHERE i.project_id=?1 AND (COALESCE(issues.repo_id,pr.repo_id,i.repo_id) IS NULL OR {}) ORDER BY i.position,i.id", super::super::repositories::access::readable_condition()))
        .bind(project).bind(viewer).fetch_all(&mut *tx).await?;
    tx.commit().await?;
    Ok(items)
}

pub async fn update_item(
    pool: &SqlitePool,
    actor: i64,
    credential: &AccountCredential<'_>,
    project: i64,
    item: i64,
    updates: &[FieldUpdate],
) -> Result<Item> {
    anyhow::ensure!(
        updates.len() <= crate::backend::db::projects::MAX_FIELDS,
        Refusal::Invalid
    );
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let existing = writable_item(&mut tx, actor, credential, project, item).await?;
    let fields: Vec<Field> = sqlx::query_as("SELECT * FROM project_fields WHERE project_id=$1")
        .bind(project)
        .fetch_all(&mut *tx)
        .await?;
    let mut values: Value = serde_json::from_str(&existing.field_values)?;
    let mut seen = std::collections::HashSet::new();
    for update in updates {
        anyhow::ensure!(seen.insert(update.id), Refusal::Invalid);
        let field = fields
            .iter()
            .find(|f| f.id == update.id)
            .ok_or(Refusal::Invalid)?;
        crate::backend::db::projects::validate_value(field, &update.value)?;
        if update.value.is_null() {
            values
                .as_object_mut()
                .ok_or(Refusal::Invalid)?
                .remove(&field.id.to_string());
        } else {
            values[field.id.to_string()] = update.value.clone();
        }
    }
    sqlx::query(
        "UPDATE project_items SET field_values=$3,updated_at=$4 WHERE id=$1 AND project_id=$2",
    )
    .bind(item)
    .bind(project)
    .bind(values.to_string())
    .bind(chrono::Utc::now())
    .execute(&mut *tx)
    .await?;
    let item = sqlx::query_as(&format!("{ITEM} WHERE i.id=$1"))
        .bind(item)
        .fetch_one(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(item)
}

pub async fn delete_item(pool: &SqlitePool, actor: i64, credential: &AccountCredential<'_>, project: i64, item: i64) -> Result<()> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    writable_item(&mut tx, actor, credential, project, item).await?;
    sqlx::query("DELETE FROM project_items WHERE id=$1 AND project_id=$2")
        .bind(item)
        .bind(project)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(())
}

pub async fn create_view(
    pool: &SqlitePool,
    actor: i64,
    credential: &AccountCredential<'_>,
    project: i64,
    input: &NewView,
) -> Result<View> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    write_project(&mut tx, actor, credential, project).await?;
    let fields = sqlx::query_as("SELECT * FROM project_fields WHERE project_id=$1 ORDER BY id")
        .bind(project)
        .fetch_all(&mut *tx)
        .await?;
    let mut input = input.clone();
    let fields: Vec<Field> = fields;
    if input.visible_fields.is_empty() {
        input.visible_fields = fields.iter().map(|f| f.id).collect();
    }
    if input.layout == "board" && input.vertical_group_by.is_empty() {
        input.vertical_group_by = fields
            .iter()
            .find(|f| f.data_type == "single_select")
            .map(|f| vec![f.id])
            .unwrap_or_default();
    }
    input.validate(&fields)?;
    let now = chrono::Utc::now();
    let view = sqlx::query_as("INSERT INTO project_views(project_id,number,creator_id,configuration,created_at,updated_at) SELECT $1,COALESCE(MAX(number),0)+1,$2,$3,$4,$4 FROM project_views WHERE project_id=$1 RETURNING *")
        .bind(project).bind(actor).bind(serde_json::to_string(&input)?).bind(now).fetch_one(&mut *tx).await?;
    tx.commit().await?;
    Ok(view)
}

pub async fn views(pool: &SqlitePool, viewer: Option<i64>, project: i64) -> Result<Vec<View>> {
    let mut tx = pool.begin().await?;
    readable(&mut tx, viewer, project).await?;
    let views = sqlx::query_as("SELECT * FROM project_views WHERE project_id=$1 ORDER BY number")
        .bind(project)
        .fetch_all(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(views)
}

pub async fn link_repository(
    pool: &SqlitePool,
    actor: i64,
    credential: &AccountCredential<'_>,
    project: i64,
    repo: i64,
    linked: bool,
) -> Result<()> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    anyhow::ensure!(
        super::super::discussions::lock_admin_authority(&mut tx, actor, repo).await?,
        Refusal::Denied
    );
    write_project(&mut tx, actor, credential, project).await?;
    if linked {
        sqlx::query("INSERT INTO project_repositories(project_id,repo_id) VALUES($1,$2) ON CONFLICT DO NOTHING").bind(project).bind(repo).execute(&mut *tx).await?;
    } else {
        sqlx::query("DELETE FROM project_repositories WHERE project_id=$1 AND repo_id=$2")
            .bind(project)
            .bind(repo)
            .execute(&mut *tx)
            .await?;
    }
    tx.commit().await?;
    Ok(())
}
