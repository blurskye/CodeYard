//! GraphQL admission is shared by all web nodes using the database.
use super::DbPool;
use anyhow::Result;
use chrono::{DateTime, Utc};

#[derive(sqlx::FromRow)]
pub struct RateLimit {
    pub used: i32,
    pub reset_at: DateTime<Utc>,
}

/// Resolve only the owning repository; the API then applies live access and the family's scope
/// before calling the same record readers as REST. Prefixes never supply SQL identifiers.
pub(crate) fn node_repository_sql(kind: &str) -> Option<&'static str> {
    Some(match kind {
        "CC" => "SELECT repo_id FROM commit_comments WHERE id=$1",
        "M" => "SELECT repo_id FROM milestones WHERE id=$1",
        "REA" => "SELECT repo_id FROM reactions WHERE id=$1",
        "CS" => "SELECT repo_id FROM commit_status_history WHERE id=$1",
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

native_operation!(crate::backend::db::postgres::graphql::node_repository,
    crate::backend::db::sqlite::graphql::node_repository,
    fn node_repository(pool: &DbPool, kind: &str, id: i64) -> Result<Option<i64>>);
native_operation!(crate::backend::db::postgres::graphql::team_locator,
    crate::backend::db::sqlite::graphql::team_locator,
    fn team_locator(pool:&DbPool,id:i64)->Result<Option<(i64,String)>>);
native_operation!(crate::backend::db::postgres::graphql::reaction,
    crate::backend::db::sqlite::graphql::reaction,
    fn reaction(pool: &DbPool, id: i64) -> Result<Option<super::reactions::Reaction>>);
native_operation!(crate::backend::db::postgres::graphql::status,
    crate::backend::db::sqlite::graphql::status,
    fn status(pool: &DbPool, id: i64) -> Result<Option<super::models::CommitStatus>>);

native_operation!(crate::backend::db::postgres::graphql::admit_query,
    crate::backend::db::sqlite::graphql::admit_query,
    fn admit_query(pool: &DbPool, token: i64) -> Result<Option<RateLimit>>);
