mod exports;
use crate::backend::db::projects::{
    validate_project, Field, NewField, NewView, Owner, Project, Refusal, SelectOption, Settings,
};
use crate::backend::db::users::accounts::AccountCredential;
use anyhow::Result;
pub use exports::{fields_page, list_page};
use sqlx::{PgPool, Postgres, Transaction};
mod extended;
mod items;
pub use extended::*;
pub use items::*;

async fn owner_authority(
    tx: &mut Transaction<'_, Postgres>,
    actor: i64,
    owner: Owner,
    allocate: bool,
) -> Result<()> {
    super::users::transitions::lock_membership_shared(tx).await?;
    let (kind, id) = owner.parts();
    let table = if kind == "user" { "users" } else { "organizations" };
    let lock = if allocate { "FOR UPDATE" } else { "FOR SHARE" };
    let exists: Option<i64> = sqlx::query_scalar(&format!("SELECT id FROM {table} WHERE id=$1 {lock}"))
        .bind(id).fetch_optional(&mut **tx).await?;
    anyhow::ensure!(exists.is_some(), Refusal::Missing);
    let active: bool = sqlx::query_scalar("SELECT NOT is_banned AND NOT must_change_password FROM users WHERE id=$1 FOR SHARE")
        .bind(actor).fetch_optional(&mut **tx).await?.unwrap_or(false);
    anyhow::ensure!(active, Refusal::Denied);
    match owner {
        Owner::User(id) => {
            let exists: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM users WHERE id=$1)")
                .bind(id)
                .fetch_one(&mut **tx)
                .await?;
            anyhow::ensure!(exists, Refusal::Missing);
            anyhow::ensure!(id == actor, Refusal::Denied);
        }
        Owner::Organization(id) => {
            let allowed: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM organizations o JOIN org_memberships m ON m.org_id=o.id WHERE o.id=$1 AND COALESCE((SELECT s.has_organization_projects FROM organization_settings s WHERE s.org_id=o.id),TRUE) AND m.user_id=$2 AND m.role IN ('owner','admin') AND EXISTS(SELECT 1 FROM org_memberships owners JOIN users u ON u.id=owners.user_id WHERE owners.org_id=o.id AND owners.role='owner' AND NOT u.is_banned AND NOT u.must_change_password))").bind(id).bind(actor).fetch_one(&mut **tx).await?;
            anyhow::ensure!(allowed, Refusal::Denied);
        }
    }
    Ok(())
}

async fn write_project(tx: &mut Transaction<'_, Postgres>, actor: i64, credential: &AccountCredential<'_>, id: i64) -> Result<Project> {
    super::users::transitions::lock_membership_shared(tx).await?;
    let project: Project =
        sqlx::query_as("SELECT * FROM projects WHERE id=$1 AND deleted_at IS NULL")
            .bind(id)
            .fetch_optional(&mut **tx)
            .await?
            .ok_or(Refusal::Missing)?;
    let owner = if project.owner_kind == "org" {
        Owner::Organization(project.owner_id)
    } else {
        Owner::User(project.owner_id)
    };
    owner_authority(tx, actor, owner, false).await?;
    anyhow::ensure!(super::users::transitions::credential_live(tx, actor, credential, true).await?, Refusal::Denied);
    let project =
        sqlx::query_as("SELECT * FROM projects WHERE id=$1 AND deleted_at IS NULL FOR UPDATE")
            .bind(id)
            .fetch_optional(&mut **tx)
            .await?
            .ok_or(Refusal::Missing)?;
    Ok(project)
}

async fn readable(tx: &mut Transaction<'_, Postgres>, viewer: Option<i64>, id: i64) -> Result<()> {
    readable_at(tx, viewer, id, false).await
}

async fn readable_at(tx: &mut Transaction<'_, Postgres>, viewer: Option<i64>, id: i64, include_deleted: bool) -> Result<()> {
    super::users::transitions::lock_membership_shared(tx).await?;
    let visible: Option<i64> = sqlx::query_scalar(&format!(
        "SELECT p.id FROM projects p WHERE p.id=$1 AND ($3 OR p.deleted_at IS NULL) AND {} FOR SHARE",
        crate::backend::db::projects::readable_owner_condition("$2")
    ))
    .bind(id)
    .bind(viewer)
    .bind(include_deleted)
    .fetch_optional(&mut **tx)
    .await?;
    anyhow::ensure!(visible.is_some(), Refusal::Missing);
    Ok(())
}

