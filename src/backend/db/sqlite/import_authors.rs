use crate::backend::db::{import_authors::Author, source_imports::ImportRefusal};
use anyhow::Result;
use sqlx::SqlitePool;

pub async fn record(
    pool: &SqlitePool,
    actor: i64,
    repo: i64,
    authors: &[(String, String)],
) -> Result<Vec<Author>> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    anyhow::ensure!(
        super::discussions::lock_admin_authority(&mut tx, actor, repo).await?,
        ImportRefusal::Denied
    );
    let mut rows = Vec::new();
    for (name, email) in authors {
        let row = sqlx::query_as("INSERT INTO repository_import_authors(repo_id,author_name,author_email) VALUES($1,$2,$3) ON CONFLICT(repo_id,author_name,author_email) DO UPDATE SET author_name=excluded.author_name RETURNING *").bind(repo).bind(name).bind(email).fetch_one(&mut *tx).await?;
        rows.push(row);
    }
    tx.commit().await?;
    Ok(rows)
}

pub async fn map(
    pool: &SqlitePool,
    actor: i64,
    repo: i64,
    author: i64,
    name: Option<&str>,
    email: Option<&str>,
) -> Result<Option<Author>> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    anyhow::ensure!(
        super::discussions::lock_admin_authority(&mut tx, actor, repo).await?,
        ImportRefusal::Denied
    );
    let row = sqlx::query_as("UPDATE repository_import_authors SET mapped_name=COALESCE($3,mapped_name),mapped_email=COALESCE($4,mapped_email) WHERE repo_id=$1 AND id=$2 AND EXISTS(SELECT 1 FROM repository_imports WHERE repo_id=$1) RETURNING *").bind(repo).bind(author).bind(name).bind(email).fetch_optional(&mut *tx).await?;
    tx.commit().await?;
    Ok(row)
}
