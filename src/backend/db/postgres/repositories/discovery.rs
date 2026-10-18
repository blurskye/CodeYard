use crate::backend::db::discussions::contains_pattern;
use crate::backend::db::models::{Issue, PullRequest, Repository, User};
use crate::backend::db::postgres::repositories::access::readable_condition;
use crate::backend::db::repositories::discovery::{
    repository_order_by, ItemSort, NewForkedRepository, RepositorySearch,
};
use crate::backend::db::repositories::records::RepositorySort;
use anyhow::Result;
use sqlx::PgPool;

pub async fn create_forked_repository(
    pool: &PgPool,
    input: NewForkedRepository<'_>,
) -> Result<Repository> {
    let result = sqlx::query_as::<_, Repository>(
        r#"
        INSERT INTO repositories (owner_kind, owner_user_id, name, description, is_private, disk_path, is_fork, forked_from_id, default_branch, created_by_id)
        SELECT 'user', $1, $2, $3, ($4 OR source.is_private OR source.visibility_locked), $5, true, $6, $7, $1
        FROM repositories source WHERE source.id=$6
        RETURNING *
        "#,
    )
    .bind(input.owner_id)
    .bind(input.name)
    .bind(input.description)
    .bind(input.is_private)
    .bind(input.disk_path)
    .bind(input.forked_from)
    .bind(input.default_branch)
    .fetch_one(pool)
    .await?;

    Ok(result)
}

pub async fn get_user_fork(
    pool: &PgPool,
    user_id: i64,
    forked_from: i64,
) -> Result<Option<Repository>> {
    let repo = sqlx::query_as::<_, Repository>(
        "SELECT * FROM repositories WHERE owner_kind = 'user' AND owner_user_id = $1 AND forked_from_id = $2",
    )
    .bind(user_id)
    .bind(forked_from)
    .fetch_optional(pool)
    .await?;

    Ok(repo)
}

pub async fn list_forks(
    pool: &PgPool,
    repo_id: i64,
    limit: i64,
    offset: i64,
) -> Result<Vec<Repository>> {
    anyhow::ensure!(offset >= 0, "Page offset must be nonnegative");
    let repos = sqlx::query_as::<_, Repository>(
        "SELECT * FROM repositories WHERE forked_from_id = $1 ORDER BY created_at DESC, id DESC LIMIT $2 OFFSET $3",
    )
    .bind(repo_id)
    .bind(limit.clamp(1, 100))
    .bind(offset)
    .fetch_all(pool)
    .await?;

    Ok(repos)
}

/// Joins the owner of `r` and the viewer, which the access projection reads as `u`. `$1` is the
/// viewer.
const OWNER_JOINS: &str =
    "LEFT JOIN users owner_user ON r.owner_kind = 'user' AND r.owner_user_id = owner_user.id \
    LEFT JOIN organizations owner_org ON r.owner_kind = 'org' AND r.owner_org_id = owner_org.id \
    LEFT JOIN users u ON u.id = $1";

const OWNER_NAME: &str = "COALESCE(owner_user.username, owner_org.name)";

/// The repositories `search` names. Text matches the name, description or owner. An owned search
/// matches the owner exactly and the name within it. The text is bound at `$2`; an owned search
/// binds its name at `$3`.
fn repository_match(search: &RepositorySearch<'_>) -> String {
    match search {
        RepositorySearch::Text(_) => {
            format!("(r.name ILIKE $2 OR r.description ILIKE $2 OR {OWNER_NAME} ILIKE $2)")
        }
        RepositorySearch::Owned { .. } => format!("{OWNER_NAME} = $2 AND r.name ILIKE $3"),
    }
}

pub async fn search_repositories(
    pool: &PgPool,
    viewer: Option<i64>,
    query: &str,
    sort: RepositorySort,
    offset: i64,
    limit: i64,
) -> Result<Vec<Repository>> {
    anyhow::ensure!(offset >= 0, "Result offset must be nonnegative");
    anyhow::ensure!(limit >= 0, "Result limit must be nonnegative");
    let search = RepositorySearch::parse(query);
    // The paging placeholders follow the match's binds: one for text, two for an owned search.
    let (limit_at, offset_at) = match &search {
        RepositorySearch::Text(_) => (3, 4),
        RepositorySearch::Owned { .. } => (4, 5),
    };
    let sql = format!(
        "SELECT r.* FROM repositories r {OWNER_JOINS} WHERE {} AND {} ORDER BY {} LIMIT ${limit_at} OFFSET ${offset_at}",
        readable_condition(),
        repository_match(&search),
        repository_order_by(sort)
    );
    let statement = sqlx::query_as::<_, Repository>(&sql).bind(viewer);
    let statement = match search {
        RepositorySearch::Text(text) => statement.bind(contains_pattern(text)),
        RepositorySearch::Owned { owner, name } => {
            statement.bind(owner).bind(contains_pattern(name))
        }
    };
    Ok(statement.bind(limit).bind(offset).fetch_all(pool).await?)
}

