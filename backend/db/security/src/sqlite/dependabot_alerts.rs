use crate::dependabot_alerts::Update;
use anyhow::ensure;
use anyhow::Result;
use chrono::Utc;
use codeyard_db_core::queries::credentials::Actor;
use codeyard_db_core::queries::dependabot_alerts::Alert;
use codeyard_model::advisories::Advisory;
use codeyard_model::advisories::Refusal;
use serde_json::Value;
use sqlx::Sqlite;
use sqlx::SqlitePool;
use sqlx::Transaction;

pub async fn global_page(
    pool: &SqlitePool,
    query: &crate::dependabot_alerts::CatalogueQuery,
) -> Result<crate::dependabot_alerts::CataloguePage> {
    use futures_util::TryStreamExt;
    ensure!(
        (1..=100).contains(&query.limit) && query.offset <= 1_000_000,
        Refusal::Invalid
    );
    let mut tx = pool.begin().await?;
    let (statement, args, offset_applied) =
        crate::dependabot_alerts::catalogue_statement(false, query);
    let mut selected = crate::dependabot_alerts::CatalogueSelection::new(query, offset_applied);
    {
        let mut candidates = sqlx::query_as::<_, (String, String)>(&statement);
        for value in &args {
            candidates = candidates.bind(value);
        }
        let mut candidates = candidates.fetch(&mut *tx);
        while let Some((id, metadata)) = candidates.try_next().await? {
            if !selected.push(id, serde_json::from_str(&metadata)?) {
                break;
            }
        }
    }
    let (ids, previous, next) = selected.finish();
    let mut documents = Vec::with_capacity(ids.len());
    for id in ids {
        match global_one_in(&mut tx, &id).await {
            Ok(Some(value)) => documents.push(value),
            Ok(None) => (),
            Err(error) if error.downcast_ref::<Refusal>().is_some() => {
                tracing::warn!(%id, "Invalid published advisory excluded from catalogue")
            }
            Err(error) => return Err(error),
        }
    }
    tx.commit().await?;
    Ok(crate::dependabot_alerts::CataloguePage {
        documents,
        previous,
        next,
    })
}

/// The whole advisory catalogue.
pub async fn global(pool: &SqlitePool) -> Result<Vec<Value>> {
    let mut tx = pool.begin().await?;
    let rows = global_in(&mut tx, None).await?;
    tx.commit().await?;
    Ok(rows)
}
/// The catalogue advisories that have no OSV record, which `vulnerabilities::findings` cannot
/// see: imported ones without one, and published repository advisories that no import replaces.
pub async fn without_osv(pool: &SqlitePool) -> Result<Vec<Value>> {
    let mut tx = pool.begin().await?;
    let imported: Vec<String> = sqlx::query_scalar(
        "SELECT document FROM global_advisories WHERE osv_id IS NULL ORDER BY ghsa_id",
    )
    .fetch_all(&mut *tx)
    .await?;
    let published: Vec<Advisory> = sqlx::query_as("SELECT a.* FROM repository_advisories a JOIN repositories r ON r.id=a.repo_id WHERE a.state='published' AND NOT r.is_private AND NOT EXISTS (SELECT 1 FROM global_advisories g WHERE g.ghsa_id=a.ghsa_id) ORDER BY a.ghsa_id").fetch_all(&mut *tx).await?;
    tx.commit().await?;
    let mut rows = Vec::with_capacity(imported.len() + published.len());
    for row in published {
        match codeyard_db_core::queries::dependabot_alerts::published(row) {
            Ok(value) => rows.push(value),
            Err(_) => tracing::warn!("Invalid published advisory excluded from catalogue"),
        }
    }
    for body in imported {
        rows.push(serde_json::from_str(&body)?);
    }
    Ok(rows)
}
/// One catalogue advisory by GHSA ID through the key index. An imported document wins over a published one, as in `global_in`.
pub async fn global_one(pool: &SqlitePool, ghsa_id: &str) -> Result<Option<Value>> {
    let mut connection = pool.acquire().await?;
    global_one_in(&mut connection, ghsa_id).await
}
async fn global_one_in(
    connection: &mut sqlx::SqliteConnection,
    ghsa_id: &str,
) -> Result<Option<Value>> {
    let imported: Option<String> =
        sqlx::query_scalar("SELECT document FROM global_advisories WHERE ghsa_id=?1")
            .bind(ghsa_id)
            .fetch_optional(&mut *connection)
            .await?;
    if let Some(body) = imported {
        return Ok(Some(serde_json::from_str(&body)?));
    }
    let published: Option<Advisory> = sqlx::query_as("SELECT a.* FROM repository_advisories a JOIN repositories r ON r.id=a.repo_id WHERE a.state='published' AND NOT r.is_private AND a.ghsa_id=?1")
        .bind(ghsa_id)
        .fetch_optional(&mut *connection)
        .await?;
    match published {
        Some(row) => Ok(Some(
            codeyard_db_core::queries::dependabot_alerts::published(row)?,
        )),
        None => Ok(None),
    }
}
pub async fn import(pool: &SqlitePool, actor: impl Into<Actor>, value: &Value) -> Result<Value> {
    let authority = actor.into();
    let actor = authority.user_id;
    let value = codeyard_model::advisory_documents::import_document(value)?;
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    ensure!(
        codeyard_db_identity::sqlite::users::transitions::active_admin(&mut tx, actor).await?,
        Refusal::Denied
    );
    if let Some(credential) = authority.credential() {
        codeyard_db_identity::sqlite::users::transitions::require_account(
            &mut tx,
            actor,
            &credential,
        )
        .await?;
    }
    sqlx::query("INSERT INTO global_advisories(ghsa_id,document) VALUES($1,$2) ON CONFLICT(ghsa_id) DO UPDATE SET document=$2,osv_id=NULL")
        .bind(value["ghsa_id"].as_str().unwrap()).bind(value.to_string()).execute(&mut *tx).await?;
    tx.commit().await?;
    Ok(value)
}

