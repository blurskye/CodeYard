use crate::backend::db::issue_attributes::{
    AssignedType, Attribute, Definition, FieldChanges, FieldValue, Refusal, SelectOption, ValueInput,
};
use anyhow::Result;
use serde_json::{json, Value};
use sqlx::{PgConnection, PgPool, Postgres, Transaction};
pub(crate) async fn catalog_connection(
    c: &mut PgConnection,
    repo: i64,
    viewer: Option<i64>,
    kind: &str,
) -> Result<Vec<Attribute>> {
    let owner: Option<(Option<i64>, Option<i64>)> =
        sqlx::query_as("SELECT owner_user_id,owner_org_id FROM repositories WHERE id=$1")
            .bind(repo)
            .fetch_optional(&mut *c)
            .await?;
    match owner {
        Some((user, org)) => catalog_owner(c, user, org, viewer, kind).await,
        None => Ok(Vec::new()),
    }
}
async fn catalog_owner(
    c: &mut PgConnection,
    user: Option<i64>,
    org: Option<i64>,
    viewer: Option<i64>,
    kind: &str,
) -> Result<Vec<Attribute>> {
    let mut attributes:Vec<Attribute>=sqlx::query_as("SELECT a.* FROM issue_attributes a WHERE (a.owner_user_id=$1 OR a.owner_org_id=$2) AND a.kind=$4 AND (a.kind='type' OR a.visibility='all' OR EXISTS(SELECT 1 FROM org_memberships m JOIN users u ON u.id=m.user_id WHERE m.org_id=a.owner_org_id AND m.user_id=$3 AND NOT u.is_banned AND NOT u.must_change_password AND EXISTS(SELECT 1 FROM org_memberships o JOIN users active ON active.id=o.user_id WHERE o.org_id=m.org_id AND o.role='owner' AND NOT active.is_banned AND NOT active.must_change_password))) ORDER BY a.id").bind(user).bind(org).bind(viewer).bind(kind).fetch_all(&mut *c).await?;
    // One read for every attribute's options, in each attribute's display order.
    let options:Vec<(i64,i64,String,String,Option<String>,i64)>=sqlx::query_as("SELECT o.attribute_id,o.id,o.name,o.color,o.description,o.priority FROM issue_attribute_options o JOIN issue_attributes a ON a.id=o.attribute_id WHERE (a.owner_user_id=$1 OR a.owner_org_id=$2) AND a.kind=$3 ORDER BY o.priority,o.id").bind(user).bind(org).bind(kind).fetch_all(&mut *c).await?;
    for (attribute, id, name, color, description, priority) in options {
        if let Some(attribute) = attributes.iter_mut().find(|a| a.id == attribute) {
            attribute.options.push(SelectOption {
                id,
                name,
                color,
                description,
                priority,
            });
        }
    }
    Ok(attributes)
}
pub async fn catalog(
    pool: &PgPool,
    repo: i64,
    viewer: Option<i64>,
    kind: &str,
) -> Result<Vec<Attribute>> {
    let mut tx = pool.begin().await?;
    sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ READ ONLY")
        .execute(&mut *tx)
        .await?;
    let result = catalog_connection(&mut tx, repo, viewer, kind).await?;
    tx.commit().await?;
    Ok(result)
}
pub(crate) async fn assigned_connection(
    c: &mut PgConnection,
    repo: i64,
    issue: i64,
) -> Result<Option<Attribute>> {
    Ok(sqlx::query_as("SELECT a.* FROM issue_type_assignments s JOIN issue_attributes a ON a.id=s.attribute_id JOIN issues i ON i.id=s.issue_id JOIN repositories r ON r.id=i.repo_id WHERE i.id=$1 AND r.id=$2 AND a.kind='type' AND (a.owner_user_id=r.owner_user_id OR a.owner_org_id=r.owner_org_id)").bind(issue).bind(repo).fetch_optional(c).await?)
}
pub async fn assigned_type(pool: &PgPool, repo: i64, issue: i64) -> Result<Option<Attribute>> {
    assigned_connection(&mut *pool.acquire().await?, repo, issue).await
}
pub async fn assigned_types(
    pool: &PgPool,
    repo: i64,
    ids: &[i64],
) -> Result<Vec<AssignedType>> {
    anyhow::ensure!(ids.len() <= 500, Refusal::Invalid);
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    Ok(sqlx::query_as("SELECT s.issue_id, a.* FROM issue_type_assignments s JOIN issue_attributes a ON a.id=s.attribute_id JOIN issues i ON i.id=s.issue_id JOIN repositories r ON r.id=i.repo_id WHERE r.id=$1 AND i.id IN (SELECT unnest($2::BIGINT[])) AND a.kind='type' AND (a.owner_user_id=r.owner_user_id OR a.owner_org_id=r.owner_org_id)")
        .bind(repo)
        .bind(ids)
        .fetch_all(pool)
        .await?)
}
pub async fn values(
    pool: &PgPool,
    repo: i64,
    issue: i64,
    viewer: Option<i64>,
) -> Result<Vec<FieldValue>> {
    let mut tx = pool.begin().await?;
    sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ READ ONLY")
        .execute(&mut *tx)
        .await?;
    let rows:Vec<(i64,i64,String)>=sqlx::query_as("SELECT v.id,v.attribute_id,v.value_json FROM issue_field_values v JOIN issues i ON i.id=v.issue_id WHERE i.id=$1 AND i.repo_id=$2 ORDER BY v.attribute_id").bind(issue).bind(repo).fetch_all(&mut *tx).await?;
    let catalog = if rows.is_empty() {
        Vec::new()
    } else {
        catalog_connection(&mut tx, repo, viewer, "field").await?
    };
    let mut result = Vec::new();
    for (id, field, value) in rows {
        if let Some(attribute) = catalog.iter().find(|a| a.id == field) {
            result.push(FieldValue {
                id,
                attribute: attribute.clone(),
                value: serde_json::from_str(&value)?,
            });
        }
    }
    tx.commit().await?;
    Ok(result)
}
async fn owner_connection(c: &mut PgConnection, repo: i64, actor: i64) -> Result<bool> {
    Ok(sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM repositories r JOIN users u ON u.id=$2 WHERE r.id=$1 AND NOT u.is_banned AND NOT u.must_change_password AND ((r.owner_kind='user' AND (r.owner_user_id=u.id OR u.is_admin)) OR (r.owner_kind='org' AND EXISTS(SELECT 1 FROM org_memberships m WHERE m.org_id=r.owner_org_id AND m.user_id=u.id AND m.role='owner') AND EXISTS(SELECT 1 FROM org_memberships m JOIN users active ON active.id=m.user_id WHERE m.org_id=r.owner_org_id AND m.role='owner' AND NOT active.is_banned AND NOT active.must_change_password))))").bind(repo).bind(actor).fetch_one(c).await?)
}
pub async fn owner_can_configure(pool: &PgPool, repo: i64, actor: i64) -> Result<bool> {
    owner_connection(&mut *pool.acquire().await?, repo, actor).await
}
async fn begin_write(
    pool: &PgPool,
    repo: i64,
    actor: i64,
    issue: Option<i64>,
    owner: bool,
) -> Result<Transaction<'_, Postgres>> {
    let mut tx = pool.begin().await?;
    anyhow::ensure!(
        super::discussions::lock_write_authority(&mut tx, actor, repo).await?,
        Refusal::Denied
    );
    lock_metadata(&mut tx).await?;
    if owner {
        anyhow::ensure!(
            owner_connection(&mut tx, repo, actor).await?,
            Refusal::Denied
        );
    }
    if let Some(issue) = issue {
        let id: Option<i64> =
            sqlx::query_scalar("SELECT id FROM issues WHERE id=$1 AND repo_id=$2 FOR UPDATE")
                .bind(issue)
                .bind(repo)
                .fetch_optional(&mut *tx)
                .await?;
        anyhow::ensure!(id.is_some(), Refusal::Missing);
    }
    Ok(tx)
}
pub(crate) async fn lock_metadata(tx: &mut Transaction<'_, Postgres>) -> Result<()> {
    sqlx::query_scalar::<_, i64>("SELECT id FROM issue_attribute_guard WHERE id=1 FOR UPDATE")
        .fetch_one(&mut **tx)
        .await?;
    Ok(())
}
pub async fn configure(pool: &PgPool, repo: i64, actor: i64, d: &Definition) -> Result<i64> {
    d.validate()?;
    let mut tx = begin_write(pool, repo, actor, None, true).await?;
    let (user, org): (Option<i64>, Option<i64>) =
        sqlx::query_as("SELECT owner_user_id,owner_org_id FROM repositories WHERE id=$1")
            .bind(repo)
            .fetch_one(&mut *tx)
            .await?;
    configure_owner(tx, actor, (user, org), d)
        .await
        .map(|attribute| attribute.id)
}
async fn configure_owner(
    mut tx: Transaction<'_, Postgres>,
    actor: i64,
    (user, org): (Option<i64>, Option<i64>),
    d: &Definition,
) -> Result<Attribute> {
    anyhow::ensure!(d.kind != "field" || org.is_some(), Refusal::Invalid);
    let old = if let Some(id) = d.id {
        let old = catalog_owner(&mut tx, user, org, Some(actor), &d.kind)
            .await?
            .into_iter()
            .find(|a| a.id == id)
            .ok_or(Refusal::Missing)?;
        anyhow::ensure!(old.data_type == d.data_type, Refusal::Invalid);
        Some(old)
    } else {
        None
    };
    let catalog = catalog_owner(&mut tx, user, org, Some(actor), &d.kind).await?;
    anyhow::ensure!(old.is_some() || catalog.len() < 25, Refusal::Limit);
    anyhow::ensure!(
        !catalog
            .iter()
            .any(|a| Some(a.id) != d.id && a.name.to_lowercase() == d.name.to_lowercase()),
        Refusal::Invalid
    );
    if old.is_none() {
        anyhow::ensure!(d.options.iter().all(|o| o.id.is_none()), Refusal::Invalid);
    }
    let id = if let Some(id) = d.id {
        sqlx::query("UPDATE issue_attributes SET name=$2,description=$3,visibility=$4,is_enabled=$5,color=$6,updated_at=clock_timestamp() WHERE id=$1").bind(id).bind(&d.name).bind(&d.description).bind(&d.visibility).bind(d.is_enabled).bind(&d.color).execute(&mut *tx).await?;
        id
    } else {
        sqlx::query_scalar("INSERT INTO issue_attributes(owner_user_id,owner_org_id,kind,name,description,data_type,visibility,is_enabled,color) VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9) RETURNING id").bind(user).bind(org).bind(&d.kind).bind(&d.name).bind(&d.description).bind(&d.data_type).bind(&d.visibility).bind(d.is_enabled).bind(&d.color).fetch_one(&mut *tx).await?
    };
    let old_options: Vec<SelectOption> = old.map(|a| a.options).unwrap_or_default();
    for option in &d.options {
        anyhow::ensure!(
            option
                .id
                .is_none_or(|id| old_options.iter().any(|o| o.id == id)),
            Refusal::Invalid
        );
    }
    // Temporary names cannot collide with validated names, including when two options swap names.
    sqlx::query(
        "UPDATE issue_attribute_options SET name=$2||CAST(id AS TEXT) WHERE attribute_id=$1",
    )
    .bind(id)
    .bind("\u{1f}")
    .execute(&mut *tx)
    .await?;
    let mut removed_option = false;
    for option in &old_options {
        if !d.options.iter().any(|o| o.id == Some(option.id)) {
            removed_option = true;
            sqlx::query("DELETE FROM issue_attribute_options WHERE id=$1")
                .bind(option.id)
                .execute(&mut *tx)
                .await?;
        }
    }
    let mut valid = Vec::new();
    for (position, option) in d.options.iter().enumerate() {
        let option_id = if let Some(id) = option.id {
            sqlx::query(
                "UPDATE issue_attribute_options SET name=$2,color=$3,priority=$4,description=$5 WHERE id=$1",
            )
            .bind(id)
            .bind(&option.name)
            .bind(&option.color)
            .bind(option.priority.unwrap_or(position as i64))
            .bind(&option.description)
            .execute(&mut *tx)
            .await?;
            id
        } else {
            sqlx::query_scalar("INSERT INTO issue_attribute_options(attribute_id,name,color,priority,description) VALUES ($1,$2,$3,$4,$5) RETURNING id").bind(id).bind(&option.name).bind(&option.color).bind(option.priority.unwrap_or(position as i64)).bind(&option.description).fetch_one(&mut *tx).await?
        };
        valid.push(option_id);
    }
    // Stored selections can only point at removed options, so scan them only then.
    if removed_option
        && matches!(
            d.data_type.as_deref(),
            Some("single_select" | "multi_select")
        )
    {
        let rows: Vec<(i64, String)> =
            sqlx::query_as("SELECT id,value_json FROM issue_field_values WHERE attribute_id=$1")
                .bind(id)
                .fetch_all(&mut *tx)
                .await?;
        for (value_id, text) in rows {
            let value: Value = serde_json::from_str(&text)?;
            let next = if d.data_type.as_deref() == Some("single_select") {
                value
                    .as_i64()
                    .filter(|id| valid.contains(id))
                    .map(Value::from)
            } else {
                let ids = value
                    .as_array()
                    .ok_or(Refusal::Invalid)?
                    .iter()
                    .filter(|v| v.as_i64().is_some_and(|id| valid.contains(&id)))
                    .cloned()
                    .collect::<Vec<_>>();
                if ids.is_empty() {
                    None
                } else {
                    Some(Value::Array(ids))
                }
            };
            if next.as_ref() == Some(&value) {
                continue;
            }
            if let Some(next) = next {
                sqlx::query("UPDATE issue_field_values SET value_json=$2 WHERE id=$1")
                    .bind(value_id)
                    .bind(serde_json::to_string(&next)?)
                    .execute(&mut *tx)
                    .await?;
            } else {
                sqlx::query("DELETE FROM issue_field_values WHERE id=$1")
                    .bind(value_id)
                    .execute(&mut *tx)
                    .await?;
            }
        }
    }
    let attribute = catalog_owner(&mut tx, user, org, Some(actor), &d.kind)
        .await?
        .into_iter()
        .find(|attribute| attribute.id == id)
        .ok_or(Refusal::Missing)?;
    tx.commit().await?;
    Ok(attribute)
}
pub async fn organization_catalog(
    pool: &PgPool,
    org: i64,
    actor: i64,
    kind: &str,
) -> Result<Vec<Attribute>> {
    use crate::backend::db::organizations::OrganizationOutcome;
    let mut tx = pool.begin().await?;
    sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ READ ONLY")
        .execute(&mut *tx)
        .await?;
    match super::organizations::authority::read_authority(&mut tx, actor, org).await? {
        OrganizationOutcome::Applied(_) => {}
        OrganizationOutcome::Missing => return Err(Refusal::Missing.into()),
        _ => return Err(Refusal::Denied.into()),
    }
    let result = catalog_owner(&mut tx, None, Some(org), Some(actor), kind).await?;
    tx.commit().await?;
    Ok(result)
}
async fn begin_organization_write(
    pool: &PgPool,
    org: i64,
    actor: crate::backend::db::users::tokens::Actor,
) -> Result<Transaction<'_, Postgres>> {
    use crate::backend::db::{models::OrgRole, organizations::OrganizationOutcome};
    let mut tx = pool.begin().await?;
    match super::organizations::authority::authority(&mut tx, actor.user_id, org).await? {
        OrganizationOutcome::Applied(OrgRole::Owner) => {}
        OrganizationOutcome::Missing => return Err(Refusal::Missing.into()),
        _ => return Err(Refusal::Denied.into()),
    }
    lock_metadata(&mut tx).await?;
    if let Some(token) = actor.token_id {
        anyhow::ensure!(super::users::tokens::permits_write(&mut tx, actor.user_id, token, None, Some(org), "organization_administration").await?, Refusal::Denied);
    }
    Ok(tx)
}
pub async fn configure_organization(
    pool: &PgPool,
    org: i64,
    actor: impl Into<crate::backend::db::users::tokens::Actor>,
    d: &Definition,
) -> Result<Attribute> {
    d.validate()?;
    let actor = actor.into();
    let tx = begin_organization_write(pool, org, actor).await?;
    configure_owner(tx, actor.user_id, (None, Some(org)), d).await
}
pub async fn create_organization_field(
    pool: &PgPool,
    org: i64,
    actor: impl Into<crate::backend::db::users::tokens::Actor>,
    d: &Definition,
) -> Result<Attribute> {
    anyhow::ensure!(d.id.is_none() && d.kind == "field", Refusal::Invalid);
    d.validate()?;
    let actor = actor.into();
    let tx = begin_organization_write(pool, org, actor).await?;
    configure_owner(tx, actor.user_id, (None, Some(org)), d).await
}
pub async fn patch_organization_field(
    pool: &PgPool,
    org: i64,
    actor: impl Into<crate::backend::db::users::tokens::Actor>,
    id: i64,
    changes: &FieldChanges,
) -> Result<Attribute> {
    let actor = actor.into();
    let mut tx = begin_organization_write(pool, org, actor).await?;
    let current = catalog_owner(&mut tx, None, Some(org), Some(actor.user_id), "field")
        .await?
        .into_iter()
        .find(|attribute| attribute.id == id)
        .ok_or(Refusal::Missing)?;
    let definition = changes.apply_to(&current);
    definition.validate()?;
    configure_owner(tx, actor.user_id, (None, Some(org)), &definition).await
}
pub async fn delete_organization_definition(
    pool: &PgPool,
    org: i64,
    actor: impl Into<crate::backend::db::users::tokens::Actor>,
    id: i64,
    kind: &str,
) -> Result<()> {
    let mut tx = begin_organization_write(pool, org, actor.into()).await?;
    let deleted =
        sqlx::query("DELETE FROM issue_attributes WHERE id=$1 AND owner_org_id=$2 AND kind=$3")
            .bind(id)
            .bind(org)
            .bind(kind)
            .execute(&mut *tx)
            .await?;
    anyhow::ensure!(deleted.rows_affected() == 1, Refusal::Missing);
    tx.commit().await?;
    Ok(())
}
pub async fn delete_definition(pool: &PgPool, repo: i64, actor: i64, id: i64) -> Result<()> {
    let mut tx = begin_write(pool, repo, actor, None, true).await?;
    let exists:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM issue_attributes a JOIN repositories r ON r.id=$2 WHERE a.id=$1 AND (a.owner_user_id=r.owner_user_id OR a.owner_org_id=r.owner_org_id))").bind(id).bind(repo).fetch_one(&mut *tx).await?;
    anyhow::ensure!(exists, Refusal::Missing);
    sqlx::query("DELETE FROM issue_attributes WHERE id=$1")
        .bind(id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(())
}
pub async fn set_values(
    pool: &PgPool,
    repo: i64,
    actor: i64,
    issue: i64,
    values: &[ValueInput],
    replace: bool,
) -> Result<()> {
    let mut tx = begin_write(pool, repo, actor, Some(issue), false).await?;
    fields_in_transaction(&mut tx, repo, actor, issue, values, replace).await?;
    tx.commit().await?;
    Ok(())
}
pub(crate) async fn fields_in_transaction(
    tx: &mut Transaction<'_, Postgres>,
    repo: i64,
    actor: i64,
    issue: i64,
    values: &[ValueInput],
    replace: bool,
) -> Result<Option<i64>> {
    anyhow::ensure!(values.len() <= 25, Refusal::Limit);
    let fields = catalog_connection(tx, repo, Some(actor), "field").await?;
    let mut seen = std::collections::HashSet::new();
    let mut validated = Vec::new();
    for value in values {
        anyhow::ensure!(seen.insert(value.field_id), Refusal::Invalid);
        let field = fields
            .iter()
            .find(|f| f.id == value.field_id)
            .ok_or(Refusal::Invalid)?;
        validated.push((
            field.id,
            serde_json::to_string(&field.canonical(&value.value)?)?,
        ));
    }
    let existing:Vec<(i64,String)>=sqlx::query_as("SELECT v.attribute_id,v.value_json FROM issue_field_values v JOIN issue_attributes a ON a.id=v.attribute_id JOIN repositories r ON r.id=$2 WHERE v.issue_id=$1 AND a.kind='field' AND a.owner_org_id=r.owner_org_id ORDER BY v.attribute_id").bind(issue).bind(repo).fetch_all(&mut **tx).await?;
    let clear = replace || values.is_empty();
    if clear {
        anyhow::ensure!(
            existing
                .iter()
                .all(|(id, _)| fields.iter().any(|f| f.id == *id)),
            Refusal::Denied
        );
    }
    let mut desired = if clear { Vec::new() } else { existing.clone() };
    for (id, value) in &validated {
        desired.retain(|(other, _)| other != id);
        desired.push((*id, value.clone()));
    }
    desired.sort_by_key(|(id, _)| *id);
    anyhow::ensure!(desired.len() <= 25, Refusal::Limit);
    if desired == existing {
        return Ok(None);
    }
    for (id, _) in &existing {
        if !desired.iter().any(|(other, _)| other == id) {
            sqlx::query("DELETE FROM issue_field_values WHERE issue_id=$1 AND attribute_id=$2")
                .bind(issue)
                .bind(id)
                .execute(&mut **tx)
                .await?;
        }
    }
    for (id, value) in desired {
        sqlx::query("INSERT INTO issue_field_values(issue_id,attribute_id,value_json) VALUES ($1,$2,$3) ON CONFLICT(issue_id,attribute_id) DO UPDATE SET value_json=excluded.value_json").bind(issue).bind(id).bind(value).execute(&mut **tx).await?;
    }
    touch(tx, issue).await?;
    Ok(Some(
        event(tx, (repo, issue, actor), "issue_field_changed", "", "").await?,
    ))
}
pub async fn delete_value(
    pool: &PgPool,
    repo: i64,
    actor: i64,
    issue: i64,
    field: i64,
) -> Result<()> {
    let mut tx = begin_write(pool, repo, actor, Some(issue), false).await?;
    let fields = catalog_connection(&mut tx, repo, Some(actor), "field").await?;
    anyhow::ensure!(fields.iter().any(|f| f.id == field), Refusal::Missing);
    let result =
        sqlx::query("DELETE FROM issue_field_values WHERE issue_id=$1 AND attribute_id=$2")
            .bind(issue)
            .bind(field)
            .execute(&mut *tx)
            .await?;
    anyhow::ensure!(result.rows_affected() == 1, Refusal::Missing);
    touch(&mut tx, issue).await?;
    event(&mut tx, (repo, issue, actor), "issue_field_changed", "", "").await?;
    tx.commit().await?;
    Ok(())
}
pub async fn set_type(
    pool: &PgPool,
    repo: i64,
    actor: i64,
    issue: i64,
    kind: Option<i64>,
) -> Result<()> {
    let mut tx = begin_write(pool, repo, actor, Some(issue), false).await?;
    type_in_transaction(&mut tx, repo, actor, issue, kind).await?;
    tx.commit().await?;
    Ok(())
}
pub(crate) async fn type_in_transaction(
    tx: &mut Transaction<'_, Postgres>,
    repo: i64,
    actor: i64,
    issue: i64,
    kind: Option<i64>,
) -> Result<Option<i64>> {
    let old = assigned_connection(tx, repo, issue).await?;
    if old.as_ref().map(|a| a.id) == kind {
        return Ok(None);
    }
    let next = if let Some(id) = kind {
        Some(
            catalog_connection(tx, repo, Some(actor), "type")
                .await?
                .into_iter()
                .find(|a| a.id == id && a.is_enabled)
                .ok_or(Refusal::Invalid)?,
        )
    } else {
        None
    };
    sqlx::query("DELETE FROM issue_type_assignments WHERE issue_id=$1")
        .bind(issue)
        .execute(&mut **tx)
        .await?;
    if let Some(id) = kind {
        sqlx::query("INSERT INTO issue_type_assignments(issue_id,attribute_id) VALUES ($1,$2)")
            .bind(issue)
            .bind(id)
            .execute(&mut **tx)
            .await?;
    }
    let snapshot = |a: &Attribute| json!({"id":a.id,"name":a.name,"color":a.color}).to_string();
    let event_kind = if old.is_none() {
        "issue_type_added"
    } else if next.is_none() {
        "issue_type_removed"
    } else {
        "issue_type_changed"
    };
    let id = event(
        tx,
        (repo, issue, actor),
        event_kind,
        &next.as_ref().map(snapshot).unwrap_or_default(),
        &old.as_ref().map(snapshot).unwrap_or_default(),
    )
    .await?;
    touch(tx, issue).await?;
    Ok(Some(id))
}
pub(crate) async fn event(
    tx: &mut Transaction<'_, Postgres>,
    (repo, issue, actor): (i64, i64, i64),
    kind: &str,
    detail: &str,
    previous: &str,
) -> Result<i64> {
    Ok(sqlx::query_scalar("INSERT INTO timeline_events(repo_id,subject_kind,subject_id,actor_id,kind,detail,previous) VALUES ($1,'issue',$2,$3,$4,$5,$6) RETURNING id").bind(repo).bind(issue).bind(actor).bind(kind).bind(detail).bind(previous).fetch_one(&mut **tx).await?)
}
pub(crate) async fn touch(tx: &mut Transaction<'_, Postgres>, issue: i64) -> Result<()> {
    sqlx::query("UPDATE issues SET updated_at=GREATEST(clock_timestamp(),updated_at+INTERVAL '1 microsecond') WHERE id=$1").bind(issue).execute(&mut **tx).await?;
    Ok(())
}
