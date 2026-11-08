use crate::code_scanning::Analysis;
use crate::code_scanning::NewFinding;
use crate::code_scanning::ANALYSIS_COLUMNS;
use anyhow::Result;
use codeyard_database::pool::PgPool;

pub async fn pull_analyses(
    pool: &PgPool,
    repo: i64,
    head: &str,
    number: i32,
    branch: Option<&str>,
) -> Result<Vec<Analysis>> {
    Ok(sqlx::query_as(&format!("SELECT * FROM (SELECT {ANALYSIS_COLUMNS},ROW_NUMBER() OVER(PARTITION BY tool_key,category ORDER BY upload_sequence DESC,id DESC) AS newest FROM code_scanning_analyses a WHERE repo_id=$1 AND commit_sha=$2 AND (ref_name IN ('refs/pull/' || CAST($3 AS TEXT) || '/head','refs/pull/' || CAST($3 AS TEXT) || '/merge') OR ref_name=$4)) revisions WHERE newest=1 ORDER BY id DESC LIMIT 101"))
        .bind(repo).bind(head).bind(number).bind(branch).fetch_all(pool).await?)
}

pub async fn pull_processing(
    pool: &PgPool,
    repo: i64,
    head: &str,
    number: i32,
    branch: Option<&str>,
) -> Result<Option<(String, Option<String>)>> {
    Ok(sqlx::query_as("SELECT processing_status,error FROM code_scanning_uploads WHERE repo_id=$1 AND commit_sha=$2 AND (ref_name IN ('refs/pull/' || CAST($3 AS TEXT) || '/head','refs/pull/' || CAST($3 AS TEXT) || '/merge') OR ref_name=$4) ORDER BY sequence DESC LIMIT 1")
        .bind(repo).bind(head).bind(number).bind(branch).fetch_optional(pool).await?)
}

pub async fn baseline_analysis(
    pool: &PgPool,
    repo: i64,
    tool: &str,
    category: &str,
    reference: &str,
    commit: &str,
) -> Result<Option<i64>> {
    Ok(sqlx::query_scalar("SELECT id FROM code_scanning_analyses WHERE repo_id=$1 AND tool_key=$2 AND category=$3 AND ref_name=$4 AND commit_sha=$5 ORDER BY upload_sequence DESC,id DESC LIMIT 1")
        .bind(repo).bind(tool).bind(category).bind(reference).bind(commit).fetch_optional(pool).await?)
}

pub async fn new_findings(
    pool: &PgPool,
    analysis: i64,
    baseline: Option<i64>,
) -> Result<Vec<NewFinding>> {
    Ok(sqlx::query_as("SELECT a.number,f.rule,f.message,f.location,COUNT(*) OVER() AS total_count FROM code_scanning_analysis_findings f JOIN code_scanning_alerts a ON a.id=f.alert_id WHERE f.analysis_id=$1 AND a.state<>'dismissed' AND (CAST($2 AS BIGINT) IS NULL OR NOT EXISTS(SELECT 1 FROM code_scanning_analysis_findings b WHERE b.analysis_id=$2 AND b.alert_id=f.alert_id)) ORDER BY a.number LIMIT 101")
        .bind(analysis).bind(baseline).fetch_all(pool).await?)
}