pub async fn create(
    pool: &PgPool,
    actor: i64,
    credential: &AccountCredential<'_>,
    owner: Owner,
    title: &str,
    description: Option<&str>,
    public: bool,
) -> Result<Project> {
    validate_project(title, description)?;
    let mut tx = pool.begin().await?;
    owner_authority(&mut tx, actor, owner, true).await?;
    let (kind, id) = owner.parts();
    anyhow::ensure!(super::users::transitions::credential_live(&mut tx, actor, credential, true).await?, Refusal::Denied);
    let now = chrono::Utc::now();
    let project: Project = sqlx::query_as("INSERT INTO projects(owner_user_id,owner_org_id,number,creator_id,title,description,public,created_at,updated_at) SELECT $1,$2,COALESCE(MAX(number),0)+1,$3,$4,$5,$6,$7,$7 FROM projects WHERE owner_kind=$8 AND owner_id=$9 RETURNING *")
        .bind(if kind == "user" {Some(id)} else {None}).bind(if kind == "org" {Some(id)} else {None}).bind(actor).bind(title).bind(description).bind(public).bind(now).bind(kind).bind(id).fetch_one(&mut *tx).await?;
    sqlx::query("INSERT INTO project_fields(project_id,name,data_type,configuration,created_at,updated_at) VALUES($1,'Title','title','{}',$2,$2)")
        .bind(project.id).bind(now).execute(&mut *tx).await?;
    let input = NewField {
        name: "Status".into(),
        data_type: "single_select".into(),
        single_select_options: Some(
            [
                ("Todo", "GRAY"),
                ("In progress", "YELLOW"),
                ("Done", "GREEN"),
            ]
            .into_iter()
            .map(|(name, color)| SelectOption {
                name: name.into(),
                color: Some(color.into()),
                description: None,
            })
            .collect(),
        ),
        iteration_configuration: None,
    };
    sqlx::query("INSERT INTO project_fields(project_id,name,data_type,configuration,created_at,updated_at) VALUES($1,$2,$3,$4,$5,$5)")
        .bind(project.id).bind(&input.name).bind(&input.data_type).bind(input.configuration()?).bind(now).execute(&mut *tx).await?;
    let fields: Vec<Field> =
        sqlx::query_as("SELECT * FROM project_fields WHERE project_id=$1 ORDER BY id")
            .bind(project.id)
            .fetch_all(&mut *tx)
            .await?;
    for (number, name, layout) in [(1_i64, "Table", "table"), (2, "Board", "board")] {
        let config = NewView {
            name: name.into(),
            layout: layout.into(),
            filter: None,
            visible_fields: fields.iter().map(|f| f.id).collect(),
            sort_by: vec![],
            group_by: vec![],
            vertical_group_by: if layout == "board" {
                vec![fields[1].id]
            } else {
                vec![]
            },
        };
        sqlx::query("INSERT INTO project_views(project_id,number,creator_id,configuration,created_at,updated_at) VALUES($1,$2,$3,$4,$5,$5)").bind(project.id).bind(number).bind(actor).bind(serde_json::to_string(&config)?).bind(now).execute(&mut *tx).await?;
    }
    tx.commit().await?;
    Ok(project)
}

pub async fn list(pool: &PgPool, viewer: Option<i64>, filter: &crate::backend::db::projects::ListFilter, offset: i64, limit: i64) -> Result<(i64, Vec<Project>)> {
    anyhow::ensure!(offset >= 0 && (0..=101).contains(&limit), Refusal::Invalid);
    let statement = filter.statement(true);
    let mut tx = pool.begin().await?;
    sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ READ ONLY").execute(&mut *tx).await?;
    let count_sql = format!("SELECT COUNT(*) {}", statement.prefix);
    let mut count = sqlx::query_scalar::<_, i64>(&count_sql).bind(viewer);
    for arg in &statement.args { count = count.bind(arg); }
    let count = count.fetch_one(&mut *tx).await?;
    let query = format!("SELECT {} {} ORDER BY {} LIMIT {} OFFSET {}", crate::backend::db::projects::SUMMARY_COLUMNS, statement.prefix, filter.order(true), statement.placeholder(statement.args.len()+2), statement.placeholder(statement.args.len()+3));
    let mut rows = sqlx::query_as(&query).bind(viewer);
    for arg in &statement.args { rows = rows.bind(arg); }
    let rows = rows.bind(limit).bind(offset).fetch_all(&mut *tx).await?;
    tx.commit().await?;
    Ok((count, rows))
}

