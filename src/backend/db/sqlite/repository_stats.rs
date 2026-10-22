use crate::backend::db::models::RepoLanguage;
use anyhow::Result;
use sqlx::SqlitePool;

use crate::backend::db::repository_stats::{LanguageSource, RepoActivityPoint, WorkflowUsage};

pub async fn language_source(pool: &SqlitePool, repo_id: i64) -> Result<Option<LanguageSource>> {
    Ok(sqlx::query_as("SELECT id AS repo_id,default_branch,storage_node_id,write_version FROM repositories WHERE id=?1")
        .bind(repo_id).fetch_optional(pool).await?)
}

pub async fn languages_computed(
    pool: &SqlitePool,
    source: &LanguageSource,
    revision: &str,
) -> Result<bool> {
    Ok(sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM repo_language_cache WHERE repo_id=?1 AND branch=?2 AND storage_node_id IS ?3 AND write_version=?4 AND revision=?5)")
        .bind(source.repo_id).bind(&source.default_branch).bind(source.storage_node_id).bind(source.write_version).bind(revision).fetch_one(pool).await?)
}

pub async fn upsert_repo_languages(
    pool: &SqlitePool,
    source: &LanguageSource,
    revision: &str,
    languages: &[(String, i64, f32, String)],
) -> Result<bool> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let repo_id = source.repo_id;
    let current: Option<LanguageSource> = sqlx::query_as("SELECT id AS repo_id,default_branch,storage_node_id,write_version FROM repositories WHERE id=?1")
        .bind(repo_id).fetch_optional(&mut *tx).await?;
    if !current.is_some_and(|current| {
        current.default_branch == source.default_branch
            && current.storage_node_id == source.storage_node_id
            && current.write_version == source.write_version
    }) {
        return Ok(false);
    }
    sqlx::query("DELETE FROM repo_languages WHERE repo_id=?1")
        .bind(repo_id)
        .execute(&mut *tx)
        .await?;

    for (language, bytes, percentage, color) in languages {
        sqlx::query(
            r#"
            INSERT INTO repo_languages (repo_id, language, bytes, percentage, color)
            VALUES (?1, ?2, ?3, ?4, ?5)
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
    sqlx::query("INSERT INTO repo_language_cache(repo_id,branch,storage_node_id,write_version,revision) VALUES(?1,?2,?3,?4,?5) ON CONFLICT(repo_id) DO UPDATE SET branch=EXCLUDED.branch,storage_node_id=EXCLUDED.storage_node_id,write_version=EXCLUDED.write_version,revision=EXCLUDED.revision")
        .bind(repo_id).bind(&source.default_branch).bind(source.storage_node_id).bind(source.write_version).bind(revision).execute(&mut *tx).await?;
    tx.commit().await?;
    Ok(true)
}

pub async fn language_revision(
    pool: &SqlitePool,
    repo_id: i64,
    branch: &str,
) -> Result<Option<String>> {
    Ok(sqlx::query_scalar::<_, String>(
        "SELECT c.revision FROM repo_language_cache c JOIN repositories r ON r.id=c.repo_id WHERE c.repo_id=?1 AND c.branch=?2 AND r.default_branch=c.branch AND r.storage_node_id IS c.storage_node_id AND r.write_version=c.write_version",
    )
    .bind(repo_id)
    .bind(branch)
    .fetch_optional(pool)
    .await?)
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

pub async fn list_repo_activity_points(
    pool: &SqlitePool,
    repo_id: i64,
    days: i32,
) -> Result<Vec<RepoActivityPoint>> {
    // Stored timestamps are canonical UTC text, so they compare directly with `moment`.
    Ok(sqlx::query_as(r#"WITH RECURSIVE
        window_start(moment) AS (SELECT strftime('%Y-%m-%dT00:00:00.000000Z','now',printf('%d days',1-?2))),
        day_span(day) AS (
            SELECT date('now',printf('%d days',1-?2))
            UNION ALL SELECT date(day,'+1 day') FROM day_span WHERE day<date('now')
        ), events(day,commits,pulls_opened,pulls_merged,issues_opened,issues_closed,runs) AS (
            SELECT date(created_at),commit_count,0,0,0,0,0 FROM repository_pushes,window_start
                WHERE repo_id=?1 AND created_at>=moment
            UNION ALL SELECT date(created_at),0,1,0,0,0,0 FROM pull_requests,window_start
                WHERE repo_id=?1 AND created_at>=moment
            UNION ALL SELECT date(merged_at),0,0,1,0,0,0 FROM pull_requests,window_start
                WHERE repo_id=?1 AND merged_at>=moment
            UNION ALL SELECT date(created_at),0,0,0,1,0,0 FROM issues,window_start
                WHERE repo_id=?1 AND created_at>=moment
            UNION ALL SELECT date(closed_at),0,0,0,0,1,0 FROM issues,window_start
                WHERE repo_id=?1 AND closed_at>=moment
            UNION ALL SELECT date(created_at),0,0,0,0,0,1 FROM ci_runs,window_start
                WHERE repo_id=?1 AND created_at>=moment
        ), daily AS (
            SELECT day,SUM(commits) AS commits,SUM(pulls_opened) AS pulls_opened,SUM(pulls_merged) AS pulls_merged,
                SUM(issues_opened) AS issues_opened,SUM(issues_closed) AS issues_closed,SUM(runs) AS runs
            FROM events GROUP BY day
        )
        SELECT d.day,COALESCE(e.commits,0) AS commits,COALESCE(e.pulls_opened,0) AS pulls_opened,
            COALESCE(e.pulls_merged,0) AS pulls_merged,COALESCE(e.issues_opened,0) AS issues_opened,
            COALESCE(e.issues_closed,0) AS issues_closed,COALESCE(e.runs,0) AS runs
        FROM day_span d LEFT JOIN daily e ON e.day=d.day ORDER BY d.day"#)
        .bind(repo_id).bind(days.clamp(7, 730)).fetch_all(pool).await?)
}

pub async fn list_workflow_usage(
    pool: &SqlitePool,
    repo_id: i64,
    days: i32,
) -> Result<Vec<WorkflowUsage>> {
    Ok(sqlx::query_as(r#"SELECT workflow_path,COUNT(*) AS runs,
            SUM(status='success') AS succeeded,SUM(status='failed') AS failed,
            AVG(CASE WHEN started_at IS NOT NULL AND completed_at IS NOT NULL
                THEN (julianday(completed_at)-julianday(started_at))*86400.0 END) AS average_seconds
        FROM ci_runs WHERE repo_id=?1 AND created_at>=strftime('%Y-%m-%dT00:00:00.000000Z','now',printf('%d days',1-?2))
        GROUP BY workflow_path ORDER BY runs DESC,workflow_path LIMIT 100"#)
        .bind(repo_id).bind(days.clamp(7, 730)).fetch_all(pool).await?)
}
