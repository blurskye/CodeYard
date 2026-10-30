//! Actual accepted Git ref changes, independently of optional profile statistics or webhooks.
use crate::backend::db::DbPool;
use anyhow::Result;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    Push,
    ForcePush,
    BranchCreation,
    BranchDeletion,
    PrMerge,
    MergeQueueMerge,
}
impl Kind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Push => "push",
            Self::ForcePush => "force_push",
            Self::BranchCreation => "branch_creation",
            Self::BranchDeletion => "branch_deletion",
            Self::PrMerge => "pr_merge",
            Self::MergeQueueMerge => "merge_queue_merge",
        }
    }
}
pub struct RefActivity<'a> {
    pub(in crate::backend::db) reference: &'a str,
    pub(in crate::backend::db) before: &'a str,
    pub(in crate::backend::db) after: &'a str,
    pub(in crate::backend::db) kind: Kind,
}
impl<'a> RefActivity<'a> {
    pub fn new(reference: &'a str, before: &'a str, after: &'a str, kind: Kind) -> Result<Self> {
        anyhow::ensure!(
            reference.starts_with("refs/heads/")
                && reference.len() <= 1024
                && !reference.chars().any(char::is_control),
            "Invalid activity reference"
        );
        anyhow::ensure!(
            [before, after]
                .iter()
                .all(|sha| sha.len() == 40 && sha.bytes().all(|ch| ch.is_ascii_hexdigit())),
            "Invalid activity commit ID"
        );
        Ok(Self {
            reference,
            before,
            after,
            kind,
        })
    }
}
#[derive(Clone, Debug, sqlx::FromRow)]
pub struct Activity {
    pub id: i64,
    pub repo_id: i64,
    pub actor_id: Option<i64>,
    pub actor: Option<String>,
    pub reference: String,
    pub before_sha: String,
    pub after_sha: String,
    pub activity_type: String,
    pub created_at: DateTime<Utc>,
}
#[derive(Default)]
pub struct Filter<'a> {
    pub boundary: Option<i64>,
    pub descending: bool,
    pub reference: Option<&'a str>,
    pub actor: Option<&'a str>,
    pub kind: Option<Kind>,
    pub since: Option<DateTime<Utc>>,
}
pub(crate) fn list_sql(pg: bool, filter: &Filter<'_>) -> String {
    let direction = if filter.descending { "DESC" } else { "ASC" };
    let boundary = match (filter.boundary, filter.descending) {
        (Some(_), true) => "a.id<$2",
        (Some(_), false) => "a.id>$2",
        (None, _) => "CAST($2 AS BIGINT) IS NULL",
    };
    let reference = if filter.reference.is_some() { "a.reference=$3" } else { "CAST($3 AS TEXT) IS NULL" };
    let actor = if filter.actor.is_some() { "lower(u.username)=lower($4)" } else { "CAST($4 AS TEXT) IS NULL" };
    let kind = if filter.kind.is_some() { "a.activity_type=$5" } else { "CAST($5 AS TEXT) IS NULL" };
    let since = match (filter.since.is_some(), pg) {
        (true, _) => "a.created_at>=$6",
        (false, true) => "CAST($6 AS TIMESTAMPTZ) IS NULL",
        (false, false) => "CAST($6 AS TEXT) IS NULL",
    };
    format!("SELECT a.*,u.username AS actor FROM repository_ref_activities a LEFT JOIN users u ON u.id=a.actor_id WHERE a.repo_id=$1 AND {boundary} AND {reference} AND {actor} AND {kind} AND {since} ORDER BY a.id {direction} LIMIT $7")
}

native_operation!(crate::backend::db::postgres::repositories::activity::record,crate::backend::db::sqlite::repositories::activity::record,
    fn record(pool:&DbPool,repo_id:i64,actor:i64,operation:&Uuid,changes:&[RefActivity<'_>])->Result<()>);
native_operation!(crate::backend::db::postgres::repositories::activity::list,crate::backend::db::sqlite::repositories::activity::list,
    fn list(pool:&DbPool,repo_id:i64,filter:&Filter<'_>,limit:i64)->Result<Vec<Activity>>);