pub async fn get(pool: &PgPool, viewer: Option<i64>, owner: Owner, number: i64) -> Result<Project> {
    let mut tx = pool.begin().await?;
    let (kind, id) = owner.parts();
    super::users::transitions::lock_membership_shared(&mut tx).await?;
    let project: Project = sqlx::query_as(&format!("SELECT p.* FROM projects p WHERE p.owner_kind=$1 AND p.owner_id=$2 AND p.number=$3 AND {} FOR SHARE", crate::backend::db::projects::readable_condition("$4")))
        .bind(kind).bind(id).bind(number).bind(viewer).fetch_optional(&mut *tx).await?.ok_or(Refusal::Missing)?;
    tx.commit().await?;
    Ok(project)
}

pub async fn save(pool: &PgPool, actor: i64, credential: &AccountCredential<'_>, project: i64, settings: &Settings) -> Result<Project> {
    let Settings {
        title,
        description,
        public,
        closed,
    } = settings;
    let mut tx = pool.begin().await?;
    write_project(&mut tx, actor, credential, project).await?;
    let now = chrono::Utc::now();
    // Omitted fields come from the locked row, not from a copy the caller read earlier.
    let project: Project = sqlx::query_as("UPDATE projects SET title=COALESCE($2,title),description=COALESCE($3,description),public=COALESCE($4,public),closed_at=CASE WHEN $5 IS NULL THEN closed_at WHEN $5 THEN COALESCE(closed_at,$6) ELSE NULL END,updated_at=$6 WHERE id=$1 RETURNING *")
        .bind(project).bind(title).bind(description).bind(public).bind(closed).bind(now).fetch_one(&mut *tx).await?;
    validate_project(&project.title, project.description.as_deref())?;
    tx.commit().await?;
    Ok(project)
}

pub async fn delete(pool: &PgPool, actor: i64, credential: &AccountCredential<'_>, project: i64) -> Result<()> {
    let mut tx = pool.begin().await?;
    write_project(&mut tx, actor, credential, project).await?;
    for table in ["project_items", "project_views", "project_fields", "project_repositories", "project_teams"] {
        sqlx::query(&format!("DELETE FROM {table} WHERE project_id=$1")).bind(project).execute(&mut *tx).await?;
    }
    sqlx::query("UPDATE projects SET title='',description=NULL,readme='',public=FALSE,deleted_at=$2,updated_at=$2 WHERE id=$1")
        .bind(project)
        .bind(chrono::Utc::now())
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(())
}

pub async fn fields(pool: &PgPool, viewer: Option<i64>, project: i64) -> Result<Vec<Field>> {
    fields_at(pool, viewer, project, false).await
}

pub async fn fields_at(pool: &PgPool, viewer: Option<i64>, project: i64, include_deleted: bool) -> Result<Vec<Field>> {
    let mut tx = pool.begin().await?;
    readable_at(&mut tx, viewer, project, include_deleted).await?;
    let fields = sqlx::query_as("SELECT * FROM project_fields WHERE project_id=$1 ORDER BY id")
        .bind(project)
        .fetch_all(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(fields)
}

pub async fn add_field(pool: &PgPool, actor: i64, credential: &AccountCredential<'_>, project: i64, input: &NewField) -> Result<Field> {
    let config = input.configuration()?;
    let mut tx = pool.begin().await?;
    write_project(&mut tx, actor, credential, project).await?;
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM project_fields WHERE project_id=$1")
        .bind(project)
        .fetch_one(&mut *tx)
        .await?;
    anyhow::ensure!(
        count < crate::backend::db::projects::MAX_FIELDS as i64,
        Refusal::Invalid
    );
    let names: Vec<String> = sqlx::query_scalar("SELECT name FROM project_fields WHERE project_id=$1")
        .bind(project).fetch_all(&mut *tx).await?;
    anyhow::ensure!(!names.iter().any(|name| name.eq_ignore_ascii_case(&input.name)), Refusal::Invalid);
    let field = sqlx::query_as("INSERT INTO project_fields(project_id,name,data_type,configuration,created_at,updated_at) VALUES($1,$2,$3,$4,$5,$5) ON CONFLICT(project_id,name) DO NOTHING RETURNING *")
        .bind(project).bind(&input.name).bind(&input.data_type).bind(config).bind(chrono::Utc::now()).fetch_optional(&mut *tx).await?.ok_or(Refusal::Invalid)?;
    sqlx::query("UPDATE projects SET updated_at=$2 WHERE id=$1")
        .bind(project)
        .bind(chrono::Utc::now())
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(field)
}
