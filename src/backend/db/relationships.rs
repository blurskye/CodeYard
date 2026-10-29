//! Issue-only relationship graphs. Native writes serialize graph validation with the change.
use super::DbPool;
use anyhow::Result;
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    BlockedBy,
    Blocking,
    Related,
    Children,
    Parent,
}
impl Kind {
    pub fn parse(value: &str) -> Option<Self> {
        Some(match value {
            "blocked_by" => Self::BlockedBy,
            "blocking" => Self::Blocking,
            "related" => Self::Related,
            "children" => Self::Children,
            "parent" => Self::Parent,
            _ => return None,
        })
    }
    pub fn name(self) -> &'static str {
        match self {
            Self::BlockedBy => "blocked_by",
            Self::Blocking => "blocking",
            Self::Related => "related",
            Self::Children => "children",
            Self::Parent => "parent",
        }
    }
    pub(crate) fn storage(self) -> &'static str {
        match self {
            Self::BlockedBy | Self::Blocking => "blocks",
            Self::Related => "related",
            Self::Children | Self::Parent => "parent",
        }
    }
    pub(crate) fn edge(self, root: i64, other: i64) -> (i64, i64) {
        match self {
            Self::BlockedBy | Self::Parent => (other, root),
            Self::Related => (root.min(other), root.max(other)),
            _ => (root, other),
        }
    }
    pub(crate) fn selection(self) -> (&'static str, &'static str) {
        match self {
            Self::BlockedBy | Self::Parent => ("e.source_id", "e.target_id=$2"),
            Self::Related => (
                "CASE WHEN e.source_id=$2 THEN e.target_id ELSE e.source_id END",
                "(e.source_id=$2 OR e.target_id=$2)",
            ),
            _ => ("e.target_id", "e.source_id=$2"),
        }
    }
}
#[derive(Clone, Copy)]
pub enum Mutation {
    Add {
        kind: Kind,
        other: i64,
        replace: bool,
    },
    Remove {
        kind: Kind,
        other: i64,
    },
    Move {
        child: i64,
        before: Option<i64>,
        after: Option<i64>,
    },
}
impl Mutation {
    pub(crate) fn target(self) -> i64 {
        match self {
            Self::Add { other, .. } | Self::Remove { other, .. } => other,
            Self::Move { child, .. } => child,
        }
    }
}
#[derive(Debug, thiserror::Error)]
pub enum Refusal {
    #[error("Relationship mutation denied")]
    Denied,
    #[error("Issue or relationship not found")]
    Missing,
    #[error("Invalid issue relationship")]
    Invalid,
    #[error("Relationship would form a cycle")]
    Cycle,
    #[error("The sub-issue already has a parent")]
    HasParent,
    #[error("Issue relationship limit exceeded")]
    Limit,
}
#[derive(sqlx::FromRow)]
pub struct Link {
    pub id: i64,
    pub repo_id: i64,
    pub number: i32,
    pub title: String,
    pub state: String,
    pub owner: String,
    pub repo: String,
}
/// The relationship counts of one issue from its related issues in one repository. Readers add up
/// the rows of the repositories they may read.
#[derive(sqlx::FromRow)]
pub struct RelationCounts {
    pub root: i64,
    pub repo_id: i64,
    pub total: i64,
    pub completed: i64,
    pub blocked_by: i64,
    pub blocking: i64,
    pub total_blocked_by: i64,
    pub total_blocking: i64,
}
native_operation!(crate::backend::db::postgres::relationships::list,crate::backend::db::sqlite::relationships::list,
    fn list(pool:&DbPool,viewer:Option<i64>,root:i64,kind:Kind,offset:i64,limit:i64)->Result<Vec<Link>>);
native_operation!(crate::backend::db::postgres::relationships::change,crate::backend::db::sqlite::relationships::change,
    fn change(pool:&DbPool,actor:i64,repo:i64,root:i64,mutation:Mutation)->Result<()>);
native_operation!(crate::backend::db::postgres::relationships::summaries,crate::backend::db::sqlite::relationships::summaries,
    /// The sub-issue and dependency counts of each issue in `roots` (at most 100), one row per
    /// root and repository of the related issues.
    fn summaries(pool:&DbPool,roots:&[i64])->Result<Vec<RelationCounts>>);
