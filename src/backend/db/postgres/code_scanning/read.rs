//! Current scanning authority and bounded PR findings share a repeatable read snapshot.
use crate::backend::db::code_scanning::{Analysis, NewFinding, PullGroup, PullSelection, PullSnapshot, ANALYSIS_COLUMNS};
use super::super::repositories::access::USER_REPO_ACCESS_SQL;
use anyhow::Result;
use sqlx::{PgConnection, PgPool};

async fn allowed(c: &mut PgConnection, actor: i64, session: &str, repo: i64) -> Result<bool> {
    let authority: Option<(Option<i64>, bool)> = sqlx::query_as(&format!(
        "SELECT r.owner_org_id,COALESCE((r.owner_kind='user' AND u.is_admin) OR ({USER_REPO_ACCESS_SQL})>=3,FALSE) FROM repositories r JOIN users u ON u.id=$1 AND NOT u.is_banned AND NOT u.must_change_password WHERE r.id=$2 AND NOT r.is_gist AND EXISTS(SELECT 1 FROM sessions s WHERE s.id=$3 AND s.user_id=u.id AND s.expires_at>$4)"
    )).bind(actor).bind(repo).bind(session).bind(chrono::Utc::now()).fetch_optional(&mut *c).await?;
    match authority {
        Some((_, true)) => Ok(true),
        Some((Some(org), false)) => super::super::organizations::roles::manages_security(c, actor, org).await,
        _ => Ok(false),
    }
}

pub async fn security_read_allowed(pool: &PgPool, actor: i64, session: &str, repo: i64) -> Result<bool> {
    allowed(&mut *pool.acquire().await?, actor, session, repo).await
}

pub async fn read_alert_snapshot(pool: &PgPool, actor: i64, session: &str, selection: &crate::backend::db::code_scanning::AlertSelection<'_>) -> Result<crate::backend::db::code_scanning::AlertSnapshot> {
    use crate::backend::db::code_scanning::{Alert, AlertScope, AlertSnapshot, Instance, Refusal};
    anyhow::ensure!(selection.offset>=0 && (1..=101).contains(&selection.limit), "Invalid alert page");
    let mut tx = pool.begin().await?;
    sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ, READ ONLY").execute(&mut *tx).await?;
    anyhow::ensure!(allowed(&mut tx, actor, session, selection.repo).await?, Refusal::Denied);
    let rows: Vec<Alert> = match selection.number {
        Some(number) => sqlx::query_as("SELECT * FROM code_scanning_alerts WHERE repo_id=$1 AND number=$2")
            .bind(selection.repo).bind(number).fetch_optional(&mut *tx).await?.into_iter().collect(),
        None => super::list::alert_page_on(&mut tx, AlertScope::Repository(selection.repo), selection.filters, selection.offset, selection.limit).await?,
    };
    let ids: Vec<_> = rows.iter().map(|row| row.id).collect();
    let reference = match selection.filters.reference {
        Some(reference) => Some(reference.to_owned()),
        None => sqlx::query_scalar("SELECT 'refs/heads/' || default_branch FROM repositories WHERE id=$1")
            .bind(selection.repo).fetch_optional(&mut *tx).await?,
    };
    let mut instances: Vec<Instance> = if ids.is_empty() { Vec::new() } else {
        sqlx::query_as("SELECT * FROM (SELECT i.*,ROW_NUMBER() OVER(PARTITION BY alert_id ORDER BY CASE WHEN state='open' THEN 0 ELSE 1 END,upload_sequence DESC,ref_name,category) AS newest FROM code_scanning_instances i WHERE alert_id=ANY($1) AND (CAST($2 AS TEXT) IS NULL OR ref_name=$2) AND (CAST($3 AS TEXT) IS NULL OR category=$3)) page WHERE newest=1")
            .bind(&ids).bind(reference).bind(selection.category).fetch_all(&mut *tx).await?
    };
    if instances.is_empty() && selection.number.is_some() && selection.filters.reference.is_none() && !ids.is_empty() {
        instances = sqlx::query_as("SELECT * FROM code_scanning_instances WHERE alert_id=$1 AND (CAST($2 AS TEXT) IS NULL OR category=$2) ORDER BY CASE WHEN state='open' THEN 0 ELSE 1 END,upload_sequence DESC,ref_name,category LIMIT 1")
            .bind(ids[0]).bind(selection.category).fetch_all(&mut *tx).await?;
    }
    tx.commit().await?;
    Ok(AlertSnapshot { rows, instances: instances.into_iter().map(|instance| (instance.alert_id, instance)).collect() })
}

pub async fn pull_merge_candidates(pool: &PgPool, actor: i64, session: &str, repo: i64, number: i32) -> Result<Vec<String>> {
    let mut tx = pool.begin().await?;
    sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ, READ ONLY").execute(&mut *tx).await?;
    if !allowed(&mut tx, actor, session, repo).await? { return Ok(Vec::new()); }
    let reference = format!("refs/pull/{number}/merge");
    let rows = sqlx::query_scalar("SELECT commit_sha FROM (SELECT commit_sha,MAX(sequence) AS sequence FROM code_scanning_uploads WHERE repo_id=$1 AND ref_name=$2 GROUP BY commit_sha) candidates ORDER BY sequence DESC LIMIT 101")
        .bind(repo).bind(reference).fetch_all(&mut *tx).await?;
    tx.commit().await?;
    Ok(rows)
}

