use super::authority::*;
use crate::backend::db::organizations::immutable_releases::Change;
use crate::backend::db::organizations::immutable_releases::Settings;
use crate::backend::db::organizations::OrganizationOutcome;
use anyhow::Result;
use codeyard_db_core::models::OrgRole;
use sqlx::SqlitePool;
use OrganizationOutcome::{Applied, Conflict, Denied};

pub async fn get(pool: &SqlitePool, actor: i64, org: i64) -> Result<OrganizationOutcome<Settings>> {
    let mut tx = begin_read(pool).await?;
    if require_read_org!(tx, actor, org) != OrgRole::Owner {
        return Ok(Denied);
    }
    let enforced_repositories = sqlx::query_scalar(
        "SELECT enforced_repositories FROM organization_immutable_releases WHERE org_id=$1",
    )
    .bind(org)
    .fetch_optional(&mut *tx)
    .await?
    .unwrap_or_else(|| "none".to_owned());
    tx.commit().await?;
    Ok(Applied(Settings {
        enforced_repositories,
    }))
}

pub async fn repositories(
    pool: &SqlitePool,
    actor: i64,
    org: i64,
    offset: i64,
    limit: i64,
) -> Result<
    OrganizationOutcome<(
        i64,
        Vec<crate::backend::db::repository_stats::RepositorySnapshot>,
    )>,
> {
    anyhow::ensure!(
        offset >= 0 && (1..=100).contains(&limit),
        "Invalid repository page"
    );
    let mut tx = begin_read(pool).await?;
    if require_read_org!(tx, actor, org) != OrgRole::Owner {
        return Ok(Denied);
    }
    let selection = "FROM organization_immutable_release_repositories s JOIN repositories r ON r.id=s.repo_id JOIN organizations o ON o.id=r.owner_org_id WHERE s.org_id=?1 AND r.owner_kind='org' AND r.owner_org_id=?1 AND r.owner_user_id IS NULL";
    let count = sqlx::query_scalar(&format!("SELECT COUNT(*) {selection}"))
        .bind(org)
        .fetch_one(&mut *tx)
        .await?;
    let rows = sqlx::query_as(&format!(
        "SELECT r.*,{},o.name AS owner {selection} ORDER BY r.id LIMIT ?2 OFFSET ?3",
        crate::backend::db::sqlite::repository_stats::DOCUMENT_FACTS
    ))
    .bind(org)
    .bind(limit)
    .bind(offset)
    .fetch_all(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(Applied((count, rows)))
}

pub async fn change(
    pool: &SqlitePool,
    actor: i64,
    org: i64,
    change: &Change,
) -> Result<OrganizationOutcome<()>> {
    let mut tx = begin(pool).await?;
    if require_org!(tx, actor, org) != OrgRole::Owner {
        return Ok(Denied);
    }
    // A selection only means something in selected mode, so it is changed only there (or
    // together with switching to it); like GitHub, other modes refuse selected_repository_ids.
    let ids = match change {
        Change::Enforcement(mode, ids) => {
            if !matches!(mode.as_str(), "all" | "selected" | "none")
                || (ids.is_some() && mode != "selected")
            {
                return Ok(Conflict);
            }
            ids.as_deref().unwrap_or(&[])
        }
        Change::Repositories(ids) => ids.as_slice(),
        Change::Repository(id, _) => std::slice::from_ref(id),
    };
    if !matches!(change, Change::Enforcement(..)) {
        let mode: Option<String> = sqlx::query_scalar(
            "SELECT enforced_repositories FROM organization_immutable_releases WHERE org_id=$1",
        )
        .bind(org)
        .fetch_optional(&mut *tx)
        .await?;
        if mode.as_deref() != Some("selected") {
            return Ok(Conflict);
        }
    }
    if !scoped_repos(&mut tx, org, ids).await? {
        return Ok(Conflict);
    }
    if let Change::Enforcement(mode, _) = change {
        sqlx::query("INSERT INTO organization_immutable_releases(org_id,enforced_repositories) VALUES($1,$2) ON CONFLICT(org_id) DO UPDATE SET enforced_repositories=excluded.enforced_repositories").bind(org).bind(mode).execute(&mut *tx).await?;
    }
    match change {
        Change::Repositories(_) | Change::Enforcement(_, Some(_)) => {
            sqlx::query("DELETE FROM organization_immutable_release_repositories WHERE org_id=$1")
                .bind(org)
                .execute(&mut *tx)
                .await?;
        }
        Change::Repository(id, false) => {
            sqlx::query("DELETE FROM organization_immutable_release_repositories WHERE org_id=$1 AND repo_id=$2").bind(org).bind(id).execute(&mut *tx).await?;
        }
        _ => {}
    }
    if !matches!(change, Change::Repository(_, false)) {
        sqlx::query("INSERT INTO organization_immutable_release_repositories(org_id,repo_id) SELECT $1,value FROM json_each($2) WHERE TRUE ON CONFLICT DO NOTHING")
            .bind(org).bind(serde_json::to_string(ids)?).execute(&mut *tx).await?;
    }
    tx.commit().await?;
    Ok(Applied(()))
}