pub async fn count_repositories(
    pool: &PgPool,
    viewer: Option<i64>,
    query: &str,
    cap: i64,
) -> Result<i64> {
    let search = RepositorySearch::parse(query);
    // The cap follows the match's binds: one for text, two for an owned search.
    let cap_at = match &search {
        RepositorySearch::Text(_) => 3,
        RepositorySearch::Owned { .. } => 4,
    };
    let sql = format!(
        "SELECT COUNT(*) FROM (SELECT 1 FROM repositories r {OWNER_JOINS} WHERE {} AND {} LIMIT ${cap_at}) AS matches",
        readable_condition(),
        repository_match(&search)
    );
    let statement = sqlx::query_scalar::<_, i64>(&sql).bind(viewer);
    let statement = match search {
        RepositorySearch::Text(text) => statement.bind(contains_pattern(text)),
        RepositorySearch::Owned { owner, name } => {
            statement.bind(owner).bind(contains_pattern(name))
        }
    };
    Ok(statement.bind(cap).fetch_one(pool).await?)
}

pub async fn search_issues(
    pool: &PgPool,
    viewer: Option<i64>,
    query: &str,
    sort: ItemSort,
    offset: i64,
    limit: i64,
) -> Result<Vec<Issue>> {
    anyhow::ensure!(offset >= 0, "Result offset must be nonnegative");
    anyhow::ensure!(limit >= 0, "Result limit must be nonnegative");
    let sql = format!(
        "SELECT i.* FROM issues i \
         INNER JOIN repositories r ON i.repo_id = r.id \
         LEFT JOIN users u ON u.id = $1 \
         WHERE {} AND (i.title ILIKE $2 OR i.body ILIKE $2) \
         ORDER BY {} LIMIT $3 OFFSET $4",
        readable_condition(),
        sort.order_by("i")
    );
    Ok(sqlx::query_as::<_, Issue>(&sql)
        .bind(viewer)
        .bind(contains_pattern(query))
        .bind(limit)
        .bind(offset)
        .fetch_all(pool)
        .await?)
}

pub async fn count_issues(
    pool: &PgPool,
    viewer: Option<i64>,
    query: &str,
    cap: i64,
) -> Result<i64> {
    let sql = format!(
        "SELECT COUNT(*) FROM (SELECT 1 FROM issues i \
         INNER JOIN repositories r ON i.repo_id = r.id \
         LEFT JOIN users u ON u.id = $1 \
         WHERE {} AND (i.title ILIKE $2 OR i.body ILIKE $2) LIMIT $3) AS matches",
        readable_condition()
    );
    Ok(sqlx::query_scalar::<_, i64>(&sql)
        .bind(viewer)
        .bind(contains_pattern(query))
        .bind(cap)
        .fetch_one(pool)
        .await?)
}

pub async fn search_pull_requests(
    pool: &PgPool,
    viewer: Option<i64>,
    query: &str,
    sort: ItemSort,
    offset: i64,
    limit: i64,
) -> Result<Vec<PullRequest>> {
    anyhow::ensure!(offset >= 0, "Result offset must be nonnegative");
    anyhow::ensure!(limit >= 0, "Result limit must be nonnegative");
    let sql = format!(
        "SELECT p.* FROM pull_requests p \
         INNER JOIN repositories r ON p.repo_id = r.id \
         LEFT JOIN users u ON u.id = $1 \
         WHERE {} AND (p.title ILIKE $2 OR p.body ILIKE $2) \
         ORDER BY {} LIMIT $3 OFFSET $4",
        readable_condition(),
        sort.order_by("p")
    );
    Ok(sqlx::query_as::<_, PullRequest>(&sql)
        .bind(viewer)
        .bind(contains_pattern(query))
        .bind(limit)
        .bind(offset)
        .fetch_all(pool)
        .await?)
}

pub async fn count_pull_requests(
    pool: &PgPool,
    viewer: Option<i64>,
    query: &str,
    cap: i64,
) -> Result<i64> {
    let sql = format!(
        "SELECT COUNT(*) FROM (SELECT 1 FROM pull_requests p \
         INNER JOIN repositories r ON p.repo_id = r.id \
         LEFT JOIN users u ON u.id = $1 \
         WHERE {} AND (p.title ILIKE $2 OR p.body ILIKE $2) LIMIT $3) AS matches",
        readable_condition()
    );
    Ok(sqlx::query_scalar::<_, i64>(&sql)
        .bind(viewer)
        .bind(contains_pattern(query))
        .bind(cap)
        .fetch_one(pool)
        .await?)
}

pub async fn search_users(
    pool: &PgPool,
    query: &str,
    offset: i64,
    limit: i64,
) -> Result<Vec<User>> {
    anyhow::ensure!(offset >= 0, "Result offset must be nonnegative");
    anyhow::ensure!(limit >= 0, "Result limit must be nonnegative");
    Ok(sqlx::query_as::<_, User>(
        "SELECT * FROM users WHERE username ILIKE $1 OR display_name ILIKE $1 \
         ORDER BY username, id LIMIT $2 OFFSET $3",
    )
    .bind(contains_pattern(query))
    .bind(limit)
    .bind(offset)
    .fetch_all(pool)
    .await?)
}

pub async fn count_users(pool: &PgPool, query: &str, cap: i64) -> Result<i64> {
    Ok(sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*) FROM (SELECT 1 FROM users WHERE username ILIKE $1 OR display_name ILIKE $1 LIMIT $2) AS matches",
    )
    .bind(contains_pattern(query))
    .bind(cap)
    .fetch_one(pool)
    .await?)
}
