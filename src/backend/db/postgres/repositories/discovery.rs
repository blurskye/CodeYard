use crate::backend::db::models::{Issue, Repository, User};
use crate::backend::db::repositories::discovery::{NewForkedRepository, RepositorySearch};
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

pub async fn search_repositories(
    pool: &PgPool,
    query: &str,
    limit: i64,
) -> Result<Vec<Repository>> {
    let repos = match RepositorySearch::parse(query) {
        RepositorySearch::Text(text) => {
            sqlx::query_as::<_, Repository>(
                r#"
                SELECT r.* FROM repositories r
                LEFT JOIN users u ON r.owner_kind = 'user' AND r.owner_user_id = u.id
                LEFT JOIN organizations o ON r.owner_kind = 'org' AND r.owner_org_id = o.id
                WHERE r.is_private = false
                  AND (r.name ILIKE $1 OR r.description ILIKE $1
                       OR COALESCE(u.username, o.name) ILIKE $1)
                ORDER BY r.stars_count DESC, r.updated_at DESC
                LIMIT $2
                "#,
            )
            .bind(format!("%{text}%"))
            .bind(limit)
            .fetch_all(pool)
            .await?
        }
        RepositorySearch::Owned { owner, name } => {
            sqlx::query_as::<_, Repository>(
                r#"
                SELECT r.* FROM repositories r
                LEFT JOIN users u ON r.owner_kind = 'user' AND r.owner_user_id = u.id
                LEFT JOIN organizations o ON r.owner_kind = 'org' AND r.owner_org_id = o.id
                WHERE r.is_private = false
                  AND COALESCE(u.username, o.name) = $1
                  AND r.name ILIKE $2
                ORDER BY r.stars_count DESC, r.updated_at DESC
                LIMIT $3
                "#,
            )
            .bind(owner)
            .bind(format!("%{name}%"))
            .bind(limit)
            .fetch_all(pool)
            .await?
        }
    };

    Ok(repos)
}

pub async fn search_issues(pool: &PgPool, query: &str, limit: i64) -> Result<Vec<Issue>> {
    let pattern = format!("%{}%", query);
    let issues = sqlx::query_as::<_, Issue>(
        r#"
        SELECT i.* FROM issues i
        INNER JOIN repositories r ON i.repo_id = r.id
        WHERE r.is_private = false
          AND (i.title ILIKE $1 OR i.body ILIKE $1)
        ORDER BY i.updated_at DESC
        LIMIT $2
        "#,
    )
    .bind(&pattern)
    .bind(limit)
    .fetch_all(pool)
    .await?;

    Ok(issues)
}

pub async fn search_users(pool: &PgPool, query: &str, limit: i64) -> Result<Vec<User>> {
    let pattern = format!("%{}%", query);
    let users = sqlx::query_as::<_, User>(
        r#"
        SELECT * FROM users
        WHERE username ILIKE $1 OR display_name ILIKE $1
        ORDER BY username
        LIMIT $2
        "#,
    )
    .bind(&pattern)
    .bind(limit)
    .fetch_all(pool)
    .await?;

    Ok(users)
}
