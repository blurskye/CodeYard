use crate::repositories::discovery::repository_order_by;
use crate::repositories::discovery::ItemSort;
use crate::repositories::discovery::NewForkedRepository;
use crate::repositories::discovery::RepositorySearch;
use crate::repositories::records::RepositorySort;
use anyhow::ensure;
use anyhow::Result;
use codeyard_db_core::models::Issue;
use codeyard_db_core::models::PullRequest;
use codeyard_db_core::models::Repository;
use codeyard_db_core::models::User;
use codeyard_db_core::queries::sqlite::repositories::access::readable_condition;
use codeyard_db_core::queries::sqlite::search::contains;
use sqlx::SqlitePool;

pub async fn create_forked_repository(
    pool: &SqlitePool,
    input: NewForkedRepository<'_>,
) -> Result<Repository> {
    // Read source policy and insert in one statement. Visibility-floor triggers
    // preserve/narrow descendants after source changes; no HTTP backend branch.
    Ok(sqlx::query_as(r#"INSERT INTO repositories(owner_kind,owner_user_id,name,description,is_private,disk_path,is_fork,forked_from_id,default_branch,created_by_id)
        SELECT 'user',?1,?2,?3,(?4 OR source.is_private OR source.visibility_locked),?5,1,?6,?7,?1
        FROM repositories source WHERE source.id=?6 AND NOT source.is_gist RETURNING *"#)
        .bind(input.owner_id).bind(input.name).bind(input.description).bind(input.is_private)
        .bind(input.disk_path).bind(input.forked_from).bind(input.default_branch).fetch_one(pool).await?)
}

pub async fn get_user_fork(
    pool: &SqlitePool,
    user_id: i64,
    forked_from: i64,
) -> Result<Option<Repository>> {
    get_owner_fork(
        pool,
        crate::repositories::creation::RepositoryTarget::Personal(user_id),
        forked_from,
    )
    .await
}

pub async fn get_owner_fork(
    pool: &SqlitePool,
    owner: crate::repositories::creation::RepositoryTarget,
    forked_from: i64,
) -> Result<Option<Repository>> {
    use crate::repositories::creation::RepositoryTarget;
    let (kind, user, org) = match owner {
        RepositoryTarget::Personal(id) => ("user", Some(id), None),
        RepositoryTarget::Organization(id) => ("org", None, Some(id)),
    };
    Ok(sqlx::query_as("SELECT * FROM repositories WHERE owner_kind=?1 AND (owner_user_id=?2 OR owner_org_id=?3) AND forked_from_id=?4 AND NOT is_gist").bind(kind).bind(user).bind(org).bind(forked_from).fetch_optional(pool).await?)
}

/// Resolve an owner's fork by network identity, not its mutable repository name.
pub async fn comparison_forks(
    pool: &SqlitePool,
    repo: i64,
    owner: &str,
) -> Result<Vec<Repository>> {
    Ok(sqlx::query_as("WITH RECURSIVE ancestors(id,forked_from_id) AS (SELECT id,forked_from_id FROM repositories WHERE id=?1 UNION SELECT r.id,r.forked_from_id FROM repositories r JOIN ancestors a ON a.forked_from_id=r.id), network(id) AS (SELECT id FROM ancestors WHERE forked_from_id IS NULL UNION SELECT r.id FROM repositories r JOIN network n ON r.forked_from_id=n.id) SELECT r.* FROM repositories r JOIN network n ON n.id=r.id LEFT JOIN users u ON u.id=r.owner_user_id LEFT JOIN organizations o ON o.id=r.owner_org_id WHERE (u.username=?2 OR o.name=?2) ORDER BY (r.name=(SELECT name FROM repositories WHERE id=?1)) DESC,r.id LIMIT 2").bind(repo).bind(owner).fetch_all(pool).await?)
}

pub async fn list_forks(
    pool: &SqlitePool,
    repo_id: i64,
    limit: i64,
    offset: i64,
) -> Result<Vec<Repository>> {
    ensure!(offset >= 0, "Page offset must be nonnegative");
    Ok(sqlx::query_as(
        "SELECT * FROM repositories WHERE forked_from_id=?1 AND NOT is_gist ORDER BY created_at DESC, id DESC LIMIT ?2 OFFSET ?3",
    )
    .bind(repo_id)
    .bind(limit.clamp(1, 100))
    .bind(offset)
    .fetch_all(pool)
    .await?)
}

const FORK_VIEWER_JOIN: &str = "LEFT JOIN users u ON u.id=?1 AND (?2 IS NULL OR EXISTS(SELECT 1 FROM sessions s WHERE s.id=?2 AND s.user_id=u.id AND s.expires_at>strftime('%Y-%m-%dT%H:%M:%f000Z','now')))";

pub async fn visible_fork_counts(
    pool: &SqlitePool,
    viewer: Option<i64>,
    session: Option<&str>,
    ids: &[i64],
) -> Result<std::collections::HashMap<i64, i32>> {
    ensure!(ids.len() <= 100, "Too many repository counts");
    if ids.is_empty() {
        return Ok(std::collections::HashMap::new());
    }
    let rows = sqlx::query_as::<_, (i64, i32)>(&format!(
        "SELECT r.forked_from_id, COUNT(*) FROM repositories r {FORK_VIEWER_JOIN} WHERE r.forked_from_id IN (SELECT value FROM json_each(?3)) AND {} GROUP BY r.forked_from_id",
        readable_condition(),
    )).bind(viewer).bind(session).bind(serde_json::to_string(ids)?).fetch_all(pool).await?;
    Ok(rows.into_iter().collect())
}

pub async fn list_visible_forks(
    pool: &SqlitePool,
    viewer: Option<i64>,
    session: Option<&str>,
    repo: i64,
    limit: i64,
    offset: i64,
) -> Result<Vec<Repository>> {
    ensure!(offset >= 0, "Page offset must be nonnegative");
    Ok(sqlx::query_as(&format!(
        "SELECT r.* FROM repositories r {FORK_VIEWER_JOIN} WHERE r.forked_from_id=?3 AND {} ORDER BY r.created_at DESC, r.id DESC LIMIT ?4 OFFSET ?5",
        readable_condition(),
    )).bind(viewer).bind(session).bind(repo).bind(limit.clamp(1, 101)).bind(offset).fetch_all(pool).await?)
}

/// Joins the owner of `r` and the viewer, which the access projection reads as `u`. `?1` is the
/// viewer.
const OWNER_JOINS: &str = "LEFT JOIN users ON r.owner_kind='user' AND r.owner_user_id=users.id \
    LEFT JOIN organizations ON r.owner_kind='org' AND r.owner_org_id=organizations.id \
    LEFT JOIN users u ON u.id = ?1";

const OWNER_NAME: &str = "COALESCE(users.username, organizations.name)";

/// The repositories `search` names. Text matches the name, description or owner. An owned search
/// matches the owner exactly and the name within it. The text is bound at `?2`; an owned search
/// binds its name at `?3`.
fn repository_match(search: &RepositorySearch<'_>) -> String {
    match search {
        RepositorySearch::Text(_) => {
            format!("(r.name REGEXP ?2 OR r.description REGEXP ?2 OR {OWNER_NAME} REGEXP ?2)")
        }
        RepositorySearch::Owned { .. } => format!("{OWNER_NAME} = ?2 AND r.name REGEXP ?3"),
    }
}

pub async fn search_repositories(
    pool: &SqlitePool,
    viewer: Option<i64>,
    query: &str,
    sort: RepositorySort,
    offset: i64,
    limit: i64,
) -> Result<Vec<Repository>> {
    ensure!(offset >= 0, "Result offset must be nonnegative");
    ensure!(limit >= 0, "Result limit must be nonnegative");
    let mut connection = search_connection(pool).await?;
    let pool = &mut *connection;
    let search = RepositorySearch::parse(query);
    // The paging placeholders follow the match's binds: one for text, two for an owned search.
    let (limit_at, offset_at) = match &search {
        RepositorySearch::Text(_) => (3, 4),
        RepositorySearch::Owned { .. } => (4, 5),
    };
    let sql = format!(
        "SELECT r.* FROM repositories r {OWNER_JOINS} WHERE {} AND {} ORDER BY {} LIMIT ?{limit_at} OFFSET ?{offset_at}",
        readable_condition(),
        repository_match(&search),
        repository_order_by(sort)
    );
    let statement = sqlx::query_as::<_, Repository>(&sql).bind(viewer);
    let statement = match search {
        RepositorySearch::Text(text) => statement.bind(contains(text)),
        RepositorySearch::Owned { owner, name } => statement.bind(owner).bind(contains(name)),
    };
    let result = statement.bind(limit).bind(offset).fetch_all(pool).await;
    connection.finish().await?;
    Ok(result?)
}

pub async fn search_repository_ids(
    pool: &SqlitePool,
    viewer: Option<i64>,
    query: &str,
    limit: i64,
) -> Result<Vec<i64>> {
    ensure!(limit >= 0, "Result limit must be nonnegative");
    let search = RepositorySearch::parse(query);
    // The limit follows the match's binds: one for text, two for an owned search.
    let limit_at = match &search {
        RepositorySearch::Text(_) => 3,
        RepositorySearch::Owned { .. } => 4,
    };
    let sql = format!(
        "SELECT r.id FROM repositories r {OWNER_JOINS} WHERE {} AND {} ORDER BY {} LIMIT ?{limit_at}",
        readable_condition(),
        repository_match(&search),
        repository_order_by(RepositorySort::Updated)
    );
    let mut connection = search_connection(pool).await?;
    let statement = sqlx::query_scalar::<_, i64>(&sql).bind(viewer);
    let statement = match search {
        RepositorySearch::Text(text) => statement.bind(contains(text)),
        RepositorySearch::Owned { owner, name } => statement.bind(owner).bind(contains(name)),
    };
    let result = statement.bind(limit).fetch_all(&mut *connection).await;
    connection.finish().await?;
    Ok(result?)
}

pub async fn count_repositories(
    pool: &SqlitePool,
    viewer: Option<i64>,
    query: &str,
    cap: i64,
) -> Result<i64> {
    let mut connection = search_connection(pool).await?;
    let pool = &mut *connection;
    let search = RepositorySearch::parse(query);
    // The cap follows the match's binds: one for text, two for an owned search.
    let cap_at = match &search {
        RepositorySearch::Text(_) => 3,
        RepositorySearch::Owned { .. } => 4,
    };
    let sql = format!(
        "SELECT COUNT(*) FROM (SELECT 1 FROM repositories r {OWNER_JOINS} WHERE {} AND {} LIMIT ?{cap_at}) AS matches",
        readable_condition(),
        repository_match(&search)
    );
    let statement = sqlx::query_scalar::<_, i64>(&sql).bind(viewer);
    let statement = match search {
        RepositorySearch::Text(text) => statement.bind(contains(text)),
        RepositorySearch::Owned { owner, name } => statement.bind(owner).bind(contains(name)),
    };
    let result = statement.bind(cap).fetch_one(pool).await;
    connection.finish().await?;
    Ok(result?)
}

pub(crate) struct SearchConnection(Option<sqlx::pool::PoolConnection<sqlx::Sqlite>>);

impl std::ops::Deref for SearchConnection {
    type Target = sqlx::SqliteConnection;
    fn deref(&self) -> &Self::Target {
        self.0.as_ref().expect("search connection")
    }
}
impl std::ops::DerefMut for SearchConnection {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.0.as_mut().expect("search connection")
    }
}
impl SearchConnection {
    pub async fn finish(mut self) -> Result<()> {
        self.lock_handle().await?.remove_progress_handler();
        self.0.take();
        Ok(())
    }
}
impl Drop for SearchConnection {
    fn drop(&mut self) {
        // Cancellation must not leave an expired callback on a pooled connection.
        if let Some(connection) = self.0.as_mut() {
            connection.close_on_drop();
        }
    }
}