pub async fn pull_snapshot(pool: &PgPool, actor: i64, session: &str, selection: &PullSelection<'_>) -> Result<PullSnapshot> {
    let mut tx = pool.begin().await?;
    sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ, READ ONLY").execute(&mut *tx).await?;
    if !allowed(&mut tx, actor, session, selection.repo).await? { return Ok(PullSnapshot::default()); }
    let analyses: Vec<Analysis> = sqlx::query_as(&format!("SELECT * FROM (SELECT {ANALYSIS_COLUMNS},ROW_NUMBER() OVER(PARTITION BY tool_key,category ORDER BY upload_sequence DESC,id DESC) AS newest FROM code_scanning_analyses a WHERE repo_id=$1 AND ((commit_sha=$2 AND (ref_name='refs/pull/' || CAST($3 AS TEXT) || '/head' OR ref_name=$4)) OR (ref_name='refs/pull/' || CAST($3 AS TEXT) || '/merge' AND commit_sha=ANY($5)))) revisions WHERE newest=1 ORDER BY id DESC LIMIT 101"))
        .bind(selection.repo).bind(selection.head).bind(selection.number).bind(selection.branch).bind(selection.merge_commits).fetch_all(&mut *tx).await?;
    let mut snapshot = PullSnapshot { more: analyses.len()>100, ..Default::default() };
    let mut remaining = 100i64;
    for analysis in analyses.into_iter().take(100) {
        let baseline: Option<(i64, i64)> = sqlx::query_as("SELECT id,results_count FROM code_scanning_analyses WHERE repo_id=$1 AND tool_key=$2 AND category=$3 AND ref_name=$4 AND commit_sha=$5 ORDER BY upload_sequence DESC,id DESC LIMIT 1")
            .bind(selection.repo).bind(&analysis.tool_key).bind(&analysis.category).bind(selection.base_branch).bind(selection.base).fetch_optional(&mut *tx).await?;
        let retained: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM code_scanning_analysis_findings WHERE analysis_id=$1").bind(analysis.id).fetch_one(&mut *tx).await?;
        let mut incomplete = analysis.results_count>retained;
        if let Some((id, results)) = baseline {
            let retained: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM code_scanning_analysis_findings WHERE analysis_id=$1").bind(id).fetch_one(&mut *tx).await?;
            incomplete |= results>retained;
        }
        let baseline = baseline.map(|(id, _)| id);
        let predicate = "FROM code_scanning_analysis_findings f JOIN code_scanning_alerts a ON a.id=f.alert_id WHERE f.analysis_id=$1 AND a.state<>'dismissed' AND (CAST($2 AS BIGINT) IS NULL OR NOT EXISTS(SELECT 1 FROM code_scanning_analysis_findings b WHERE b.analysis_id=$2 AND b.alert_id=f.alert_id))";
        let total: i64 = sqlx::query_scalar(&format!("SELECT COUNT(*) {predicate}")).bind(analysis.id).bind(baseline).fetch_one(&mut *tx).await?;
        let findings: Vec<NewFinding> = if remaining==0 { Vec::new() } else {
            sqlx::query_as(&format!("SELECT a.number,json_build_object('id',substr(f.rule::json->>'id',1,256))::text AS rule,substr(f.message,1,1024) AS message,json_build_object('path',substr(f.location::json->>'path',1,4097),'start_line',f.location::json->'start_line')::text AS location,COUNT(*) OVER() AS total_count {predicate} ORDER BY a.number LIMIT $3"))
                .bind(analysis.id).bind(baseline).bind(remaining).fetch_all(&mut *tx).await?
        };
        remaining -= findings.len() as i64;
        snapshot.groups.push(PullGroup { analysis, baseline, incomplete, total, findings });
    }
    snapshot.processing = sqlx::query_as("SELECT ref_name,processing_status,substr(error,1,512) FROM (SELECT ref_name,processing_status,error,ROW_NUMBER() OVER(PARTITION BY ref_name ORDER BY sequence DESC) AS newest FROM code_scanning_uploads WHERE repo_id=$1 AND ((commit_sha=$2 AND (ref_name='refs/pull/' || CAST($3 AS TEXT) || '/head' OR ref_name=$4)) OR (ref_name='refs/pull/' || CAST($3 AS TEXT) || '/merge' AND commit_sha=ANY($5)))) statuses WHERE newest=1 ORDER BY ref_name LIMIT 3")
        .bind(selection.repo).bind(selection.head).bind(selection.number).bind(selection.branch).bind(selection.merge_commits).fetch_all(&mut *tx).await?;
    tx.commit().await?;
    Ok(snapshot)
}
