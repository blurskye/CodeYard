use super::authority::*;
mod access;
mod suites;
use crate::backend::db::{
    models::OrgRole,
    organizations::OrganizationOutcome,
    rulesets::{
        ActorType, Definition, Invalid, ListFilter, Owner, Page, PropertySource, Ruleset,
        StoredRuleset, StoredVersion, Version, View, MAX_RULESETS,
    },
};
use anyhow::Result;
use serde_json::Value;
use sqlx::{PgConnection, PgPool};
pub use suites::{
    get_repository_suite, get_suite, list_repository_suites, list_suites, record_suite,
};
use OrganizationOutcome::{Applied, Denied, Missing};

fn invalid(field: &'static str, message: &str) -> Invalid {
    Invalid {
        field,
        message: message.into(),
    }
}
async fn links(c: &mut PgConnection, owner: Owner, input: &Definition) -> Result<()> {
    let org: Option<i64> = match owner {
        Owner::Organization(org) => Some(org),
        Owner::Repository(repo) => {
            sqlx::query_scalar("SELECT owner_org_id FROM repositories WHERE id=$1")
                .bind(repo)
                .fetch_one(&mut *c)
                .await?
        }
    };
    if org.is_none()
        && input
            .bypass_actors
            .iter()
            .any(|a| a.actor_type == ActorType::OrganizationAdmin)
    {
        return Err(invalid(
            "bypass_actors",
            "Organization administrators require an organization repository",
        )
        .into());
    }
    let teams: Vec<i64> = input
        .bypass_actors
        .iter()
        .filter(|a| a.actor_type == ActorType::Team)
        .filter_map(|a| a.actor_id)
        .collect();
    let users: Vec<i64> = input
        .bypass_actors
        .iter()
        .filter(|a| a.actor_type == ActorType::User)
        .filter_map(|a| a.actor_id)
        .collect();
    if !teams.is_empty() {
        let count:i64=sqlx::query_scalar("SELECT COUNT(*) FROM teams WHERE org_id=$1 AND id=ANY($2) AND privacy IN ('visible','secret')").bind(org).bind(&teams).fetch_one(&mut *c).await?;
        if count != teams.len() as i64 {
            return Err(invalid(
                "bypass_actors",
                "A team does not belong to this organization or is unavailable",
            )
            .into());
        }
    }
    if !users.is_empty() {
        let count:i64=sqlx::query_scalar("SELECT COUNT(*) FROM users u WHERE u.id=ANY($2) AND NOT u.is_banned AND NOT u.must_change_password AND NOT EXISTS(SELECT 1 FROM organization_blocks b WHERE b.org_id=$1 AND b.user_id=u.id)").bind(org).bind(&users).fetch_one(&mut *c).await?;
        if count != users.len() as i64 {
            return Err(invalid("bypass_actors", "A user is unavailable for this ruleset").into());
        }
    }
    if let Some(conditions) = &input.conditions {
        if let Some(ids) = &conditions.repository_id {
            let count:i64=sqlx::query_scalar("SELECT COUNT(*) FROM repositories WHERE owner_kind='org' AND owner_org_id=$1 AND owner_user_id IS NULL AND id=ANY($2)").bind(org).bind(&ids.repository_ids).fetch_one(&mut *c).await?;
            if count != ids.repository_ids.len() as i64 {
                return Err(invalid(
                    "conditions.repository_id",
                    "A repository does not belong to this organization",
                )
                .into());
            }
        }
        if let Some(properties) = &conditions.repository_property {
            let names: Vec<String> = properties
                .include
                .iter()
                .chain(&properties.exclude)
                .filter(|p| p.source == PropertySource::Custom)
                .map(|p| p.name.to_ascii_lowercase())
                .collect();
            if !names.is_empty() {
                let found:Vec<String>=sqlx::query_scalar("SELECT property_name FROM organization_repository_properties WHERE org_id=$1 AND lower(property_name)=ANY($2)").bind(org).bind(&names).fetch_all(&mut *c).await?;
                if names
                    .iter()
                    .any(|name| !found.iter().any(|found| found.eq_ignore_ascii_case(name)))
                {
                    return Err(invalid(
                        "conditions.repository_property",
                        "A custom property does not belong to this organization",
                    )
                    .into());
                }
            }
        }
    }
    Ok(())
}
fn view(mut rule: Ruleset, editable: bool) -> View<Ruleset> {
    if !editable {
        rule.definition.bypass_actors.clear();
    }
    View {
        value: rule,
        can_edit: editable,
    }
}
async fn exists(c: &mut PgConnection, owner: Owner, id: i64) -> Result<bool> {
    let (org, repo) = owner.ids();
    Ok(sqlx::query_scalar::<_, i64>(
        "SELECT id FROM repository_rulesets WHERE id=$1 AND (owner_org_id=$2 OR owner_repo_id=$3)",
    )
    .bind(id)
    .bind(org)
    .bind(repo)
    .fetch_optional(c)
    .await?
    .is_some())
}
async fn save_version(c: &mut PgConnection, actor: i64, id: i64, definition: &str) -> Result<()> {
    sqlx::query("INSERT INTO repository_ruleset_versions(ruleset_id,definition,actor_id,actor_type) VALUES($1,$2,$3,'User')").bind(id).bind(definition).bind(actor).execute(c).await?;
    Ok(())
}
pub async fn list(
    pool: &PgPool,
    actor: Option<i64>,
    owner: Owner,
    filter: &ListFilter,
    page: Page,
) -> Result<OrganizationOutcome<Vec<View<Ruleset>>>> {
    let mut tx = begin_read(pool).await?;
    let editable = match access::authorize(&mut tx, actor, owner, false).await? {
        Applied(editable) => editable,
        Missing => return Ok(Missing),
        _ => return Ok(Denied),
    };
    let rows = access::rows(&mut tx, actor, owner, editable, filter).await?;
    tx.commit().await?;
    Ok(Applied(
        rows.into_iter()
            .skip(page.offset as usize)
            .take(page.limit as usize)
            .collect(),
    ))
}
pub async fn get(
    pool: &PgPool,
    actor: Option<i64>,
    owner: Owner,
    id: i64,
    includes_parents: bool,
) -> Result<OrganizationOutcome<View<Ruleset>>> {
    let mut tx = begin_read(pool).await?;
    let editable = match access::authorize(&mut tx, actor, owner, false).await? {
        Applied(editable) => editable,
        Missing => return Ok(Missing),
        _ => return Ok(Denied),
    };
    let rows = access::rows(
        &mut tx,
        actor,
        owner,
        editable,
        &ListFilter {
            includes_parents,
            ..ListFilter::default()
        },
    )
    .await?;
    let Some(row) = rows.into_iter().find(|row| row.value.id == id) else {
        return Ok(Missing);
    };
    tx.commit().await?;
    Ok(Applied(row))
}
pub async fn create(
    pool: &PgPool,
    actor: i64,
    owner: Owner,
    input: &Definition,
) -> Result<OrganizationOutcome<Ruleset>> {
    let mut tx = begin(pool).await?;
    match access::authorize(&mut tx, Some(actor), owner, true).await? {
        Applied(true) => {}
        Missing => return Ok(Missing),
        OrganizationOutcome::Suspended => return Ok(OrganizationOutcome::Suspended),
        _ => return Ok(Denied),
    }
    let (org, repo) = owner.ids();
    let document = input.storage_document(owner)?;
    links(&mut tx, owner, input).await?;
    let count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM repository_rulesets WHERE owner_org_id=$1 OR owner_repo_id=$2",
    )
    .bind(org)
    .bind(repo)
    .fetch_one(&mut *tx)
    .await?;
    if count >= MAX_RULESETS as i64 {
        return Err(invalid("ruleset", "This owner already has 75 rulesets").into());
    }
    let row: StoredRuleset = sqlx::query_as("INSERT INTO repository_rulesets(owner_org_id,owner_repo_id,definition) VALUES($1,$2,$3) RETURNING *").bind(org).bind(repo).bind(&document).fetch_one(&mut *tx).await?;
    save_version(&mut tx, actor, row.id, &document).await?;
    let row = row.decode()?;
    tx.commit().await?;
    Ok(Applied(row))
}
pub async fn change(
    pool: &PgPool,
    actor: i64,
    owner: Owner,
    id: i64,
    input: &Value,
) -> Result<OrganizationOutcome<Ruleset>> {
    let mut tx = begin(pool).await?;
    match access::authorize(&mut tx, Some(actor), owner, true).await? {
        Applied(true) => {}
        Missing => return Ok(Missing),
        OrganizationOutcome::Suspended => return Ok(OrganizationOutcome::Suspended),
        _ => return Ok(Denied),
    }
    let (org, repo) = owner.ids();
    let row: Option<StoredRuleset> = sqlx::query_as("SELECT * FROM repository_rulesets WHERE id=$1 AND (owner_org_id=$2 OR owner_repo_id=$3) FOR UPDATE").bind(id).bind(org).bind(repo).fetch_optional(&mut *tx).await?;
    let Some(row) = row else { return Ok(Missing) };
    let previous = row.definition.clone();
    let current = row.decode()?;
    let changed = current.definition.merge(input)?;
    let document = changed.storage_document(owner)?;
    links(&mut tx, owner, &changed).await?;
    if previous == document {
        return Ok(Applied(current));
    }
    let row: StoredRuleset = sqlx::query_as("UPDATE repository_rulesets SET definition=$2,updated_at=GREATEST(clock_timestamp(),updated_at+INTERVAL '1 microsecond') WHERE id=$1 RETURNING *").bind(id).bind(&document).fetch_one(&mut *tx).await?;
    save_version(&mut tx, actor, id, &document).await?;
    let row = row.decode()?;
    tx.commit().await?;
    Ok(Applied(row))
}
pub async fn remove(
    pool: &PgPool,
    actor: i64,
    owner: Owner,
    id: i64,
) -> Result<OrganizationOutcome<()>> {
    let mut tx = begin(pool).await?;
    match access::authorize(&mut tx, Some(actor), owner, true).await? {
        Applied(true) => {}
        Missing => return Ok(Missing),
        OrganizationOutcome::Suspended => return Ok(OrganizationOutcome::Suspended),
        _ => return Ok(Denied),
    }
    let (org, repo) = owner.ids();
    let deleted = sqlx::query(
        "DELETE FROM repository_rulesets WHERE id=$1 AND (owner_org_id=$2 OR owner_repo_id=$3)",
    )
    .bind(id)
    .bind(org)
    .bind(repo)
    .execute(&mut *tx)
    .await?
    .rows_affected();
    if deleted == 0 {
        return Ok(Missing);
    }
    tx.commit().await?;
    Ok(Applied(()))
}
pub async fn versions(
    pool: &PgPool,
    actor: Option<i64>,
    owner: Owner,
    id: i64,
    page: Page,
) -> Result<OrganizationOutcome<View<Vec<Version>>>> {
    let mut tx = begin_read(pool).await?;
    let editable = match access::authorize(&mut tx, actor, owner, false).await? {
        Applied(editable) => editable,
        Missing => return Ok(Missing),
        _ => return Ok(Denied),
    };
    if !exists(&mut tx, owner, id).await? {
        return Ok(Missing);
    }
    let rows:Vec<StoredVersion>=sqlx::query_as("SELECT * FROM repository_ruleset_versions WHERE ruleset_id=$1 ORDER BY id DESC LIMIT $2 OFFSET $3").bind(id).bind(page.limit).bind(page.offset).fetch_all(&mut *tx).await?;
    let mut versions = rows
        .into_iter()
        .map(StoredVersion::decode)
        .collect::<Result<Vec<_>>>()?;
    if !editable {
        for version in &mut versions {
            version.definition.bypass_actors.clear();
        }
    }
    tx.commit().await?;
    Ok(Applied(View {
        value: versions,
        can_edit: editable,
    }))
}
pub async fn version(
    pool: &PgPool,
    actor: Option<i64>,
    owner: Owner,
    id: i64,
    version: i64,
) -> Result<OrganizationOutcome<View<Version>>> {
    let mut tx = begin_read(pool).await?;
    let editable = match access::authorize(&mut tx, actor, owner, false).await? {
        Applied(editable) => editable,
        Missing => return Ok(Missing),
        _ => return Ok(Denied),
    };
    if !exists(&mut tx, owner, id).await? {
        return Ok(Missing);
    }
    let row: Option<StoredVersion> =
        sqlx::query_as("SELECT * FROM repository_ruleset_versions WHERE id=$1 AND ruleset_id=$2")
            .bind(version)
            .bind(id)
            .fetch_optional(&mut *tx)
            .await?;
    let Some(row) = row else { return Ok(Missing) };
    let mut row = row.decode()?;
    if !editable {
        row.definition.bypass_actors.clear();
    }
    tx.commit().await?;
    Ok(Applied(View {
        value: row,
        can_edit: editable,
    }))
}
/// Effective membership, not a separate repository grant, determines a team bypass.
pub async fn bypass_teams(pool: &PgPool, repo: i64, actor: i64, teams: &[i64]) -> Result<Vec<i64>> {
    anyhow::ensure!(teams.len() <= 500, "Too many bypass teams");
    Ok(sqlx::query_scalar("SELECT t.id FROM teams t JOIN repositories r ON r.owner_kind='org' AND r.owner_user_id IS NULL AND r.owner_org_id=t.org_id JOIN effective_team_memberships tm ON tm.team_id=t.id JOIN org_memberships m ON m.org_id=t.org_id AND m.user_id=tm.user_id JOIN users u ON u.id=tm.user_id WHERE r.id=$1 AND tm.user_id=$2 AND t.id=ANY($3) AND t.privacy IN ('visible','secret') AND tm.role IN ('member','maintainer') AND m.role IN ('owner','admin','member') AND NOT u.is_banned AND NOT u.must_change_password")
        .bind(repo).bind(actor).bind(teams).fetch_all(pool).await?)
}

/// The rulesets a push to `repo` is judged by: its own and the matching organization ones.
pub async fn push_rulesets(pool: &PgPool, repo: i64) -> Result<Vec<Ruleset>> {
    let mut tx = begin_read(pool).await?;
    let rows = access::rows(
        &mut tx,
        None,
        Owner::Repository(repo),
        true,
        &ListFilter {
            includes_parents: true,
            ..ListFilter::default()
        },
    )
    .await?;
    tx.commit().await?;
    Ok(rows.into_iter().map(|row| row.value).collect())
}
