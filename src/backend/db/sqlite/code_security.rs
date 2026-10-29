use crate::backend::db::{
    code_security::*,
    models::{OrgRole, Repository},
    organizations::OrganizationOutcome,
};
use anyhow::{ensure, Result};
use sqlx::{Sqlite, SqliteConnection, SqlitePool, Transaction};

async fn write(pool: &SqlitePool, actor: i64, org: i64) -> Result<Transaction<'_, Sqlite>> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    match super::organizations::authority::authority(&mut tx, actor, org).await? {
        OrganizationOutcome::Applied(OrgRole::Owner | OrgRole::Admin) => {}
        OrganizationOutcome::Applied(OrgRole::Member) if super::organizations::roles::manages_security(&mut tx, actor, org).await? => {}
        OrganizationOutcome::Missing => return Err(Refusal::Missing.into()),
        _ => return Err(Refusal::Denied.into()),
    }
    Ok(tx)
}
async fn readable(connection: &mut SqliteConnection, actor: i64, org: i64) -> Result<()> {
    match super::organizations::authority::read_authority(connection, actor, org).await? {
        OrganizationOutcome::Applied(OrgRole::Owner | OrgRole::Admin) => Ok(()),
        OrganizationOutcome::Applied(OrgRole::Member) if super::organizations::roles::holds_security_role(connection, actor, org).await? => Ok(()),
        OrganizationOutcome::Missing => Err(Refusal::Missing.into()),
        _ => Err(Refusal::Denied.into()),
    }
}
async fn configuration(
    connection: &mut SqliteConnection,
    org: i64,
    id: i64,
) -> Result<Configuration> {
    Ok(
        sqlx::query_as("SELECT * FROM code_security_configurations WHERE org_id=$1 AND id=$2")
            .bind(org)
            .bind(id)
            .fetch_optional(connection)
            .await?
            .ok_or(Refusal::Missing)?,
    )
}
async fn apply_repository(
    connection: &mut SqliteConnection,
    repo: i64,
    actor: Option<i64>,
    config: Option<&Configuration>,
) -> Result<()> {
    let current: Option<RepositorySettings> =
        sqlx::query_as("SELECT * FROM repository_security_settings WHERE repo_id=$1")
            .bind(repo)
            .fetch_optional(&mut *connection)
            .await?;
    let settings = match config {
        Some(config) => apply(
            current
                .as_ref()
                .map(|row| row.security_and_analysis.as_str()),
            &config.settings,
        )?,
        None => current.map_or_else(|| "{}".into(), |row| row.security_and_analysis),
    };
    if let Some(config) = config {
        let previous: Option<crate::backend::db::secret_scanning::Settings> = sqlx::query_as(
            "SELECT enabled,push_protection FROM secret_scanning_settings WHERE repo_id=$1",
        )
        .bind(repo)
        .fetch_optional(&mut *connection)
        .await?;
        super::secret_scanning::save_settings_on_connection(
            connection,
            actor,
            repo,
            &scanning_settings(&settings, previous.unwrap_or_default())?,
        )
        .await?;
        // Private vulnerability reporting lives in its own table and is offered on public
        // repositories only.
        match private_reporting(&config.settings)? {
            Some(true) => {
                sqlx::query("INSERT INTO repository_private_reporting(repo_id,enabled_by_id) SELECT id,$2 FROM repositories WHERE id=$1 AND NOT is_private ON CONFLICT(repo_id) DO NOTHING")
                    .bind(repo).bind(actor).execute(&mut *connection).await?;
            }
            Some(false) => {
                sqlx::query("DELETE FROM repository_private_reporting WHERE repo_id=$1")
                    .bind(repo)
                    .execute(&mut *connection)
                    .await?;
            }
            None => {}
        }
    }
    sqlx::query("INSERT INTO repository_security_settings(repo_id,configuration_id,security_and_analysis) VALUES($1,$2,$3) ON CONFLICT(repo_id) DO UPDATE SET configuration_id=excluded.configuration_id,security_and_analysis=excluded.security_and_analysis")
        .bind(repo).bind(config.map(|c|c.id)).bind(settings).execute(connection).await?;
    Ok(())
}
pub async fn list(pool: &SqlitePool, actor: i64, org: i64) -> Result<Vec<Configuration>> {
    let mut tx = pool.begin().await?;
    readable(&mut tx, actor, org).await?;
    let rows =
        sqlx::query_as("SELECT * FROM code_security_configurations WHERE org_id=$1 ORDER BY id")
            .bind(org)
            .fetch_all(&mut *tx)
            .await?;
    tx.commit().await?;
    Ok(rows)
}
pub async fn save(
    pool: &SqlitePool,
    actor: i64,
    org: i64,
    id: Option<i64>,
    patch: &serde_json::Value,
) -> Result<Configuration> {
    let mut tx = write(pool, actor, org).await?;
    let current = match id {
        Some(id) => Some(configuration(&mut tx, org, id).await?),
        None => None,
    };
    let settings = merge(current.as_ref().map(|c| c.settings.as_str()), patch)?;
    let name = settings["name"].as_str().ok_or(Refusal::Invalid)?;
    let duplicate:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM code_security_configurations WHERE org_id=$1 AND name=$2 AND id<>$3)").bind(org).bind(name).bind(id.unwrap_or(-1)).fetch_one(&mut *tx).await?;
    ensure!(!duplicate, Refusal::Duplicate);
    let now = chrono::Utc::now();
    let row: Configuration = if let Some(id) = id {
        sqlx::query_as("UPDATE code_security_configurations SET name=$3,settings=$4,updated_at=$5 WHERE org_id=$1 AND id=$2 RETURNING *").bind(org).bind(id).bind(name).bind(settings.to_string()).bind(now).fetch_one(&mut *tx).await?
    } else {
        sqlx::query_as("INSERT INTO code_security_configurations(org_id,name,settings,created_at,updated_at) VALUES($1,$2,$3,$4,$4) RETURNING *").bind(org).bind(name).bind(settings.to_string()).bind(now).fetch_one(&mut *tx).await?
    };
    let repos:Vec<i64>=sqlx::query_scalar("SELECT r.id FROM repositories r JOIN repository_security_settings s ON s.repo_id=r.id WHERE r.owner_org_id=$1 AND s.configuration_id=$2 ORDER BY r.id").bind(org).bind(row.id).fetch_all(&mut *tx).await?;
    for repo in repos {
        apply_repository(&mut tx, repo, Some(actor), Some(&row)).await?;
    }
    tx.commit().await?;
    Ok(row)
}
pub async fn delete(pool: &SqlitePool, actor: i64, org: i64, id: i64) -> Result<()> {
    let mut tx = write(pool, actor, org).await?;
    configuration(&mut tx, org, id).await?;
    sqlx::query("DELETE FROM code_security_configurations WHERE org_id=$1 AND id=$2")
        .bind(org)
        .bind(id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(())
}
pub async fn set_default(
    pool: &SqlitePool,
    actor: i64,
    org: i64,
    id: i64,
    scope: &str,
) -> Result<Configuration> {
    ensure!(
        ["all", "none", "public", "private_and_internal"].contains(&scope),
        Refusal::Invalid
    );
    let mut tx = write(pool, actor, org).await?;
    configuration(&mut tx, org, id).await?;
    let rows: Vec<Configuration> =
        sqlx::query_as("SELECT * FROM code_security_configurations WHERE org_id=$1")
            .bind(org)
            .fetch_all(&mut *tx)
            .await?;
    for row in rows {
        let next = if row.id == id {
            scope
        } else if scope == "all" || row.default_for_new_repos == scope {
            "none"
        } else if row.default_for_new_repos == "all" {
            match scope {
                "public" => "private_and_internal",
                "private_and_internal" => "public",
                _ => "all",
            }
        } else {
            &row.default_for_new_repos
        };
        sqlx::query("UPDATE code_security_configurations SET default_for_new_repos=$2 WHERE id=$1")
            .bind(row.id)
            .bind(next)
            .execute(&mut *tx)
            .await?;
    }
    let row = configuration(&mut tx, org, id).await?;
    tx.commit().await?;
    Ok(row)
}
pub async fn attach(
    pool: &SqlitePool,
    actor: i64,
    org: i64,
    id: Option<i64>,
    scope: &str,
    selected: &[i64],
) -> Result<()> {
    ensure!(
        [
            "all",
            "all_without_configurations",
            "public",
            "private_or_internal",
            "selected"
        ]
        .contains(&scope),
        Refusal::Invalid
    );
    ensure!(
        if scope == "selected" {
            !selected.is_empty() && selected.len() <= 250 && selected.iter().all(|id| *id > 0)
        } else {
            selected.is_empty()
        },
        Refusal::Invalid
    );
    let mut tx = write(pool, actor, org).await?;
    let config = match id {
        Some(id) => Some(configuration(&mut tx, org, id).await?),
        None => None,
    };
    let rows: Vec<Repository> =
        sqlx::query_as("SELECT * FROM repositories WHERE owner_org_id=$1 ORDER BY id")
            .bind(org)
            .fetch_all(&mut *tx)
            .await?;
    ensure!(
        selected.iter().all(|id| rows.iter().any(|r| r.id == *id)),
        Refusal::Missing
    );
    for repo in rows {
        let chosen=match scope {"selected"=>selected.contains(&repo.id),"public"=>!repo.is_private,"private_or_internal"=>repo.is_private,"all_without_configurations"=>!sqlx::query_scalar::<_,bool>("SELECT EXISTS(SELECT 1 FROM repository_security_settings WHERE repo_id=$1 AND configuration_id IS NOT NULL)").bind(repo.id).fetch_one(&mut *tx).await?,_=>true};
        if chosen {
            apply_repository(&mut tx, repo.id, Some(actor), config.as_ref()).await?;
        }
    }
    tx.commit().await?;
    Ok(())
}
pub async fn repositories(
    pool: &SqlitePool,
    actor: i64,
    org: i64,
    id: i64,
) -> Result<Vec<Repository>> {
    let mut tx = pool.begin().await?;
    readable(&mut tx, actor, org).await?;
    configuration(&mut tx, org, id).await?;
    let rows=sqlx::query_as("SELECT r.* FROM repositories r JOIN repository_security_settings s ON s.repo_id=r.id WHERE r.owner_org_id=$1 AND s.configuration_id=$2 ORDER BY r.id").bind(org).bind(id).fetch_all(&mut *tx).await?;
    tx.commit().await?;
    Ok(rows)
}
pub async fn repository_settings(
    pool: &SqlitePool,
    repo: i64,
) -> Result<Option<RepositorySettings>> {
    Ok(
        sqlx::query_as("SELECT * FROM repository_security_settings WHERE repo_id=$1")
            .bind(repo)
            .fetch_optional(pool)
            .await?,
    )
}
pub async fn repository_configuration(
    pool: &SqlitePool,
    repo: i64,
) -> Result<Option<Configuration>> {
    Ok(sqlx::query_as("SELECT c.* FROM code_security_configurations c JOIN repository_security_settings s ON s.configuration_id=c.id JOIN repositories r ON r.id=s.repo_id AND r.owner_org_id=c.org_id WHERE r.id=$1").bind(repo).fetch_optional(pool).await?)
}
pub(crate) async fn apply_default(
    connection: &mut SqliteConnection,
    repo: &Repository,
) -> Result<()> {
    let Some(org) = repo.owner_org_id else {
        return Ok(());
    };
    let scope = if repo.is_private {
        "private_and_internal"
    } else {
        "public"
    };
    let row:Option<Configuration>=sqlx::query_as("SELECT * FROM code_security_configurations WHERE org_id=$1 AND default_for_new_repos IN ('all',$2) ORDER BY id LIMIT 1").bind(org).bind(scope).fetch_optional(&mut *connection).await?;
    if let Some(config) = row {
        apply_repository(connection, repo.id, None, Some(&config)).await?;
    }
    Ok(())
}
