use crate::backend::db::pages::{Attempt, Build, Deployment, Refusal, Role, Site, Update, NO_PUBLISHER};
use anyhow::Result;
use sqlx::SqlitePool;

use crate::backend::db::repositories::credentials::{repository_condition, Access, Reader};

async fn begin_read<'a>(pool: &'a SqlitePool, reader: &Reader<'_>, repo: i64) -> Result<sqlx::Transaction<'a, sqlx::Sqlite>> {
    let mut tx = pool.begin().await?;
    let allowed: bool = sqlx::query_scalar(&format!("SELECT EXISTS(SELECT 1 FROM repositories r LEFT JOIN users u ON u.id=$1 WHERE r.id=$2 AND ({}) AND ({}))", super::repositories::access::readable_condition(), repository_condition(false, "pages", Access::PublicRead, "$3", "$4", "$5")))
        .bind(reader.user).bind(repo).bind(reader.token()).bind(reader.session()).bind(reader.hash()).fetch_one(&mut *tx).await?;
    anyhow::ensure!(allowed, Refusal::Missing);
    Ok(tx)
}

async fn authorize_write(tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>, actor: i64, credential: &crate::backend::db::users::accounts::AccountCredential<'_>, repo: i64, role: Role) -> Result<()> {
    let allowed = match role {
        Role::Admin => super::discussions::lock_admin_authority(tx, actor, repo).await?,
        Role::Maintain => super::discussions::lock_maintain_authority(tx, actor, repo).await?,
        Role::Write => super::discussions::lock_write_authority(tx, actor, repo).await?,
    };
    anyhow::ensure!(allowed && super::users::transitions::credential_live(tx, actor, credential, true).await?, Refusal::Denied);
    Ok(())
}