pub(crate) async fn search_connection(pool: &SqlitePool) -> Result<SearchConnection> {
    let mut connection = SearchConnection(Some(pool.acquire().await?));
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(1);
    connection
        .lock_handle()
        .await?
        .set_progress_handler(1000, move || std::time::Instant::now() < deadline);
    Ok(connection)
}

pub async fn search_issues(
    pool: &SqlitePool,
    viewer: Option<i64>,
    query: &str,
    sort: ItemSort,
    offset: i64,
    limit: i64,
) -> Result<Vec<Issue>> {
    let mut connection = search_connection(pool).await?;
    let pool = &mut *connection;
    ensure!(offset >= 0, "Result offset must be nonnegative");
    ensure!(limit >= 0, "Result limit must be nonnegative");
    let sql = format!(
        "SELECT i.* FROM issues i CROSS JOIN repositories r ON r.id=i.repo_id \
         LEFT JOIN users u ON u.id = ?1 \
         WHERE {} AND (i.title REGEXP ?2 OR i.body REGEXP ?2) ORDER BY {} LIMIT ?3 OFFSET ?4",
        readable_condition(),
        sort.order_by("i")
    );
    let result = sqlx::query_as::<_, Issue>(&sql)
        .bind(viewer)
        .bind(contains(query))
        .bind(limit)
        .bind(offset)
        .fetch_all(pool)
        .await;
    connection.finish().await?;
    Ok(result?)
}

