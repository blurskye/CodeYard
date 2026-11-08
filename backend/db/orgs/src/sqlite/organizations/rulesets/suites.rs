use super::*;
use codeyard_database::queries::organizations::OrganizationOutcome;
use codeyard_database::queries::rulesets::records::StoredSuite;
use codeyard_database::queries::rulesets::records::Suite;
use codeyard_database::queries::rulesets::records::SuiteEnforcement;
use codeyard_database::queries::rulesets::records::SuiteFilter;
use codeyard_database::queries::rulesets::records::SuiteResult;
use codeyard_database::queries::sqlite::organizations::authority::begin_read;
use codeyard_database::require_read_org_sqlite;

pub async fn list_suites(
    pool: &SqlitePool,
    actor: i64,
    org: i64,
    filter: &SuiteFilter,
    page: Page,
) -> Result<OrganizationOutcome<Vec<Suite>>> {
    let mut tx = begin_read(pool).await?;
    require_read_org_sqlite!(tx, actor, org);
    let readable = codeyard_database::queries::sqlite::repositories::access::readable_condition();
    let reference = filter.reference.as_deref();
    let short_ref = reference.filter(|r| !r.starts_with("refs/"));
    let enforcement = enforcement(filter);
    let columns = StoredSuite::SUMMARY_COLUMNS;
    let rows:Vec<StoredSuite>=sqlx::query_as(&format!("SELECT {columns} FROM repository_rule_suites s JOIN repositories r ON r.id=s.repo_id JOIN users u ON u.id=?2 WHERE s.org_id=?1 AND r.owner_kind='org' AND r.owner_org_id=?1 AND r.owner_user_id IS NULL AND {readable} AND s.created_at>=?3 AND (?4 IS NULL OR s.ref_name=?4 OR s.ref_name='refs/heads/'||?5 OR s.ref_name='refs/tags/'||?5) AND (?6 IS NULL OR s.repository_name=?6 COLLATE NOCASE) AND (?7 IS NULL OR s.actor_name=?7 COLLATE NOCASE) AND (?8 IS NULL OR s.result=?8) AND (?9 IS NULL OR EXISTS(SELECT 1 FROM json_each(s.evaluations) e WHERE json_extract(e.value,'$.enforcement')=?9)) ORDER BY s.id DESC LIMIT ?10 OFFSET ?11"))
        .bind(org).bind(actor).bind(filter.since).bind(reference).bind(short_ref)
        .bind(&filter.repository_name).bind(&filter.actor_name).bind(filter.result.map(|r|r.as_str())).bind(enforcement)
        .bind(page.limit).bind(page.offset).fetch_all(&mut *tx).await?;
    let rows = rows
        .into_iter()
        .map(StoredSuite::decode)
        .collect::<Result<Vec<_>>>()?;
    tx.commit().await?;
    Ok(Applied(rows))
}
pub async fn get_suite(
    pool: &SqlitePool,
    actor: i64,
    org: i64,
    id: i64,
) -> Result<OrganizationOutcome<Suite>> {
    let mut tx = begin_read(pool).await?;
    require_read_org_sqlite!(tx, actor, org);
    let readable = codeyard_database::queries::sqlite::repositories::access::readable_condition();
    let row:Option<StoredSuite>=sqlx::query_as(&format!("SELECT s.* FROM repository_rule_suites s JOIN repositories r ON r.id=s.repo_id JOIN users u ON u.id=?2 WHERE s.org_id=?1 AND s.id=?3 AND r.owner_kind='org' AND r.owner_org_id=?1 AND r.owner_user_id IS NULL AND {readable}")).bind(org).bind(actor).bind(id).fetch_optional(&mut *tx).await?;
    let Some(row) = row else { return Ok(Missing) };
    let row = row.decode()?;
    tx.commit().await?;
    Ok(Applied(row))
}
fn enforcement(filter: &SuiteFilter) -> Option<&'static str> {
    match filter.enforcement {
        Some(SuiteEnforcement::Active) => Some("active"),
        Some(SuiteEnforcement::Evaluate) => Some("evaluate"),
        Some(SuiteEnforcement::DeletedRuleset) => Some("deleted ruleset"),
        None => None,
    }
}
/// Records one push evaluation. The suite's `id` and `created_at` are assigned here.
pub async fn record_suite(pool: &SqlitePool, suite: &Suite) -> Result<i64> {
    let evaluations = serde_json::to_string(&suite.evaluations)?;
    Ok(sqlx::query_scalar::<_, i64>("INSERT INTO repository_rule_suites(org_id,repo_id,repository_name,actor_id,actor_name,ref_name,before_sha,after_sha,result,evaluation_result,evaluations) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11) RETURNING id")
        .bind(suite.org_id)
        .bind(suite.repo_id)
        .bind(&suite.repository_name)
        .bind(suite.actor_id)
        .bind(&suite.actor_name)
        .bind(&suite.reference)
        .bind(&suite.before_sha)
        .bind(&suite.after_sha)
        .bind(suite.result.as_str())
        .bind(suite.evaluation_result.map(SuiteResult::as_str))
        .bind(evaluations)
        .fetch_one(pool)
        .await?)
}
/// Repository rule suites are for the repository's administrators, as on GitHub.
pub async fn list_repository_suites(
    pool: &SqlitePool,
    actor: i64,
    repo: i64,
    filter: &SuiteFilter,
    page: Page,
) -> Result<OrganizationOutcome<Vec<Suite>>> {
    let mut tx = begin_read(pool).await?;
    match access::authorize(&mut tx, Some(actor), Owner::Repository(repo), false).await? {
        Applied(true) => {}
        Missing => return Ok(Missing),
        _ => return Ok(Denied),
    }
    let reference = filter.reference.as_deref();
    let short_ref = reference.filter(|r| !r.starts_with("refs/"));
    let rows: Vec<StoredSuite> = sqlx::query_as(&format!("SELECT {} FROM repository_rule_suites s WHERE s.repo_id=?1 AND s.created_at>=?2 AND (?3 IS NULL OR s.ref_name=?3 OR s.ref_name='refs/heads/'||?4 OR s.ref_name='refs/tags/'||?4) AND (?5 IS NULL OR s.actor_name=?5 COLLATE NOCASE) AND (?6 IS NULL OR s.result=?6) AND (?7 IS NULL OR EXISTS(SELECT 1 FROM json_each(s.evaluations) e WHERE json_extract(e.value,'$.enforcement')=?7)) ORDER BY s.id DESC LIMIT ?8 OFFSET ?9", StoredSuite::SUMMARY_COLUMNS))
        .bind(repo)
        .bind(filter.since)
        .bind(reference)
        .bind(short_ref)
        .bind(&filter.actor_name)
        .bind(filter.result.map(|r| r.as_str()))
        .bind(enforcement(filter))
        .bind(page.limit)
        .bind(page.offset)
        .fetch_all(&mut *tx)
        .await?;
    let rows = rows
        .into_iter()
        .map(StoredSuite::decode)
        .collect::<Result<Vec<_>>>()?;
    tx.commit().await?;
    Ok(Applied(rows))
}
pub async fn get_repository_suite(
    pool: &SqlitePool,
    actor: i64,
    repo: i64,
    id: i64,
) -> Result<OrganizationOutcome<Suite>> {
    let mut tx = begin_read(pool).await?;
    match access::authorize(&mut tx, Some(actor), Owner::Repository(repo), false).await? {
        Applied(true) => {}
        Missing => return Ok(Missing),
        _ => return Ok(Denied),
    }
    let row: Option<StoredSuite> =
        sqlx::query_as("SELECT * FROM repository_rule_suites WHERE repo_id=?1 AND id=?2")
            .bind(repo)
            .bind(id)
            .fetch_optional(&mut *tx)
            .await?;
    let Some(row) = row else { return Ok(Missing) };
    let row = row.decode()?;
    tx.commit().await?;
    Ok(Applied(row))
}
