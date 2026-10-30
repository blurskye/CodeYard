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
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Order {
    Position,
    CreatedAt,
    DependencyAddedAt,
}
/// One page of the issues related to `root` that `viewer` may read, optionally limited to some
/// repositories.
#[derive(Clone, Copy)]
pub struct Window<'a> {
    pub viewer: Option<i64>,
    pub root: i64,
    pub kind: Kind,
    pub repositories: Option<&'a [i64]>,
    pub order: Order,
    pub descending: bool,
    pub offset: i64,
    pub limit: i64,
}
/// The related issues of `$2` that viewer `$1` may read. `$3` is the edge kind, `$4` the CI bot
/// name, and `repositories` may refer to `$5`.
pub(crate) fn related_sql(kind: Kind, readable: &str, repositories: &str) -> String {
    let (target, source) = kind.selection();
    format!("FROM issue_relations e JOIN issues i ON i.id={target} JOIN repositories r ON r.id=i.repo_id LEFT JOIN users u ON u.id=$1 LEFT JOIN users owners ON r.owner_kind='user' AND owners.id=r.owner_user_id LEFT JOIN organizations orgs ON r.owner_kind='org' AND orgs.id=r.owner_org_id WHERE {source}=$2 AND e.kind=$3 AND {readable} AND ({repositories}) AND (COALESCE(u.username,'')<>$4 OR i.repo_id=(SELECT repo_id FROM issues WHERE id=$2))")
}
/// The page query: `$6` is the limit and `$7` the offset. Issue IDs break ties, so offsets are
/// stable.
pub(crate) fn window_sql(page: &Window<'_>, readable: &str, repositories: &str) -> String {
    let direction = if page.descending { "DESC" } else { "ASC" };
    let order = match page.order {
        Order::Position => "e.position",
        Order::CreatedAt => "i.created_at",
        Order::DependencyAddedAt => "e.created_at",
    };
    format!("SELECT i.id,i.repo_id,i.number,i.title,i.state,i.created_at,COALESCE(owners.username,orgs.name) AS owner,r.name AS repo {} ORDER BY {order} {direction},i.id {direction} LIMIT $6 OFFSET $7", related_sql(page.kind, readable, repositories))
}
native_operation!(crate::backend::db::postgres::relationships::window,crate::backend::db::sqlite::relationships::window,
    fn window(pool:&DbPool,page:&Window<'_>)->Result<Vec<Link>>);
native_operation!(crate::backend::db::postgres::relationships::repository_counts,crate::backend::db::sqlite::relationships::repository_counts,
    /// How many readable related issues each repository holds, so callers that filter whole
    /// repositories can count and page without loading the issues.
    fn repository_counts(pool:&DbPool,viewer:Option<i64>,root:i64,kind:Kind)->Result<Vec<(i64,i64)>>);
/// The related issues in edge position order.
pub async fn list(
    pool: &DbPool,
    viewer: Option<i64>,
    root: i64,
    kind: Kind,
    offset: i64,
    limit: i64,
) -> Result<Vec<Link>> {
    window(
        pool,
        &Window {
            viewer,
            root,
            kind,
            repositories: None,
            order: Order::Position,
            descending: false,
            offset,
            limit,
        },
    )
    .await
}
pub async fn change(pool: &DbPool, actor: i64, repo: i64, root: i64, mutation: Mutation) -> Result<()> {
    change_with_token(pool, actor, repo, root, mutation, None).await
}
native_operation!(crate::backend::db::postgres::relationships::change,crate::backend::db::sqlite::relationships::change,
    fn change_with_token(pool:&DbPool,actor:i64,repo:i64,root:i64,mutation:Mutation,token:Option<i64>)->Result<()>);
native_operation!(crate::backend::db::postgres::relationships::summaries,crate::backend::db::sqlite::relationships::summaries,
    /// The sub-issue and dependency counts of each issue in `roots` (at most 100), one row per
    /// root and repository of the related issues.
    fn summaries(pool:&DbPool,roots:&[i64])->Result<Vec<RelationCounts>>);