pub async fn count_issues(
    pool: &SqlitePool,
    viewer: Option<i64>,
    query: &str,
    cap: i64,
) -> Result<i64> {
    let mut connection = search_connection(pool).await?;
    let pool = &mut *connection;
    let sql = format!(
        "SELECT COUNT(*) FROM (SELECT 1 FROM issues i CROSS JOIN repositories r ON r.id=i.repo_id \
         LEFT JOIN users u ON u.id = ?1 \
         WHERE {} AND (i.title REGEXP ?2 OR i.body REGEXP ?2) LIMIT ?3) AS matches",
        readable_condition()
    );
    let result = sqlx::query_scalar::<_, i64>(&sql)
        .bind(viewer)
        .bind(contains(query))
        .bind(cap)
        .fetch_one(pool)
        .await;
    connection.finish().await?;
    Ok(result?)
}

pub async fn search_pull_requests(
    pool: &SqlitePool,
    viewer: Option<i64>,
    query: &str,
    sort: ItemSort,
    offset: i64,
    limit: i64,
) -> Result<Vec<PullRequest>> {
    let mut connection = search_connection(pool).await?;
    let pool = &mut *connection;
    ensure!(offset >= 0, "Result offset must be nonnegative");
    ensure!(limit >= 0, "Result limit must be nonnegative");
    let sql = format!(
        "SELECT p.* FROM pull_requests p CROSS JOIN repositories r ON r.id=p.repo_id \
         LEFT JOIN users u ON u.id = ?1 \
         WHERE {} AND (p.title REGEXP ?2 OR p.body REGEXP ?2) ORDER BY {} LIMIT ?3 OFFSET ?4",
        readable_condition(),
        sort.order_by("p")
    );
    let result = sqlx::query_as::<_, PullRequest>(&sql)
        .bind(viewer)
        .bind(contains(query))
        .bind(limit)
        .bind(offset)
        .fetch_all(pool)
        .await;
    connection.finish().await?;
    Ok(result?)
}

