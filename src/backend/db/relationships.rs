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
            Self::BlockedBy | Self::Parent => ("e.source_id", "e.target_id"),
            Self::Related => (
                "CASE WHEN e.source_id=$2 THEN e.target_id ELSE e.source_id END",
                "CASE WHEN e.source_id=$2 THEN e.source_id ELSE e.target_id END",
            ),
            _ => ("e.target_id", "e.source_id"),
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
    pub created_at: chrono::DateTime<chrono::Utc>,
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
#[derive(Clone, Copy)]
pub enum Order {
    Position,
    CreatedAt,
    DependencyAddedAt,
}

pub struct Window<'a> {
    pub viewer: i64,
    pub root: i64,
    pub kind: Kind,
    pub credential: &'a super::repositories::access::ReadFilter,
    pub order: Order,
    pub descending: bool,
    pub offset: i64,
    pub limit: i64,
    pub count: bool,
}

pub struct RelationPage {
    pub total: Option<i64>,
    /// Lightweight issue identities, not titles or bodies.
    pub issues: Vec<(i64, i64)>,
}

pub(crate) fn window_sql(window: &Window<'_>, readable: &str, count: bool) -> String {
    let (target, source) = window.kind.selection();
    let select = if count { "COUNT(*)" } else { "i.id,i.repo_id" };
    let credential = window.credential.condition();
    let direction = if window.descending { "DESC" } else { "ASC" };
    let order = match window.order {
        Order::Position => "e.position",
        Order::CreatedAt => "i.created_at",
        Order::DependencyAddedAt => "e.created_at",
    };
    let tail = if count { String::new() } else {
        format!(" ORDER BY {order} {direction},e.source_id {direction},e.target_id {direction} LIMIT $4 OFFSET $5")
    };
    format!("SELECT {select} FROM issue_relations e JOIN issues i ON i.id={target} \
        JOIN repositories r ON r.id=i.repo_id LEFT JOIN users u ON u.id=$1 \
        WHERE {source}=$2 AND e.kind=$3 AND ({readable}) AND ({credential}) \
        AND (COALESCE(u.username,'')<>$6 OR i.repo_id=(SELECT repo_id FROM issues WHERE id=$2)) \
        AND CAST($4 AS BIGINT)>=0 AND CAST($5 AS BIGINT)>=0{tail}")
}

native_operation!(crate::backend::db::postgres::relationships::window,
    crate::backend::db::sqlite::relationships::window,
    fn window(pool: &DbPool, window: &Window<'_>) -> Result<RelationPage>);

native_operation!(crate::backend::db::postgres::relationships::list,crate::backend::db::sqlite::relationships::list,
    fn list(pool:&DbPool,viewer:Option<i64>,root:i64,kind:Kind,offset:i64,limit:i64)->Result<Vec<Link>>);
pub async fn change(pool: &DbPool, actor: i64, repo: i64, root: i64, mutation: Mutation) -> Result<()> {
    change_with_token(pool, actor, repo, root, mutation, None).await
}
native_operation!(crate::backend::db::postgres::relationships::change,crate::backend::db::sqlite::relationships::change,
    fn change_with_token(pool:&DbPool,actor:i64,repo:i64,root:i64,mutation:Mutation,token:Option<i64>)->Result<()>);
native_operation!(crate::backend::db::postgres::relationships::summaries,crate::backend::db::sqlite::relationships::summaries,
    /// The sub-issue and dependency counts of each issue in `roots` (at most 100), one row per
    /// root and repository of the related issues.
    fn summaries(pool:&DbPool,roots:&[i64])->Result<Vec<RelationCounts>>);