async fn authorize(
    tx: &mut Transaction<'_, Sqlite>,
    actor: &Actor,
    repo: i64,
    write: bool,
) -> Result<()> {
    ensure!(
        codeyard_db_core::queries::sqlite::discussions::lock_security_authority(
            tx,
            actor.user_id,
            repo,
            write
        )
        .await?
            && codeyard_db_core::queries::sqlite::credentials::repository(
                tx,
                actor,
                repo,
                "vulnerability_alerts",
                write
            )
            .await?,
        Refusal::Denied
    );
    let settings: Option<String> = sqlx::query_scalar(
        "SELECT security_and_analysis FROM repository_security_settings WHERE repo_id=$1",
    )
    .bind(repo)
    .fetch_optional(&mut **tx)
    .await?;
    let settings = settings
        .map(|s| serde_json::from_str::<Value>(&s))
        .transpose()?;
    ensure!(
        !settings.is_some_and(|s| s["dependabot_alerts"]["status"] == "disabled"),
        Refusal::Missing
    );
    Ok(())
}
pub async fn alerts(pool: &SqlitePool, actor: impl Into<Actor>, repo: i64) -> Result<Vec<Alert>> {
    let actor = actor.into();
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    authorize(&mut tx, &actor, repo, false).await?;
    refresh(&mut tx, repo, None).await?;
    let rows =
        sqlx::query_as("SELECT * FROM dependabot_alerts WHERE repo_id=$1 ORDER BY number DESC")
            .bind(repo)
            .fetch_all(&mut *tx)
            .await?;
    tx.commit().await?;
    Ok(rows)
}
pub async fn update(
    pool: &SqlitePool,
    actor: impl Into<Actor>,
    repo: i64,
    number: i64,
    input: &Update,
) -> Result<Alert> {
    input.validate()?;
    let actor = actor.into();
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    authorize(&mut tx, &actor, repo, true).await?;
    refresh(&mut tx, repo, None).await?;
    let previous: Alert =
        sqlx::query_as("SELECT * FROM dependabot_alerts WHERE repo_id=$1 AND number=$2")
            .bind(repo)
            .bind(number)
            .fetch_optional(&mut *tx)
            .await?
            .ok_or(Refusal::Missing)?;
    ensure!(previous.state != "fixed", Refusal::Conflict);
    let now = Utc::now();
    let dismissed = input.state == "dismissed";
    let row = sqlx::query_as("UPDATE dependabot_alerts SET state=$3,dismissed_reason=$4,dismissed_comment=$5,dismissed_at=$6,dismissed_by=$7,updated_at=$8 WHERE repo_id=$1 AND number=$2 RETURNING *")
        .bind(repo).bind(number).bind(&input.state).bind(&input.dismissed_reason).bind(&input.dismissed_comment).bind(dismissed.then_some(now)).bind(dismissed.then_some(actor.user_id)).bind(now).fetch_one(&mut *tx).await?;
    tx.commit().await?;
    Ok(row)
}
pub async fn organization_repositories(
    pool: &SqlitePool,
    actor: i64,
    org: i64,
) -> Result<Vec<i64>> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let allowed: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM org_memberships m JOIN users u ON u.id=m.user_id WHERE m.org_id=$1 AND m.user_id=$2 AND m.role IN ('owner','admin') AND NOT u.is_banned AND NOT u.must_change_password AND EXISTS(SELECT 1 FROM org_memberships owners JOIN users active ON active.id=owners.user_id WHERE owners.org_id=m.org_id AND owners.role='owner' AND NOT active.is_banned AND NOT active.must_change_password))").bind(org).bind(actor).fetch_one(&mut *tx).await?;
    ensure!(
        allowed
            || codeyard_db_core::queries::sqlite::organizations::roles::manages_security(
                &mut tx, actor, org
            )
            .await?,
        Refusal::Denied
    );
    let rows = sqlx::query_scalar(
        "SELECT r.id FROM repositories r LEFT JOIN repository_security_settings s ON s.repo_id=r.id WHERE r.owner_kind='org' AND r.owner_org_id=$1 AND COALESCE(json_extract(s.security_and_analysis, '$.dependabot_alerts.status'), '') <> 'disabled' ORDER BY r.id",
    )
    .bind(org)
    .fetch_all(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(rows)
}

use codeyard_db_core::queries::sqlite::dependabot_alerts::refresh;

use codeyard_db_core::queries::sqlite::dependabot_alerts::global_in;

use codeyard_db_core::queries::sqlite::dependabot_alerts::global_in;

use codeyard_db_core::queries::sqlite::dependabot_alerts::refresh;
