use crate::backend::db::advisories::{identifier, Advisory, Data, Listing, Refusal, Scope};
use anyhow::Result;
use chrono::Utc;
use serde_json::Value;
use sqlx::SqlitePool;
fn visibility() -> String {
    let level = super::repositories::access::USER_REPO_ACCESS_SQL;
    format!("(NOT r.is_private OR (NOT u.is_banned AND NOT u.must_change_password AND ((r.owner_kind='user' AND (r.owner_user_id=u.id OR u.is_admin)) OR ({level})>=1))) AND (a.state='published' OR (NOT u.is_banned AND NOT u.must_change_password AND (a.author_id=u.id OR (r.owner_kind='user' AND u.is_admin) OR ({level})>=5)))")
}
pub async fn list(
    pool: &SqlitePool,
    actor: Option<i64>,
    scope: Scope,
    options: &Listing<'_>,
) -> Result<Vec<Advisory>> {
    let (repo, org) = match scope {
        Scope::Repository(id) => (Some(id), None),
        Scope::Organization(id) => (None, Some(id)),
    };
    let column = format!("julianday({})", options.sort.column());
    let ascending = options.ascending ^ options.before;
    let order = if ascending { "ASC" } else { "DESC" };
    let compare = if ascending { ">" } else { "<" };
    let (time, id) = options
        .cursor
        .map(|(at, id)| (Some(at), Some(id)))
        .unwrap_or_default();
    let sql=format!("SELECT a.* FROM repository_advisories a JOIN repositories r ON r.id=a.repo_id LEFT JOIN users u ON u.id=?1 WHERE (?2 IS NULL OR a.repo_id=?2) AND (?3 IS NULL OR (r.owner_kind='org' AND r.owner_org_id=?3)) AND (?4 IS NULL OR a.state=?4) AND (?5 IS NULL OR ({column},a.id) {compare} (julianday(?5),?6)) AND ({}) ORDER BY {column} {order},a.id {order} LIMIT ?7 OFFSET ?8",visibility());
    let mut rows = sqlx::query_as(&sql)
        .bind(actor)
        .bind(repo)
        .bind(org)
        .bind(options.state)
        .bind(time)
        .bind(id)
        .bind(options.limit.clamp(1, 101))
        .bind(options.offset.clamp(0, 100000))
        .fetch_all(pool)
        .await?;
    if options.before {
        rows.reverse();
    }
    Ok(rows)
}
pub async fn get(
    pool: &SqlitePool,
    actor: Option<i64>,
    repo: i64,
    ghsa: &str,
) -> Result<Option<Advisory>> {
    Ok(sqlx::query_as(&format!("SELECT a.* FROM repository_advisories a JOIN repositories r ON r.id=a.repo_id LEFT JOIN users u ON u.id=?1 WHERE a.repo_id=?2 AND a.ghsa_id=?3 AND ({})",visibility())).bind(actor).bind(repo).bind(ghsa).fetch_optional(pool).await?)
}
pub async fn create(
    pool: &SqlitePool,
    actor: i64,
    repo: i64,
    value: &Value,
    report: bool,
) -> Result<Advisory> {
    anyhow::ensure!(value.get("state").is_none(), Refusal::Invalid);
    if let Some(fork) = value.get("start_private_fork") {
        anyhow::ensure!(!report && fork.is_boolean(), Refusal::Invalid);
    }
    let data = Data::parse(value, None, report)?;
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let report_id = if report {
        let input = crate::backend::db::repositories::security::NewReport::new(
            &data.summary,
            &data.description,
        )?;
        Some(super::repositories::security::create_report_in(&mut tx, actor, repo, &input).await?)
    } else {
        anyhow::ensure!(
            super::discussions::lock_admin_authority(&mut tx, actor, repo).await?,
            Refusal::Denied
        );
        None
    };
    let row=sqlx::query_as("INSERT INTO repository_advisories(repo_id,ghsa_id,author_id,state,document,report_id) VALUES(?1,?2,?3,?4,?5,?6) RETURNING *").bind(repo).bind(identifier()).bind(actor).bind(if report {"triage"} else {"draft"}).bind(serde_json::to_string(&data)?).bind(report_id).fetch_one(&mut *tx).await?;
    tx.commit().await?;
    Ok(row)
}
async fn administration(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    actor: i64,
    repo: i64,
    ghsa: &str,
) -> Result<Advisory> {
    anyhow::ensure!(
        super::discussions::lock_admin_authority(tx, actor, repo).await?,
        Refusal::Denied
    );
    Ok(
        sqlx::query_as("SELECT * FROM repository_advisories WHERE repo_id=?1 AND ghsa_id=?2")
            .bind(repo)
            .bind(ghsa)
            .fetch_optional(&mut **tx)
            .await?
            .ok_or(Refusal::Missing)?,
    )
}
pub async fn update(
    pool: &SqlitePool,
    actor: i64,
    repo: i64,
    ghsa: &str,
    patch: &Value,
) -> Result<Advisory> {
    anyhow::ensure!(patch.get("start_private_fork").is_none(), Refusal::Invalid);
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let row = administration(&mut tx, actor, repo, ghsa).await?;
    let previous = Data::from_row(&row)?;
    let data = Data::parse(patch, Some(&previous), false)?;
    let state = if let Some(state) = patch.get("state") {
        state.as_str().ok_or(Refusal::Invalid)?
    } else {
        &row.state
    };
    anyhow::ensure!(
        matches!(state, "draft" | "published" | "closed")
            || (state == "triage" && patch.get("state").is_none()),
        Refusal::Invalid
    );
    anyhow::ensure!(
        row.state != "published" || state == "published",
        Refusal::Conflict
    );
    anyhow::ensure!(
        row.state != "triage" || state != "published",
        Refusal::Conflict
    );
    if state == "published" {
        anyhow::ensure!(
            !data.vulnerabilities.is_empty()
                && data
                    .vulnerabilities
                    .iter()
                    .all(|v| v.package.name.is_some())
                && data.severity.is_some(),
            Refusal::Invalid
        );
    }
    let now = Utc::now();
    let published_at = if state == "published" {
        Some(row.published_at.unwrap_or(now))
    } else {
        row.published_at
    };
    let publisher = if state == "published" {
        row.publisher_id.or(Some(actor))
    } else {
        row.publisher_id
    };
    let closed_at = if state == "closed" {
        row.closed_at.or(Some(now))
    } else {
        None
    };
    let updated=sqlx::query_as("UPDATE repository_advisories SET document=?3,state=?4,updated_at=?5,published_at=?6,publisher_id=?7,closed_at=?8,report_accepted=?9 WHERE repo_id=?1 AND ghsa_id=?2 RETURNING *").bind(repo).bind(ghsa).bind(serde_json::to_string(&data)?).bind(state).bind(now).bind(published_at).bind(publisher).bind(closed_at).bind(row.report_accepted||(row.state=="triage"&&state=="draft")).fetch_one(&mut *tx).await?;
    if let Some(report) = row.report_id {
        sqlx::query("UPDATE repository_security_reports SET state=?2,updated_at=?3 WHERE id=?1 AND repo_id=?4").bind(report).bind(if state=="closed" {"closed"} else {"open"}).bind(now).bind(repo).execute(&mut *tx).await?;
    }
    tx.commit().await?;
    Ok(updated)
}
pub async fn request_cve(pool: &SqlitePool, actor: i64, repo: i64, ghsa: &str) -> Result<()> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let row = administration(&mut tx, actor, repo, ghsa).await?;
    anyhow::ensure!(
        row.state == "draft"
            && row.cve_requested_at.is_none()
            && Data::from_row(&row)?.cve_id.is_none(),
        Refusal::Conflict
    );
    sqlx::query("UPDATE repository_advisories SET cve_requested_at=?3,updated_at=?3 WHERE repo_id=?1 AND ghsa_id=?2").bind(repo).bind(ghsa).bind(Utc::now()).execute(&mut *tx).await?;
    tx.commit().await?;
    Ok(())
}
pub async fn attach_fork(
    pool: &SqlitePool,
    actor: i64,
    repo: i64,
    ghsa: &str,
    fork: i64,
) -> Result<()> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let row = administration(&mut tx, actor, repo, ghsa).await?;
    anyhow::ensure!(
        row.state == "draft" && row.private_fork_id.is_none(),
        Refusal::Conflict
    );
    let valid:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM repositories WHERE id=?1 AND is_private AND owner_kind='user' AND owner_user_id=?2 AND forked_from_id=?3)").bind(fork).bind(actor).bind(repo).fetch_one(&mut *tx).await?;
    anyhow::ensure!(valid, Refusal::Denied);
    sqlx::query("UPDATE repositories SET visibility_locked=true WHERE id=?1")
        .bind(fork)
        .execute(&mut *tx)
        .await?;
    sqlx::query("UPDATE repository_advisories SET private_fork_id=?3,updated_at=?4 WHERE repo_id=?1 AND ghsa_id=?2").bind(repo).bind(ghsa).bind(fork).bind(Utc::now()).execute(&mut *tx).await?;
    tx.commit().await?;
    Ok(())
}