pub async fn count_pull_requests(
    pool: &SqlitePool,
    viewer: Option<i64>,
    query: &str,
    cap: i64,
) -> Result<i64> {
    let mut connection = search_connection(pool).await?;
    let pool = &mut *connection;
    let sql = format!(
        "SELECT COUNT(*) FROM (SELECT 1 FROM pull_requests p CROSS JOIN repositories r ON r.id=p.repo_id \
         LEFT JOIN users u ON u.id = ?1 \
         WHERE {} AND (p.title REGEXP ?2 OR p.body REGEXP ?2) LIMIT ?3) AS matches",
        readable_condition()
    );
    let result = sqlx::query_scalar::<_, i64>(&sql)
        .bind(viewer)
        .bind(contains(query))
        .bind(cap)
        .fetch_one(pool)
        .await;
    connection.finish().await?;
    Ok(result?)
}

pub async fn search_users(
    pool: &SqlitePool,
    query: &str,
    offset: i64,
    limit: i64,
) -> Result<Vec<User>> {
    ensure!(offset >= 0, "Result offset must be nonnegative");
    ensure!(limit >= 0, "Result limit must be nonnegative");
    let mut connection = search_connection(pool).await?;
    let result = sqlx::query_as::<_, User>(
        "SELECT * FROM users WHERE username REGEXP ?1 OR display_name REGEXP ?1 \
         ORDER BY username, id LIMIT ?2 OFFSET ?3",
    )
    .bind(contains(query))
    .bind(limit)
    .bind(offset)
    .fetch_all(&mut *connection)
    .await;
    connection.finish().await?;
    Ok(result?)
}

pub async fn count_users(pool: &SqlitePool, query: &str, cap: i64) -> Result<i64> {
    let mut connection = search_connection(pool).await?;
    let result = sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*) FROM (SELECT 1 FROM users WHERE username REGEXP ?1 OR display_name REGEXP ?1 LIMIT ?2) AS matches",
    )
    .bind(contains(query)).bind(cap).fetch_one(&mut *connection).await;
    connection.finish().await?;
    Ok(result?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn native_search_deadline_stops_work_and_clears_the_callback() {
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        let mut connection = search_connection(&pool).await.unwrap();
        let error = sqlx::query_scalar::<_,i64>("WITH RECURSIVE n(x) AS (VALUES(0) UNION ALL SELECT x+1 FROM n WHERE x<1000000000) SELECT sum(x) FROM n")
            .fetch_one(&mut *connection).await.unwrap_err();
        assert_eq!(
            error.as_database_error().unwrap().code().as_deref(),
            Some("9")
        );
        connection.finish().await.unwrap();
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT 1")
                .fetch_one(&pool)
                .await
                .unwrap(),
            1
        );
        pool.close().await;
    }
}
