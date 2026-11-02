//! One pinned conversation comment per discussion, shared by native forms and REST.
use super::{assignees::AssignedItem, DbPool};
use anyhow::Result;
use chrono::{DateTime, Utc};
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct Pin {
    pub comment_id: i64,
    pub pinned_by: Option<i64>,
    pub pinned_at: DateTime<Utc>,
}
/// The pin of one issue or pull request in a batch read.
#[derive(sqlx::FromRow)]
pub struct ItemPin {
    pub item_id: i64,
    #[sqlx(flatten)]
    pub pin: Pin,
}
#[derive(Debug, thiserror::Error)]
pub enum PinRefusal {
    #[error("Pinning comments requires write access")]
    Denied,
    #[error("Comment not found")]
    Missing,
    #[error("Unpin the current comment before pinning another")]
    Occupied,
}
pub(crate) struct Scope {
    pub parent: &'static str,
    pub comments: &'static str,
    pub pins: &'static str,
    pub foreign: &'static str,
    pub id: i64,
    pub pull: bool,
}
pub(crate) fn scope(item: AssignedItem) -> Scope {
    match item {
        AssignedItem::Issue(id) => Scope {
            parent: "issues",
            comments: "issue_comments",
            pins: "issue_comment_pins",
            foreign: "issue_id",
            id,
            pull: false,
        },
        AssignedItem::PullRequest(id) => Scope {
            parent: "pull_requests",
            comments: "pr_comments",
            pins: "pr_comment_pins",
            foreign: "pull_request_id",
            id,
            pull: true,
        },
    }
}
native_operation!(crate::backend::db::postgres::pins::get_pin,crate::backend::db::sqlite::pins::get_pin,
    fn get_pin(pool:&DbPool,item:AssignedItem)->Result<Option<Pin>>);
native_operation!(crate::backend::db::postgres::pins::get_pins,crate::backend::db::sqlite::pins::get_pins,
    /// The pins of the issues, or of the pull requests, among `ids` (at most 500).
    fn get_pins(pool:&DbPool,pull_requests:bool,ids:&[i64])->Result<Vec<ItemPin>>);
native_operation!(crate::backend::db::postgres::pins::set_pin,crate::backend::db::sqlite::pins::set_pin,
    fn set_pin(pool:&DbPool,actor:i64,credential:&super::users::accounts::AccountCredential<'_>,repo:i64,item:AssignedItem,comment:i64,pinned:bool)->Result<()>);
