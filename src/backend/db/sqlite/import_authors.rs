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
    for authors in crate::backend::db::import_authors::identities(authors).chunks(200) {
        let mut insert = sqlx::QueryBuilder::<sqlx::Sqlite>::new("INSERT INTO repository_import_authors(repo_id,identity_hash,author_name,author_email) ");
        insert.push_values(authors, |mut row, (hash, name, email)| {
            row.push_bind(repo).push_bind(hash).push_bind(*name).push_bind(*email);
        });
        insert.push(" ON CONFLICT(repo_id,identity_hash) DO NOTHING");
        insert.build().execute(&mut *tx).await?;
        let hashes: Vec<_> = authors.iter().map(|(hash, _, _)| hash).collect();
        rows.extend(sqlx::query_as::<_, Author>("SELECT * FROM repository_import_authors WHERE repo_id=$1 AND identity_hash IN (SELECT value FROM json_each($2)) ORDER BY id")
            .bind(repo).bind(serde_json::to_string(&hashes)?).fetch_all(&mut *tx).await?);
    }
    rows.sort_by_key(|row| row.id);
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
