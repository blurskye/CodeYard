use anyhow::Result;
use codeyard_database::models::Repository;
use codeyard_database::native_operation;
use codeyard_database::DbPool;

/// Teams that reach the repository, each with the permission it holds including what it inherits
/// from parents. A public repository lists only teams with an explicit grant, as GitHub does;
/// a private one also lists child teams that hold access only through a parent.
pub(crate) const REPOSITORY_TEAMS_SQL:&str="SELECT t.*,e.permission FROM effective_team_repo_access e JOIN teams t ON t.id=e.team_id JOIN repositories r ON r.id=e.repo_id WHERE e.repo_id=$1 AND t.org_id=$2 AND (r.is_private OR EXISTS(SELECT 1 FROM team_repo_access a WHERE a.team_id=e.team_id AND a.repo_id=e.repo_id AND a.permission BETWEEN 1 AND 5)) AND (t.privacy='visible' OR (t.privacy='secret' AND ($3 OR EXISTS(SELECT 1 FROM team_memberships m WHERE m.team_id=t.id AND m.user_id=$4 AND m.role IN ('member','maintainer'))))) ORDER BY t.id LIMIT $5 OFFSET $6";

#[derive(sqlx::FromRow)]
pub struct RepositoryTeam {
    #[sqlx(flatten)]
    pub team: codeyard_database::models::Team,
    pub permission: i16,
    #[sqlx(skip)]
    pub parent: Option<codeyard_database::models::Team>,
}

native_operation!(crate::postgres::repositories::github::repository_teams,
    crate::sqlite::repositories::github::repository_teams,
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

/// The repositories the viewer may see that match `query`, as the part of a statement after
/// `FROM repositories r LEFT JOIN users u ON u.id=$1`. It binds $1..$5 only, so a count can
/// reuse it without the ordering and paging parameters.
fn conditions(query: &RepositoryListQuery<'_>, access: &str, readable: &str) -> String {
    let scope = match query.list {
        RepositoryList::Public => "NOT r.is_private AND r.id > $3 AND CAST($2 AS BIGINT) IS NULL".to_owned(),
        // GitHub's user profile endpoint only lists public repositories, even for the owner.
        // The profile's own repositories are the owner type; its collaborations are the member
        // type, which type=all adds.
        RepositoryList::User(_) => {
            let collaborations = if matches!(query.kind, "member" | "all") {
                " OR EXISTS(SELECT 1 FROM repo_access a WHERE a.repo_id=r.id AND a.user_id=$2 AND a.access_level BETWEEN 1 AND 5)"
            } else {
                ""
            };
            format!("NOT r.is_private AND ((r.owner_kind='user' AND r.owner_user_id=$2){collaborations})")
        }
        RepositoryList::OwnedUser(_) => "r.owner_kind='user' AND r.owner_user_id=$2".to_owned(),
        RepositoryList::Organization(_) => "r.owner_kind='org' AND r.owner_org_id=$2".to_owned(),
        RepositoryList::Forks(_) => "r.forked_from_id=$2".to_owned(),
        RepositoryList::Accessible { owner, collaborator, organization } => format!(
            "CAST($2 AS BIGINT) IS NULL AND (({owner} AND r.owner_kind='user' AND r.owner_user_id=u.id) OR \
             ({collaborator} AND EXISTS(SELECT 1 FROM repo_access a WHERE a.repo_id=r.id AND a.user_id=u.id AND a.access_level BETWEEN 1 AND 5) AND ({access})>=1) OR \
             ({organization} AND r.owner_kind='org' AND EXISTS(SELECT 1 FROM org_memberships m WHERE m.org_id=r.owner_org_id AND m.user_id=u.id) AND ({access})>=1))"),
    };
    // Whose repository "owner" means: the profile's user, otherwise the viewer.
    let owned = if matches!(query.list, RepositoryList::User(_)) {
        "(r.owner_kind='user' AND r.owner_user_id=$2)"
    } else {
        "(r.owner_kind='user' AND r.owner_user_id=u.id)"
    };
    format!(
        "{readable} AND ({scope}) AND CAST($3 AS BIGINT) >= 0 \
        AND ($4='all' OR ($4='public' AND NOT r.is_private) OR ($4='private' AND r.is_private)) \
        AND ($5='all' OR ($5='forks' AND r.is_fork) OR ($5='sources' AND NOT r.is_fork) \
            OR ($5='member' AND NOT {owned}) \
            OR ($5='owner' AND {owned}) \
            OR ($5='public' AND NOT r.is_private) OR ($5='private' AND r.is_private))"
    )
}

/// One ordered page: binds $1..$5 as `conditions` does, then the limit $6 and offset $7.
pub(crate) fn list_sql(query: &RepositoryListQuery<'_>, access: &str, readable: &str) -> String {
    let order = match query.sort {
        "created" => "r.created_at",
        "updated" => "r.updated_at",
        "pushed" => "COALESCE((SELECT MAX(p.created_at) FROM repository_pushes p WHERE p.repo_id=r.id), r.created_at)",
        "stars" => "r.stars_count",
        "watchers" => "r.watchers_count",
        "id" => "r.id",
        "name" => "LOWER(r.name)",
        _ => "LOWER(COALESCE(owners.username, organizations.name) || '/' || r.name)",
    };
    let direction = if query.descending { "DESC" } else { "ASC" };
    let conditions = conditions(query, access, readable);
    format!(
        "SELECT r.* FROM repositories r LEFT JOIN users u ON u.id=$1 \
        LEFT JOIN users owners ON r.owner_kind='user' AND owners.id=r.owner_user_id \
        LEFT JOIN organizations ON r.owner_kind='org' AND organizations.id=r.owner_org_id \
        WHERE {conditions} \
        ORDER BY {order} {direction}, r.id {direction} LIMIT $6 OFFSET $7"
    )
}

/// The number of matches, without the page's projection, ordering or its parameters: binds
/// $1..$5 only.
pub(crate) fn count_sql(query: &RepositoryListQuery<'_>, access: &str, readable: &str) -> String {
    let conditions = conditions(query, access, readable);
    format!("SELECT COUNT(*) FROM repositories r LEFT JOIN users u ON u.id=$1 WHERE {conditions}")
}

impl RepositoryListQuery<'_> {
    pub fn target_id(&self) -> Option<i64> {
        match self.list {
            RepositoryList::User(id)
            | RepositoryList::OwnedUser(id)
            | RepositoryList::Organization(id)
            | RepositoryList::Forks(id) => Some(id),
            _ => None,
        }
    }
}

native_operation!(crate::postgres::repositories::github::repository_pushed_at,
    crate::sqlite::repositories::github::repository_pushed_at,
    fn repository_pushed_at(pool: &DbPool, repo_id: i64) -> Result<chrono::DateTime<chrono::Utc>>);

native_operation!(crate::postgres::repositories::github::count_repositories,
    crate::sqlite::repositories::github::count_repositories,
    fn count_repositories(pool: &DbPool, query: RepositoryListQuery<'_>) -> Result<i64>);

native_operation!(crate::postgres::repositories::github::list_repositories,
    crate::sqlite::repositories::github::list_repositories,
    fn list_repositories(pool: &DbPool, query: RepositoryListQuery<'_>) -> Result<Vec<Repository>>);
