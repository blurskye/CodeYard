use crate::backend::db::models::{RepoContributor, RepoLanguage};
use anyhow::Result;
use sqlx::SqlitePool;

pub async fn upsert_repo_languages(
    pool: &SqlitePool,
    repo_id: i64,
    expected_branch: &str,
    languages: &[(String, i64, f32, String)],
) -> Result<()> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let branch: String = sqlx::query_scalar("SELECT default_branch FROM repositories WHERE id=?1")
        .bind(repo_id)
        .fetch_one(&mut *tx)
        .await?;
    if branch != expected_branch {
        return Ok(());
    }
    sqlx::query("DELETE FROM repo_languages WHERE repo_id=?1")
        .bind(repo_id)
        .execute(&mut *tx)
        .await?;

    for (language, bytes, percentage, color) in languages {
        sqlx::query(
            r#"
            INSERT INTO repo_languages (repo_id, language, bytes, percentage, color)
            VALUES (?1, ?2, ?3, round(?4,2), ?5)
            "#,
        )
        .bind(repo_id)
        .bind(language)
        .bind(bytes)
        .bind(percentage)
        .bind(color)
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    Ok(())
}

pub async fn get_repo_languages(pool: &SqlitePool, repo_id: i64) -> Result<Vec<RepoLanguage>> {
    let langs = sqlx::query_as::<_, RepoLanguage>(
        "SELECT id,repo_id,language,bytes,CAST(percentage AS REAL) AS percentage,color,updated_at FROM repo_languages WHERE repo_id=?1 ORDER BY bytes DESC",
    )
    .bind(repo_id)
    .fetch_all(pool)
    .await?;

    Ok(langs)
}

/// The primary language of each repository in `repo_ids`, the one with the most bytes. A
/// repository without stored languages has no row.
pub async fn get_primary_repo_languages(
    pool: &SqlitePool,
    repo_ids: &[i64],
) -> Result<Vec<RepoLanguage>> {
    if repo_ids.is_empty() {
        return Ok(Vec::new());
    }
    let mut query = sqlx::QueryBuilder::<sqlx::Sqlite>::new(
        "SELECT id,repo_id,language,bytes,CAST(percentage AS REAL) AS percentage,color,updated_at FROM (SELECT *, ROW_NUMBER() OVER (PARTITION BY repo_id ORDER BY bytes DESC, language) AS rn FROM repo_languages WHERE repo_id IN (",
    );
    let mut ids = query.separated(",");
    for id in repo_ids {
        ids.push_bind(id);
    }
    query.push(")) ranked WHERE rn = 1");
    Ok(query.build_query_as().fetch_all(pool).await?)
}

pub async fn upsert_repo_contributors(
    pool: &SqlitePool,
    repo_id: i64,
    contributors: &[(String, String, i32, Option<i64>)],
) -> Result<()> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    sqlx::query("SELECT id FROM repositories WHERE id=?1")
        .bind(repo_id)
        .fetch_one(&mut *tx)
        .await?;
    sqlx::query("DELETE FROM repo_contributors WHERE repo_id=?1")
        .bind(repo_id)
        .execute(&mut *tx)
        .await?;

    for (name, email, commits, user_id) in contributors {
        sqlx::query(
            r#"
            INSERT INTO repo_contributors (repo_id, name, email, commits_count, user_id)
            VALUES (?1, ?2, ?3, ?4, ?5)
            "#,
        )
        .bind(repo_id)
        .bind(name)
        .bind(email)
        .bind(commits)
        .bind(user_id)
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    Ok(())
}

pub async fn get_repo_contributors(
    pool: &SqlitePool,
    repo_id: i64,
    limit: i64,
) -> Result<Vec<RepoContributor>> {
    let contributors = sqlx::query_as::<_, RepoContributor>(
        "SELECT * FROM repo_contributors WHERE repo_id = ?1 ORDER BY commits_count DESC LIMIT ?2",
    )
    .bind(repo_id)
    .bind(limit)
    .fetch_all(pool)
    .await?;

    Ok(contributors)
}