pub async fn site(pool: &SqlitePool, reader: &Reader<'_>, repo: i64) -> Result<Option<Site>> {
    let mut tx = begin_read(pool, reader, repo).await?;
    let row = sqlx::query_as("SELECT * FROM pages_sites WHERE repo_id=$1").bind(repo).fetch_optional(&mut *tx).await?;
    tx.commit().await?;
    Ok(row)
}
pub async fn save(
    pool: &SqlitePool,
    actor: i64,
    credential: &crate::backend::db::users::accounts::AccountCredential<'_>,
    repo: i64,
    input: &Update,
    create: bool,
) -> Result<Site> {
    input.validate(create)?;
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    authorize_write(&mut tx, actor, credential, repo, Role::Maintain).await?;
    let existing: Option<Site> = sqlx::query_as("SELECT * FROM pages_sites WHERE repo_id=$1")
        .bind(repo)
        .fetch_optional(&mut *tx)
        .await?;
    anyhow::ensure!(!create || existing.is_none(), Refusal::Duplicate);
    anyhow::ensure!(create || existing.is_some(), Refusal::Missing);
    let default_public: bool =
        sqlx::query_scalar("SELECT NOT is_private FROM repositories WHERE id=$1")
            .bind(repo)
            .fetch_one(&mut *tx)
            .await?;
    let previous = existing.unwrap_or(Site {
        repo_id: repo,
        build_type: "legacy".into(),
        branch: None,
        path: None,
        cname: None,
        https_enforced: false,
        public: default_public,
    });
    let cname = if input.set_cname {
        input.cname.clone()
    } else {
        previous.cname
    };
    if let Some(cname) = &cname {
        let used: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM pages_sites WHERE cname=$1 AND repo_id<>$2)",
        )
        .bind(cname)
        .bind(repo)
        .fetch_one(&mut *tx)
        .await?;
        anyhow::ensure!(!used, Refusal::Duplicate);
    }
    let (branch, path) = match &input.source {
        Some(s) => (Some(s.branch.clone()), Some(s.path.clone())),
        None => (previous.branch, previous.path),
    };
    let kind = input.build_type.as_ref().unwrap_or(&previous.build_type);
    anyhow::ensure!(kind != "legacy" || branch.is_some(), Refusal::Invalid);
    let row=sqlx::query_as("INSERT INTO pages_sites(repo_id,build_type,branch,path,cname,https_enforced,is_public) VALUES($1,$2,$3,$4,$5,$6,$7) ON CONFLICT(repo_id) DO UPDATE SET build_type=excluded.build_type,branch=excluded.branch,path=excluded.path,cname=excluded.cname,https_enforced=excluded.https_enforced,is_public=excluded.is_public RETURNING *")
        .bind(repo).bind(kind).bind(branch).bind(path).bind(cname).bind(input.https_enforced.unwrap_or(previous.https_enforced)).bind(input.public.unwrap_or(previous.public)).fetch_one(&mut *tx).await?;
    tx.commit().await?;
    Ok(row)
}
pub async fn delete(pool: &SqlitePool, actor: i64, credential: &crate::backend::db::users::accounts::AccountCredential<'_>, repo: i64) -> Result<()> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    authorize_write(&mut tx, actor, credential, repo, Role::Admin).await?;
    anyhow::ensure!(
        sqlx::query("DELETE FROM pages_sites WHERE repo_id=$1")
            .bind(repo)
            .execute(&mut *tx)
            .await?
            .rows_affected()
            == 1,
        Refusal::Missing
    );
    tx.commit().await?;
    Ok(())
}
pub async fn builds(pool: &SqlitePool, reader: &Reader<'_>, repo: i64, offset: i64, limit: i64) -> Result<Vec<Build>> {
    anyhow::ensure!(offset >= 0 && (1..=101).contains(&limit), Refusal::Invalid);
    let mut tx = begin_read(pool, reader, repo).await?;
    let rows = sqlx::query_as("SELECT * FROM pages_builds WHERE repo_id=$1 ORDER BY id DESC LIMIT $2 OFFSET $3")
        .bind(repo).bind(limit).bind(offset).fetch_all(&mut *tx).await?;
    tx.commit().await?;
    Ok(rows)
}
pub async fn build(pool: &SqlitePool, reader: &Reader<'_>, repo: i64, id: i64) -> Result<Option<Build>> {
    let mut tx = begin_read(pool, reader, repo).await?;
    let row = sqlx::query_as("SELECT * FROM pages_builds WHERE repo_id=$1 AND id=$2")
        .bind(repo).bind(id).fetch_optional(&mut *tx).await?;
    tx.commit().await?;
    Ok(row)
}
pub async fn request_build(pool: &SqlitePool, actor: i64, credential: &crate::backend::db::users::accounts::AccountCredential<'_>, repo: i64, sha: &str) -> Result<Build> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    authorize_write(&mut tx, actor, credential, repo, Role::Write).await?;
    let exists: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM pages_sites WHERE repo_id=$1 AND build_type='legacy')",
    )
    .bind(repo)
    .fetch_one(&mut *tx)
    .await?;
    anyhow::ensure!(exists, Refusal::Missing);
    let row=sqlx::query_as("INSERT INTO pages_builds(repo_id,actor_id,commit_sha,status,error) VALUES($1,$2,$3,'errored',$4) RETURNING *").bind(repo).bind(actor).bind(sha).bind(NO_PUBLISHER).fetch_one(&mut *tx).await?;
    tx.commit().await?;
    Ok(row)
}
pub async fn request_deployment(
    pool: &SqlitePool,
    actor: i64,
    credential: &crate::backend::db::users::accounts::AccountCredential<'_>,
    repo: i64,
    input: Attempt<'_>,
) -> Result<Deployment> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    authorize_write(&mut tx, actor, credential, repo, Role::Write).await?;
    let exists: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM pages_sites WHERE repo_id=$1 AND build_type='workflow')",
    )
    .bind(repo)
    .fetch_one(&mut *tx)
    .await?;
    anyhow::ensure!(exists, Refusal::Missing);
    if let Some(id) = input.artifact_id {
        let valid:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM actions_objects WHERE id=$1 AND repo_id=$2 AND kind='artifact' AND finalized AND expires_at>$3)").bind(id).bind(repo).bind(chrono::Utc::now().timestamp()).fetch_one(&mut *tx).await?;
        anyhow::ensure!(valid, Refusal::Missing);
    }
    // No publisher can verify this deployment's OIDC credential or publish its files.
    // Persist a terminal failure, never an unverified job that could later be consumed.
    let row=sqlx::query_as("INSERT INTO pages_deployments(repo_id,commit_sha,environment,status,artifact_id) VALUES($1,$2,$3,'deployment_failed',$4) RETURNING *").bind(repo).bind(input.sha).bind(input.environment).bind(input.artifact_id).fetch_one(&mut *tx).await?;
    tx.commit().await?;
    Ok(row)
}
pub async fn deployment(pool: &SqlitePool, reader: &Reader<'_>, repo: i64, id: i64) -> Result<Option<Deployment>> {
    let mut tx = begin_read(pool, reader, repo).await?;
    let row = sqlx::query_as("SELECT * FROM pages_deployments WHERE repo_id=$1 AND id=$2")
        .bind(repo).bind(id).fetch_optional(&mut *tx).await?;
    tx.commit().await?;
    Ok(row)
}
pub async fn cancel(pool: &SqlitePool, actor: i64, credential: &crate::backend::db::users::accounts::AccountCredential<'_>, repo: i64, id: i64) -> Result<()> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    authorize_write(&mut tx, actor, credential, repo, Role::Write).await?;
    let row: Deployment =
        sqlx::query_as("SELECT * FROM pages_deployments WHERE repo_id=$1 AND id=$2")
            .bind(repo)
            .bind(id)
            .fetch_optional(&mut *tx)
            .await?
            .ok_or(Refusal::Missing)?;
    anyhow::ensure!(
        matches!(
            row.status.as_str(),
            "deployment_in_progress"
                | "syncing_files"
                | "finished_file_sync"
                | "updating_pages"
                | "purging_cdn"
                | "deployment_cancelled"
        ),
        Refusal::Conflict
    );
    sqlx::query(
        "UPDATE pages_deployments SET status='deployment_cancelled' WHERE repo_id=$1 AND id=$2",
    )
    .bind(repo)
    .bind(id)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(())
}
