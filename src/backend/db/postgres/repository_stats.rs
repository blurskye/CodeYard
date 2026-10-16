use crate::backend::db::models::{RepoContributor, RepoLanguage};
use anyhow::Result;
use sqlx::PgPool;

pub async fn upsert_repo_languages(
    pool: &PgPool,
    repo_id: i64,
    languages: &[(String, i64, f32, String)],
) -> Result<()> {
    let mut tx = pool.begin().await?;
    sqlx::query("SELECT id FROM repositories WHERE id=$1 FOR UPDATE")
        .bind(repo_id)
        .fetch_one(&mut *tx)
        .await?;
    sqlx::query("DELETE FROM repo_languages WHERE repo_id=$1")
        .bind(repo_id)
        .execute(&mut *tx)
        .await?;

    for (language, bytes, percentage, color) in languages {
        sqlx::query(
            r#"
            INSERT INTO repo_languages (repo_id, language, bytes, percentage, color)
            VALUES ($1, $2, $3, $4, $5)
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

pub async fn get_repo_languages(pool: &PgPool, repo_id: i64) -> Result<Vec<RepoLanguage>> {
    let langs = sqlx::query_as::<_, RepoLanguage>(
        "SELECT id,repo_id,language,bytes,percentage::double precision AS percentage,color,updated_at FROM repo_languages WHERE repo_id=$1 ORDER BY bytes DESC",
    )
    .bind(repo_id)
    .fetch_all(pool)
    .await?;

    Ok(langs)
}

pub async fn upsert_repo_contributors(
    pool: &PgPool,
    repo_id: i64,
    contributors: &[(String, String, i32, Option<i64>)],
) -> Result<()> {
    let mut tx = pool.begin().await?;
    sqlx::query("SELECT id FROM repositories WHERE id=$1 FOR UPDATE")
        .bind(repo_id)
        .fetch_one(&mut *tx)
        .await?;
    sqlx::query("DELETE FROM repo_contributors WHERE repo_id=$1")
        .bind(repo_id)
        .execute(&mut *tx)
        .await?;

    for (name, email, commits, user_id) in contributors {
        sqlx::query(
            r#"
            INSERT INTO repo_contributors (repo_id, name, email, commits_count, user_id)
            VALUES ($1, $2, $3, $4, $5)
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
    pool: &PgPool,
    repo_id: i64,
    limit: i64,
) -> Result<Vec<RepoContributor>> {
    let contributors = sqlx::query_as::<_, RepoContributor>(
        "SELECT * FROM repo_contributors WHERE repo_id = $1 ORDER BY commits_count DESC LIMIT $2",
    )
    .bind(repo_id)
    .bind(limit)
    .fetch_all(pool)
    .await?;

    Ok(contributors)
}
