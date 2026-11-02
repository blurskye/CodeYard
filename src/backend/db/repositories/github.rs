//! GitHub repository lists apply visibility and filters before their page limit.
use crate::backend::db::{models::Repository, DbPool};
use anyhow::Result;

/// Teams with an explicit grant, each reporting the permission it also inherits from parents.
pub(crate) const REPOSITORY_TEAMS_SQL:&str="SELECT t.*,e.permission FROM team_repo_access a JOIN effective_team_repo_access e ON e.team_id=a.team_id AND e.repo_id=a.repo_id JOIN teams t ON t.id=a.team_id WHERE a.repo_id=$1 AND t.org_id=$2 AND a.permission BETWEEN 1 AND 5 AND (t.privacy='visible' OR (t.privacy='secret' AND ($3 OR EXISTS(SELECT 1 FROM team_memberships m WHERE m.team_id=t.id AND m.user_id=$4 AND m.role IN ('member','maintainer'))))) ORDER BY t.id LIMIT $5 OFFSET $6";

#[derive(sqlx::FromRow)]
pub struct RepositoryTeam {
    #[sqlx(flatten)]
    pub team: crate::backend::db::models::Team,
    pub permission: i16,
}

native_operation!(crate::backend::db::postgres::repositories::github::repository_teams,
    crate::backend::db::sqlite::repositories::github::repository_teams,
    fn repository_teams(pool:&DbPool,actor:i64,repo_id:i64,limit:i64,offset:i64) -> Result<Option<Vec<RepositoryTeam>>>);

#[derive(Clone, Copy)]
pub enum RepositoryList {
    Public,
    User(i64),
    OwnedUser(i64),
    Organization(i64),
    Accessible {
        owner: bool,
        collaborator: bool,
        organization: bool,
    },
    Forks(i64),
}

#[derive(Clone, Copy)]
pub struct RepositoryListQuery<'a> {
    pub viewer: Option<i64>,
    pub list: RepositoryList,
    pub since: i64,
    pub visibility: &'a str,
    pub kind: &'a str,
    pub sort: &'a str,
    pub descending: bool,
    pub limit: i64,
    pub offset: i64,
}

pub(crate) fn list_sql(query: &RepositoryListQuery<'_>, access: &str, readable: &str) -> String {
    let scope = match query.list {
        RepositoryList::Public => "NOT r.is_private AND r.id > $3 AND CAST($2 AS BIGINT) IS NULL".to_owned(),
        // GitHub's user profile endpoint only lists public repositories, even for the owner.
        RepositoryList::User(_) => "r.owner_kind='user' AND r.owner_user_id=$2 AND NOT r.is_private".to_owned(),
        RepositoryList::OwnedUser(_) => "r.owner_kind='user' AND r.owner_user_id=$2".to_owned(),
        RepositoryList::Organization(_) => "r.owner_kind='org' AND r.owner_org_id=$2".to_owned(),
        RepositoryList::Forks(_) => "r.forked_from_id=$2".to_owned(),
        RepositoryList::Accessible { owner, collaborator, organization } => format!(
            "CAST($2 AS BIGINT) IS NULL AND (({owner} AND r.owner_kind='user' AND r.owner_user_id=u.id) OR \
             ({collaborator} AND EXISTS(SELECT 1 FROM repo_access a WHERE a.repo_id=r.id AND a.user_id=u.id AND a.access_level BETWEEN 1 AND 5) AND ({access})>=1) OR \
             ({organization} AND r.owner_kind='org' AND EXISTS(SELECT 1 FROM org_memberships m WHERE m.org_id=r.owner_org_id AND m.user_id=u.id) AND ({access})>=1))"),
    };
    let order = match query.sort {
        "created" => "r.created_at",
        "updated" => "r.updated_at",
        "pushed" => "COALESCE((SELECT MAX(p.created_at) FROM repository_pushes p WHERE p.repo_id=r.id), r.created_at)",
        "stars" => "r.stars_count",
        "watchers" => "r.watchers_count",
        "id" => "r.id",
        _ => "LOWER(COALESCE(owners.username, organizations.name) || '/' || r.name)",
    };
    let direction = if query.descending { "DESC" } else { "ASC" };
    let owner_type = if matches!(query.list, RepositoryList::User(_)) {
        "($5='owner') OR"
    } else {
        ""
    };
    format!(
        "SELECT r.* FROM repositories r LEFT JOIN users u ON u.id=$1 \
        LEFT JOIN users owners ON r.owner_kind='user' AND owners.id=r.owner_user_id \
        LEFT JOIN organizations ON r.owner_kind='org' AND organizations.id=r.owner_org_id \
        WHERE {readable} AND ({scope}) AND CAST($3 AS BIGINT) >= 0 \
        AND ($4='all' OR ($4='public' AND NOT r.is_private) OR ($4='private' AND r.is_private)) \
        AND ({owner_type} $5='all' OR ($5='forks' AND r.is_fork) OR ($5='sources' AND NOT r.is_fork) \
            OR ($5='member' AND NOT (r.owner_kind='user' AND r.owner_user_id=u.id)) \
            OR ($5='owner' AND r.owner_kind='user' AND r.owner_user_id=u.id) \
            OR ($5='public' AND NOT r.is_private) OR ($5='private' AND r.is_private)) \
        ORDER BY {order} {direction}, r.id {direction} LIMIT $6 OFFSET $7"
    )
}

impl RepositoryListQuery<'_> {
    pub(crate) fn target_id(&self) -> Option<i64> {
        match self.list {
            RepositoryList::User(id)
            | RepositoryList::OwnedUser(id)
            | RepositoryList::Organization(id)
            | RepositoryList::Forks(id) => Some(id),
            _ => None,
        }
    }
}

native_operation!(crate::backend::db::postgres::repositories::github::repository_pushed_at,
    crate::backend::db::sqlite::repositories::github::repository_pushed_at,
    fn repository_pushed_at(pool: &DbPool, repo_id: i64) -> Result<chrono::DateTime<chrono::Utc>>);

native_operation!(crate::backend::db::postgres::repositories::github::count_repositories,
    crate::backend::db::sqlite::repositories::github::count_repositories,
    fn count_repositories(pool: &DbPool, query: RepositoryListQuery<'_>) -> Result<i64>);

native_operation!(crate::backend::db::postgres::repositories::github::list_repositories,
    crate::backend::db::sqlite::repositories::github::list_repositories,
    fn list_repositories(pool: &DbPool, query: RepositoryListQuery<'_>) -> Result<Vec<Repository>>);
