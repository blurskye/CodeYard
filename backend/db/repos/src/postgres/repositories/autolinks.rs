use crate::repositories::autolinks::Autolink;
use crate::repositories::autolinks::NewAutolink;
use crate::repositories::autolinks::Refusal;
use anyhow::Result;
use codeyard_database::pool::PgPool;
use sqlx::Postgres;
use sqlx::Transaction;

async fn admin(
    tx: &mut Transaction<'_, Postgres>,
    actor: impl Into<codeyard_database::queries::credentials::Actor>,
    repo: i64,
    write: bool,
) -> Result<()> {
    let credential = actor.into();
    let actor = credential.user_id;
    let allowed = if write {
        codeyard_database::queries::postgres::discussions::lock_repository_mutation_authority(
            tx, actor, repo,
        )
        .await?
    } else {
        codeyard_database::queries::postgres::discussions::lock_archive_authority(tx, actor, repo)
            .await?
    };
    anyhow::ensure!(allowed, Refusal::Denied);
    if write {
        anyhow::ensure!(
            codeyard_database::queries::postgres::credentials::writes(
                tx,
                &credential,
                Some(repo),
                None,
                "administration"
            )
            .await?,
            Refusal::Denied
        );
    }
    Ok(())
}
pub async fn create(
    pool: &PgPool,
    actor: impl Into<codeyard_database::queries::credentials::Actor>,
    repo: i64,
    input: &NewAutolink,
) -> Result<Autolink> {
    let mut tx = pool.begin().await?;
    admin(&mut tx, actor, repo, true).await?;
    let count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM repository_autolinks WHERE repo_id=$1")
            .bind(repo)
            .fetch_one(&mut *tx)
            .await?;
    anyhow::ensure!(count < 50, Refusal::Limit);
    let row=sqlx::query_as("INSERT INTO repository_autolinks(repo_id,key_prefix,url_template,is_alphanumeric) VALUES($1,$2,$3,$4) ON CONFLICT(repo_id,key_prefix) DO NOTHING RETURNING *").bind(repo).bind(&input.key_prefix).bind(&input.url_template).bind(input.is_alphanumeric).fetch_optional(&mut *tx).await?.ok_or(Refusal::Duplicate)?;
    tx.commit().await?;
    Ok(row)
}
pub async fn list(
    pool: &PgPool,
    actor: i64,
    repo: i64,
    limit: i64,
    offset: i64,
) -> Result<Vec<Autolink>> {
    anyhow::ensure!(
        (1..=101).contains(&limit) && offset >= 0,
        "Invalid autolink page"
    );
    let mut tx = pool.begin().await?;
    admin(&mut tx, actor, repo, false).await?;
    let rows = sqlx::query_as(
        "SELECT * FROM repository_autolinks WHERE repo_id=$1 ORDER BY id LIMIT $2 OFFSET $3",
    )
    .bind(repo)
    .bind(limit)
    .bind(offset)
    .fetch_all(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(rows)
}
pub async fn get(pool: &PgPool, actor: i64, repo: i64, id: i64) -> Result<Autolink> {
    let mut tx = pool.begin().await?;
    admin(&mut tx, actor, repo, false).await?;
    let row = sqlx::query_as("SELECT * FROM repository_autolinks WHERE repo_id=$1 AND id=$2")
        .bind(repo)
        .bind(id)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or(Refusal::Missing)?;
    tx.commit().await?;
    Ok(row)
}
pub async fn remove(
    pool: &PgPool,
    actor: impl Into<codeyard_database::queries::credentials::Actor>,
    repo: i64,
    id: i64,
) -> Result<()> {
    let mut tx = pool.begin().await?;
    admin(&mut tx, actor, repo, true).await?;
    let removed = sqlx::query("DELETE FROM repository_autolinks WHERE repo_id=$1 AND id=$2")
        .bind(repo)
        .bind(id)
        .execute(&mut *tx)
        .await?
        .rows_affected();
    anyhow::ensure!(removed == 1, Refusal::Missing);
    tx.commit().await?;
    Ok(())
}
pub async fn for_rendering(pool: &PgPool, repo: i64) -> Result<Vec<Autolink>> {
    Ok(sqlx::query_as("SELECT * FROM repository_autolinks WHERE repo_id=$1 ORDER BY length(key_prefix) DESC,id LIMIT 50").bind(repo).fetch_all(pool).await?)
}
