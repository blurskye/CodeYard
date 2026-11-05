use anyhow::Result;
use chrono::DateTime;
use chrono::Utc;
use codeyard_db_core::native_operation;
use codeyard_db_core::DbPool;

pub const QUERY_LIMIT: i32 = 5000;

#[derive(sqlx::FromRow)]
pub struct RateLimit {
    pub used: i32,
    pub reset_at: DateTime<Utc>,
}

/// Resolve only the owning repository; the API then applies live access and the family's scope
/// before calling the same record readers as REST. Prefixes never supply SQL identifiers.
pub(crate) fn repository_owner_sql(kind: &str) -> Option<&'static str> {
    Some(match kind {
        "I" => "SELECT repo_id FROM issues WHERE id=$1",
        "PR" => "SELECT repo_id FROM pull_requests WHERE id=$1",
        "CC" => "SELECT repo_id FROM commit_comments WHERE id=$1",
        "M" => "SELECT repo_id FROM milestones WHERE id=$1",
        "REA" => "SELECT repo_id FROM reactions WHERE id=$1",
        "CS" => "SELECT repo_id FROM commit_status_history WHERE id=$1",
        "CRU" => "SELECT repo_id FROM check_runs WHERE id=$1",
        "CSU" => "SELECT repo_id FROM check_suites WHERE id=$1",
        "RE" => "SELECT repo_id FROM releases WHERE id=$1",
        "RA" => "SELECT r.repo_id FROM release_assets a JOIN releases r ON r.id=a.release_id WHERE a.id=$1",
        "PRR" => "SELECT p.repo_id FROM pr_reviews e JOIN pull_requests p ON p.id=e.pull_request_id WHERE e.id=$1",
        "PRRC" => "SELECT p.repo_id FROM pr_comments e JOIN pull_requests p ON p.id=e.pull_request_id WHERE e.id=$1",
        "IE" => "SELECT repo_id FROM timeline_events WHERE id=$1",
        "S" => "SELECT s.repo_id FROM pull_request_stacks s WHERE s.id=$1 AND EXISTS(SELECT 1 FROM pull_requests p WHERE p.repo_id=s.repo_id AND p.stack_id=s.id)",
        "ART" => "SELECT repo_id FROM actions_objects WHERE id=$1 AND kind='artifact' AND finalized AND expires_at>$2",
        _ => return None,
    })
}

/// Only ownership is resolved here; the operation applies its live resource policy.
/// Type names select fixed SQL, never an identifier supplied by a client.
pub(crate) fn node_owner_sql(kind: &str) -> Option<&'static str> {
    if let Some(sql) = repository_owner_sql(kind) {
        return Some(sql);
    }
    Some(match kind {
        "R" => "SELECT id FROM repositories WHERE id=$1",
        "U" => "SELECT id FROM users WHERE id=$1",
        "O" => "SELECT id FROM organizations WHERE id=$1",
        "T" => "SELECT org_id FROM teams WHERE id=$1",
        "IC" => "SELECT i.repo_id FROM issue_comments c JOIN issues i ON i.id=c.issue_id WHERE c.id=$1",
        "PC" => "SELECT p.repo_id FROM pr_comments c JOIN pull_requests p ON p.id=c.pull_request_id WHERE c.id=$1",
        "L" => "SELECT repo_id FROM labels WHERE id=$1",
        "D" => "SELECT repo_id FROM discussions WHERE id=$1",
        "DIC" => "SELECT repo_id FROM discussion_categories WHERE id=$1",
        "LB" => "SELECT repo_id FROM issue_linked_branches WHERE id=$1",
        "IT" => "SELECT COALESCE(owner_user_id,owner_org_id) FROM issue_attributes WHERE id=$1 AND kind='type'",
        "RRS" => "SELECT COALESCE(owner_repo_id,owner_org_id) FROM repository_rulesets WHERE id=$1",
        "PVT" => "SELECT owner_id FROM projects WHERE id=$1 AND deleted_at IS NULL",
        "PVTF" | "PVTSSF" | "PVTIF" => "SELECT p.owner_id FROM project_fields f JOIN projects p ON p.id=f.project_id WHERE f.id=$1 AND p.deleted_at IS NULL",
        "PVTI" | "DI" => "SELECT p.owner_id FROM project_items i JOIN projects p ON p.id=i.project_id WHERE i.id=$1 AND p.deleted_at IS NULL",
        _ => return None,
    })
}

