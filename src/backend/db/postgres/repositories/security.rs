use super::access::RepositoryRowLock;
use crate::backend::db::repositories::security::{NewReport, Refusal, Reply, Report, Thread};
use anyhow::Result;
use sqlx::{PgPool, Postgres, Transaction};

async fn authority(
    tx: &mut Transaction<'_, Postgres>,
    actor: i64,
    repo: i64,
    write: bool,
) -> Result<bool> {
    // Writers must still be able to take part: blocks and interaction limits apply, as in SQLite.
    if write {
        anyhow::ensure!(
            crate::backend::db::postgres::discussions::lock_contribution_authority_exclusive(
                tx, actor, repo
            )
            .await?,
            Refusal::Denied
        );
    }
    let lock = if write {
        RepositoryRowLock::Exclusive
    } else {
        RepositoryRowLock::Shared
    };
    let a = super::access::lock_user_repo_access_in_transaction(tx, actor, repo, lock, !write)
        .await?
        .ok_or(Refusal::Denied)?;
    let personal = !a.organization && (a.is_admin || a.owner_user_id == Some(actor));
    anyhow::ensure!(a.access.can_read() || personal, Refusal::Denied);
    anyhow::ensure!(
        !write || !a.organization || a.organization_usable,
        Refusal::Denied
    );
    Ok((!a.organization || a.organization_usable) && (a.access.can_admin() || personal))
}
pub async fn enabled(pool: &PgPool, repo: i64) -> Result<bool> {
    Ok(sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM repository_private_reporting p JOIN repositories r ON r.id=p.repo_id WHERE r.id=$1 AND NOT r.is_private)").bind(repo).fetch_one(pool).await?)
}
pub async fn set_enabled(pool: &PgPool, actor: impl Into<crate::backend::db::users::tokens::Actor>, repo: i64, enabled: bool) -> Result<()> {
    let grant = actor.into();
    let actor = grant.user_id;
    let mut tx = pool.begin().await?;
    anyhow::ensure!(
        crate::backend::db::postgres::discussions::lock_admin_authority(&mut tx, actor, repo)
            .await?,
        Refusal::Denied
    );
    if let Some(token) = grant.token_id {
        anyhow::ensure!(super::super::users::tokens::permits_write(&mut tx, actor, token, Some(repo), None, "administration").await?, Refusal::Denied);
    }
    if let Some(configuration) = super::super::code_security::repository_configuration(&mut *tx, repo).await? {
        if let Some(fixed) = crate::backend::db::code_security::private_reporting(&configuration.settings, true)? {
            anyhow::ensure!(fixed == enabled, Refusal::Denied);
        }
    }
    let private: bool = sqlx::query_scalar("SELECT is_private FROM repositories WHERE id=$1")
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
        sqlx::query("INSERT INTO repository_private_reporting (repo_id,enabled_by_id) VALUES ($1,$2) ON CONFLICT(repo_id) DO NOTHING").bind(repo).bind(actor).execute(&mut *tx).await?;
    } else {
        sqlx::query("DELETE FROM repository_private_reporting WHERE repo_id=$1")
            .bind(repo)
            .execute(&mut *tx)
            .await?;
    }
    tx.commit().await?;
    Ok(())
}
pub async fn create_report(
    pool: &PgPool,
    actor: i64,
    repo: i64,
    input: &NewReport<'_>,
) -> Result<i64> {
    let mut tx = pool.begin().await?;
    let id = create_report_in(&mut tx, actor, repo, input).await?;
    tx.commit().await?;
    Ok(id)
}
pub(crate) async fn create_report_in(
    tx: &mut Transaction<'_, Postgres>,
    actor: i64,
    repo: i64,
    input: &NewReport<'_>,
) -> Result<i64> {
    authority(tx, actor, repo, true).await?;
    let enabled:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM repository_private_reporting p JOIN repositories r ON r.id=p.repo_id WHERE r.id=$1 AND NOT r.is_private)").bind(repo).fetch_one(&mut **tx).await?;
    anyhow::ensure!(enabled, Refusal::Denied);
    let recent:i64=sqlx::query_scalar("SELECT COUNT(*) FROM repository_security_reports WHERE repo_id=$1 AND reporter_id=$2 AND created_at>=NOW()-INTERVAL '1 day'").bind(repo).bind(actor).fetch_one(&mut **tx).await?;
    anyhow::ensure!(recent < 5, Refusal::Limit);
    Ok(sqlx::query_scalar("INSERT INTO repository_security_reports (repo_id,reporter_id,title,body) VALUES ($1,$2,$3,$4) RETURNING id").bind(repo).bind(actor).bind(input.title).bind(input.body).fetch_one(&mut **tx).await?)
}
pub async fn list_reports(
    pool: &PgPool,
    actor: i64,
    session: &str,
    repo: i64,
    limit: i64,
    offset: i64,
) -> Result<Vec<Report>> {
    let mut tx = pool.begin().await?;
    let admin = authority(&mut tx, actor, repo, false).await?;
    anyhow::ensure!(super::super::users::transitions::live_session(&mut tx, actor, session).await?, Refusal::Denied);
    let rows=sqlx::query_as::<_,Report>("SELECT s.*,u.username AS reporter FROM repository_security_reports s LEFT JOIN users u ON u.id=s.reporter_id WHERE s.repo_id=$1 AND (s.reporter_id=$2 OR $3) ORDER BY s.id DESC LIMIT $4 OFFSET $5").bind(repo).bind(actor).bind(admin).bind(limit.clamp(1,101)).bind(offset.clamp(0,100000)).fetch_all(&mut *tx).await?;
    tx.commit().await?;
    Ok(rows)
}
async fn report_in(
    tx: &mut Transaction<'_, Postgres>,
    actor: i64,
    repo: i64,
    id: i64,
    admin: bool,
) -> Result<Report> {
    Ok(sqlx::query_as::<_,Report>("SELECT s.*,u.username AS reporter FROM repository_security_reports s LEFT JOIN users u ON u.id=s.reporter_id WHERE s.repo_id=$1 AND s.id=$2 AND (s.reporter_id=$3 OR $4)").bind(repo).bind(id).bind(actor).bind(admin).fetch_optional(&mut **tx).await?.ok_or(Refusal::Missing)?)
}
pub async fn report_thread(
    pool: &PgPool,
    actor: i64,
    session: &str,
    repo: i64,
    id: i64,
    offset: i64,
) -> Result<Thread> {
    let mut tx = pool.begin().await?;
    let admin = authority(&mut tx, actor, repo, false).await?;
    anyhow::ensure!(super::super::users::transitions::live_session(&mut tx, actor, session).await?, Refusal::Denied);
    let report = report_in(&mut tx, actor, repo, id, admin).await?;
    let replies=sqlx::query_as::<_,Reply>("SELECT s.*,u.username AS author FROM repository_security_replies s LEFT JOIN users u ON u.id=s.author_id WHERE s.report_id=$1 ORDER BY s.id LIMIT 21 OFFSET $2").bind(id).bind(offset.clamp(0,100000)).fetch_all(&mut *tx).await?;
    tx.commit().await?;
    Ok(Thread { report, replies })
}
pub async fn reply(pool: &PgPool, actor: i64, repo: i64, id: i64, body: &str) -> Result<()> {
    crate::backend::db::repositories::security::validate_body(body)?;
    let mut tx = pool.begin().await?;
    let admin = authority(&mut tx, actor, repo, true).await?;
    let report = report_in(&mut tx, actor, repo, id, admin).await?;
    anyhow::ensure!(
        report.state == "open",
        Refusal::Invalid("Reopen the report before replying")
    );
    let count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM repository_security_replies WHERE report_id=$1")
            .bind(id)
            .fetch_one(&mut *tx)
            .await?;
    anyhow::ensure!(count < 1000, Refusal::Limit);
    sqlx::query(
        "INSERT INTO repository_security_replies (report_id,author_id,body) VALUES ($1,$2,$3)",
    )
    .bind(id)
    .bind(actor)
    .bind(body)
    .execute(&mut *tx)
    .await?;
    sqlx::query("UPDATE repository_security_reports SET updated_at=NOW() WHERE id=$1")
        .bind(id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(())
}
pub async fn set_state(pool: &PgPool, actor: i64, repo: i64, id: i64, closed: bool) -> Result<()> {
    let mut tx = pool.begin().await?;
    let admin = authority(&mut tx, actor, repo, true).await?;
    report_in(&mut tx, actor, repo, id, admin).await?;
    sqlx::query("UPDATE repository_security_reports SET state=$2,updated_at=NOW() WHERE id=$1")
        .bind(id)
        .bind(if closed { "closed" } else { "open" })
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(())
}
