use crate::backend::db::repositories::security::{NewReport, Refusal, Reply, Report, Thread};
use anyhow::Result;
use sqlx::{Sqlite, SqlitePool, Transaction};

async fn authority(
    tx: &mut Transaction<'_, Sqlite>,
    actor: i64,
    repo: i64,
    write: bool,
) -> Result<bool> {
    if write {
        anyhow::ensure!(
            crate::backend::db::sqlite::discussions::lock_contribution_authority(tx, actor, repo)
                .await?,
            Refusal::Denied
        );
    }
    let administrator =
        crate::backend::db::sqlite::discussions::lock_archive_authority(tx, actor, repo).await?;
    let access = super::access::user_repo_access_in_transaction(tx, actor, repo).await?;
    anyhow::ensure!(administrator || access.can_read(), Refusal::Denied);
    Ok(administrator)
}
async fn retain_credential(tx: &mut Transaction<'_, Sqlite>, actor: &crate::backend::db::users::accounts::AccountActor) -> Result<()> {
    if let Some(credential) = actor.credential() {
        crate::backend::db::sqlite::users::transitions::require_account(tx, actor.user_id(), &credential).await?;
    }
    Ok(())
}
pub async fn enabled(pool: &SqlitePool, repo: i64) -> Result<bool> {
    Ok(sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM repository_private_reporting p JOIN repositories r ON r.id=p.repo_id WHERE r.id=?1 AND NOT r.is_private)").bind(repo).fetch_one(pool).await?)
}
pub async fn set_enabled(pool: &SqlitePool, actor: impl Into<crate::backend::db::users::accounts::AccountActor>, repo: i64, enabled: bool) -> Result<()> {
    let authority = actor.into();
    let actor = authority.user_id();
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    anyhow::ensure!(
        crate::backend::db::sqlite::discussions::lock_admin_authority(&mut tx, actor, repo).await?,
        Refusal::Denied
    );
    if let Some(credential) = authority.credential() {
        crate::backend::db::sqlite::users::transitions::require_account(&mut tx, actor, &credential).await?;
    }
    let configured: Option<String> = sqlx::query_scalar("SELECT c.settings FROM code_security_configurations c JOIN repository_security_settings s ON s.configuration_id=c.id JOIN repositories r ON r.id=s.repo_id AND r.owner_org_id=c.org_id WHERE r.id=$1")
        .bind(repo).fetch_optional(&mut *tx).await?;
    if let Some(configured) = configured {
        anyhow::ensure!(crate::backend::db::code_security::permits_toggle(&configured, "private_vulnerability_reporting", enabled)?, Refusal::Denied);
    }
    let private: bool = sqlx::query_scalar("SELECT is_private FROM repositories WHERE id=?1")
        .bind(repo)
        .fetch_one(&mut *tx)
        .await?;
    anyhow::ensure!(
        !enabled || !private,
        Refusal::Invalid(
            "Private vulnerability reporting is available only on public repositories"
        )
    );
    if enabled {
        sqlx::query("INSERT INTO repository_private_reporting (repo_id,enabled_by_id) VALUES (?1,?2) ON CONFLICT(repo_id) DO NOTHING").bind(repo).bind(actor).execute(&mut *tx).await?;
    } else {
        sqlx::query("DELETE FROM repository_private_reporting WHERE repo_id=?1")
            .bind(repo)
            .execute(&mut *tx)
            .await?;
    }
    let raw: Option<String> = sqlx::query_scalar("SELECT security_and_analysis FROM repository_security_settings WHERE repo_id=$1")
        .bind(repo).fetch_optional(&mut *tx).await?;
    let settings = crate::backend::db::code_security::with_toggle_status(raw.as_deref(), "private_vulnerability_reporting", enabled)?;
    sqlx::query("INSERT INTO repository_security_settings(repo_id,security_and_analysis) VALUES($1,$2) ON CONFLICT(repo_id) DO UPDATE SET security_and_analysis=excluded.security_and_analysis")
        .bind(repo).bind(settings).execute(&mut *tx).await?;
    tx.commit().await?;
    Ok(())
}
pub async fn create_report(
    pool: &SqlitePool,
    actor: impl Into<crate::backend::db::users::accounts::AccountActor>,
    repo: i64,
    input: &NewReport<'_>,
) -> Result<i64> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let actor = actor.into();
    let id = create_report_in(&mut tx, actor.user_id(), repo, input).await?;
    retain_credential(&mut tx, &actor).await?;
    tx.commit().await?;
    Ok(id)
}
pub(crate) async fn create_report_in(
    tx: &mut Transaction<'_, Sqlite>,
    actor: i64,
    repo: i64,
    input: &NewReport<'_>,
) -> Result<i64> {
    authority(tx, actor, repo, true).await?;
    let enabled:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM repository_private_reporting p JOIN repositories r ON r.id=p.repo_id WHERE r.id=?1 AND NOT r.is_private)").bind(repo).fetch_one(&mut **tx).await?;
    anyhow::ensure!(enabled, Refusal::Denied);
    let recent:i64=sqlx::query_scalar("SELECT COUNT(*) FROM repository_security_reports WHERE repo_id=?1 AND reporter_id=?2 AND created_at>=strftime('%Y-%m-%dT%H:%M:%f000Z','now','-1 day')").bind(repo).bind(actor).fetch_one(&mut **tx).await?;
    anyhow::ensure!(recent < 5, Refusal::Limit);
    Ok(sqlx::query_scalar("INSERT INTO repository_security_reports (repo_id,reporter_id,title,body) VALUES (?1,?2,?3,?4) RETURNING id").bind(repo).bind(actor).bind(input.title).bind(input.body).fetch_one(&mut **tx).await?)
}
pub async fn list_reports(
    pool: &SqlitePool,
    actor: impl Into<crate::backend::db::users::accounts::AccountActor>,
    repo: i64,
    limit: i64,
    offset: i64,
) -> Result<Vec<Report>> {
    let retained = actor.into();
    let actor = retained.user_id();
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let admin = authority(&mut tx, actor, repo, false).await?;
    retain_credential(&mut tx, &retained).await?;
    let rows=sqlx::query_as::<_,Report>("SELECT s.*,u.username AS reporter FROM repository_security_reports s LEFT JOIN users u ON u.id=s.reporter_id WHERE s.repo_id=?1 AND (s.reporter_id=?2 OR ?3) ORDER BY s.id DESC LIMIT ?4 OFFSET ?5").bind(repo).bind(actor).bind(admin).bind(limit.clamp(1,101)).bind(offset.clamp(0,100000)).fetch_all(&mut *tx).await?;
    tx.commit().await?;
    Ok(rows)
}
async fn report_in(
    tx: &mut Transaction<'_, Sqlite>,
    actor: i64,
    repo: i64,
    id: i64,
    admin: bool,
) -> Result<Report> {
    Ok(sqlx::query_as::<_,Report>("SELECT s.*,u.username AS reporter FROM repository_security_reports s LEFT JOIN users u ON u.id=s.reporter_id WHERE s.repo_id=?1 AND s.id=?2 AND (s.reporter_id=?3 OR ?4)").bind(repo).bind(id).bind(actor).bind(admin).fetch_optional(&mut **tx).await?.ok_or(Refusal::Missing)?)
}
pub async fn report_thread(
    pool: &SqlitePool,
    actor: impl Into<crate::backend::db::users::accounts::AccountActor>,
    repo: i64,
    id: i64,
    offset: i64,
) -> Result<Thread> {
    let retained = actor.into();
    let actor = retained.user_id();
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let admin = authority(&mut tx, actor, repo, false).await?;
    retain_credential(&mut tx, &retained).await?;
    let report = report_in(&mut tx, actor, repo, id, admin).await?;
    let replies=sqlx::query_as::<_,Reply>("SELECT s.*,u.username AS author FROM repository_security_replies s LEFT JOIN users u ON u.id=s.author_id WHERE s.report_id=?1 ORDER BY s.id LIMIT 21 OFFSET ?2").bind(id).bind(offset.clamp(0,100000)).fetch_all(&mut *tx).await?;
    tx.commit().await?;
    Ok(Thread { report, replies })
}
pub async fn reply(pool: &SqlitePool, actor: impl Into<crate::backend::db::users::accounts::AccountActor>, repo: i64, id: i64, body: &str) -> Result<i64> {
    crate::backend::db::repositories::security::validate_body(body)?;
    let retained = actor.into();
    let actor = retained.user_id();
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let admin = authority(&mut tx, actor, repo, true).await?;
    retain_credential(&mut tx, &retained).await?;
    let report = report_in(&mut tx, actor, repo, id, admin).await?;
    anyhow::ensure!(
        report.state == "open",
        Refusal::Invalid("Reopen the report before replying")
    );
    let count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM repository_security_replies WHERE report_id=?1")
            .bind(id)
            .fetch_one(&mut *tx)
            .await?;
    anyhow::ensure!(count < 1000, Refusal::Limit);
    sqlx::query(
        "INSERT INTO repository_security_replies (report_id,author_id,body) VALUES (?1,?2,?3)",
    )
    .bind(id)
    .bind(actor)
    .bind(body)
    .execute(&mut *tx)
    .await?;
    sqlx::query("UPDATE repository_security_reports SET updated_at=strftime('%Y-%m-%dT%H:%M:%f000Z','now') WHERE id=?1").bind(id).execute(&mut *tx).await?;
    tx.commit().await?;
    Ok(count)
}
pub async fn set_state(
    pool: &SqlitePool,
    actor: impl Into<crate::backend::db::users::accounts::AccountActor>,
    repo: i64,
    id: i64,
    closed: bool,
) -> Result<()> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let retained = actor.into();
    let actor = retained.user_id();
    let admin = authority(&mut tx, actor, repo, true).await?;
    retain_credential(&mut tx, &retained).await?;
    report_in(&mut tx, actor, repo, id, admin).await?;
    sqlx::query("UPDATE repository_security_reports SET state=?2,updated_at=strftime('%Y-%m-%dT%H:%M:%f000Z','now') WHERE id=?1").bind(id).bind(if closed {"closed"}else {"open"}).execute(&mut *tx).await?;
    tx.commit().await?;
    Ok(())
}