/// A transfer keeps prior issue roots valid through the existing, durable URL redirects.
/// They are routing hints only; readers and writers still authorize the current repository.
pub(crate) fn node_root_sql(kind: &str) -> Option<String> {
    let current = node_owner_sql(kind)?.replace("$2", "$3");
    let issue = match kind {
        "I" => Some("SELECT id FROM issues WHERE id=$1"),
        "IC" => Some("SELECT issue_id FROM issue_comments WHERE id=$1"),
        "IE" => Some("SELECT subject_id FROM timeline_events WHERE id=$1 AND subject_kind='issue'"),
        "REA" => Some("SELECT CASE WHEN r.subject_kind='issue' THEN r.subject_id ELSE c.issue_id END FROM reactions r LEFT JOIN issue_comments c ON r.subject_kind='issue_comment' AND c.id=r.subject_id WHERE r.id=$1 AND r.subject_kind IN ('issue','issue_comment')"),
        _ => None,
    };
    let historical = issue.map_or_else(String::new, |issue| format!(
        " OR EXISTS(SELECT 1 FROM owner JOIN issue_redirects d ON d.issue_id=({issue}) WHERE d.repo_id=$2)"
    ));
    Some(format!("WITH owner(root) AS ({current}) SELECT EXISTS(SELECT 1 FROM owner WHERE root=$2){historical}"))
}

native_operation!(crate::postgres::graphql::node_root_matches,
    crate::sqlite::graphql::node_root_matches,
    fn node_root_matches(pool: &DbPool, kind: &str, id: i64, root: i64) -> Result<bool>);
native_operation!(crate::postgres::graphql::node_owner,
    crate::sqlite::graphql::node_owner,
    fn node_owner(pool: &DbPool, kind: &str, id: i64) -> Result<Option<i64>>);
native_operation!(crate::postgres::graphql::linked_branch, crate::sqlite::graphql::linked_branch,
    fn linked_branch(pool: &DbPool, id: i64) -> Result<Option<crate::issues::management::LinkedBranch>>);
native_operation!(crate::postgres::graphql::ruleset_owner, crate::sqlite::graphql::ruleset_owner,
    fn ruleset_owner(pool: &DbPool, id: i64) -> Result<Option<(Option<i64>, Option<i64>)>>);
native_operation!(crate::postgres::graphql::issue_type_repositories, crate::sqlite::graphql::issue_type_repositories,
    fn issue_type_repositories(pool: &DbPool, id: i64, after: i64, viewer: i64) -> Result<Vec<i64>>);
native_operation!(crate::postgres::graphql::team_locator,
    crate::sqlite::graphql::team_locator,
    fn team_locator(pool:&DbPool,id:i64)->Result<Option<(i64,String)>>);
native_operation!(crate::postgres::graphql::reaction,
    crate::sqlite::graphql::reaction,
    fn reaction(pool: &DbPool, id: i64) -> Result<Option<crate::reactions::Reaction>>);
native_operation!(crate::postgres::graphql::status,
    crate::sqlite::graphql::status,
    fn status(pool: &DbPool, id: i64) -> Result<Option<codeyard_db_core::models::CommitStatus>>);

native_operation!(crate::postgres::graphql::admit_query,
    crate::sqlite::graphql::admit_query,
    fn admit_query(pool: &DbPool, token: i64) -> Result<Option<RateLimit>>);
