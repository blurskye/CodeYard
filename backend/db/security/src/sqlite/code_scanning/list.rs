use crate::code_scanning::Alert;
use crate::code_scanning::AlertFilters;
use crate::code_scanning::AlertScope;
use crate::code_scanning::AnalysisEntry;
use crate::code_scanning::AnalysisFilters;
use crate::code_scanning::BranchAlert;
use crate::code_scanning::ANALYSIS_COLUMNS;
use crate::code_scanning::ANALYSIS_DELETABLE;
use anyhow::ensure;
use anyhow::Result;
use sqlx::SqlitePool;

pub async fn analyses(
    pool: &SqlitePool,
    repo: i64,
    filters: &AnalysisFilters<'_>,
    offset: i64,
    limit: i64,
) -> Result<Vec<AnalysisEntry>> {
    ensure!(
        offset >= 0 && (1..=101).contains(&limit),
        "Invalid analysis page"
    );
    let direction = if filters.ascending { "ASC" } else { "DESC" };
    Ok(sqlx::query_as(&format!("SELECT {ANALYSIS_COLUMNS},{ANALYSIS_DELETABLE} AS deletable FROM code_scanning_analyses a JOIN (
        SELECT id FROM code_scanning_analyses WHERE repo_id=$1
        AND (CAST($2 AS TEXT) IS NULL OR ref_name=$2)
        AND (CAST($3 AS TEXT) IS NULL OR json_extract(tool,'$.name')=$3)
        AND (CAST($4 AS TEXT) IS NULL OR json_extract(tool,'$.guid')=$4)
        AND (CAST($5 AS TEXT) IS NULL OR upload_id=$5)
        AND (CAST($6 AS BIGINT) IS NULL OR ref_name IN ('refs/pull/' || CAST($6 AS TEXT) || '/head','refs/pull/' || CAST($6 AS TEXT) || '/merge'))
        ORDER BY created_at {direction},id {direction} LIMIT $7 OFFSET $8
        ) page ON page.id=a.id ORDER BY a.created_at {direction},a.id {direction}"))
        .bind(repo).bind(filters.reference).bind(filters.tool_name).bind(filters.tool_guid).bind(filters.sarif_id).bind(filters.pull_request).bind(limit).bind(offset).fetch_all(pool).await?)
}

pub async fn analysis(pool: &SqlitePool, repo: i64, id: i64) -> Result<Option<AnalysisEntry>> {
    Ok(sqlx::query_as(&format!("SELECT {ANALYSIS_COLUMNS},{ANALYSIS_DELETABLE} AS deletable FROM code_scanning_analyses a WHERE a.repo_id=$1 AND a.id=$2")).bind(repo).bind(id).fetch_optional(pool).await?)
}

pub async fn analysis_sarif(pool: &SqlitePool, repo: i64, id: i64) -> Result<Option<String>> {
    Ok(
        sqlx::query_scalar("SELECT sarif FROM code_scanning_analyses WHERE repo_id=$1 AND id=$2")
            .bind(repo)
            .bind(id)
            .fetch_optional(pool)
            .await?,
    )
}

pub async fn alert_page(
    pool: &SqlitePool,
    scope: AlertScope,
    filters: &AlertFilters<'_>,
    offset: i64,
    limit: i64,
) -> Result<Vec<Alert>> {
    let mut connection = pool.acquire().await?;
    alert_page_on(&mut connection, scope, filters, offset, limit).await
}

pub(super) async fn alert_page_on(
    connection: &mut sqlx::SqliteConnection,
    scope: AlertScope,
    filters: &AlertFilters<'_>,
    offset: i64,
    limit: i64,
) -> Result<Vec<Alert>> {
    ensure!(
        offset >= 0 && (1..=101).contains(&limit),
        "Invalid alert page"
    );
    ensure!(
        filters.pull_request.is_none_or(|number| number > 0),
        "Invalid pull request"
    );
    let (repo, org) = scope.ids();
    let (sort, direction) = filters.sort.sql();
    let reference =
        crate::code_scanning::instance_condition("$3", "$10", "'refs/heads/' || r.default_branch");
    let rows:Vec<BranchAlert>=sqlx::query_as(&format!(
        "SELECT a.*,page.ref_state FROM code_scanning_alerts a JOIN (
            SELECT id,ref_state FROM (
            SELECT a.id,a.created_at,a.updated_at,CASE WHEN a.state='dismissed' THEN 'dismissed'
                WHEN EXISTS(SELECT 1 FROM code_scanning_instances i WHERE i.alert_id=a.id AND {reference} AND i.state='open') THEN 'open'
                ELSE 'fixed' END AS ref_state
            FROM code_scanning_alerts a JOIN repositories r ON r.id=a.repo_id
            WHERE (a.repo_id=$1 OR (r.owner_kind='org' AND r.owner_user_id IS NULL AND r.owner_org_id=$2))
                AND EXISTS(SELECT 1 FROM code_scanning_instances i WHERE i.alert_id=a.id AND {reference})
                AND (CAST($4 AS TEXT) IS NULL OR json_extract(a.tool,'$.name')=$4)
                AND (CAST($5 AS TEXT) IS NULL OR json_extract(a.tool,'$.guid')=$5)
                AND (CAST($7 AS TEXT) IS NULL OR json_extract(a.rule,'$.severity')=$7 OR json_extract(a.rule,'$.security_severity_level')=$7)
        ) scoped WHERE (CAST($6 AS TEXT) IS NULL OR ref_state=$6 OR ($6='closed' AND ref_state<>'open'))
        ORDER BY {sort} {direction},id {direction} LIMIT $8 OFFSET $9
        ) page ON page.id=a.id ORDER BY a.{sort} {direction},a.id {direction}"
    )).bind(repo).bind(org).bind(filters.reference).bind(filters.tool_name).bind(filters.tool_guid).bind(filters.state).bind(filters.severity).bind(limit).bind(offset).bind(filters.pull_request).fetch_all(connection).await?;
    Ok(rows.into_iter().map(BranchAlert::into_alert).collect())
}
