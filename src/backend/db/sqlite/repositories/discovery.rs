use crate::backend::db::sqlite::search::{collect_matches, Pattern};
use crate::backend::db::{
    models::{Issue, Repository, User},
    repositories::discovery::{NewForkedRepository, RepositorySearch},
};
use anyhow::{ensure, Result};
use futures::StreamExt;
use sqlx::{sqlite::SqliteRow, FromRow, Row, SqlitePool};

pub async fn create_forked_repository(
    pool: &SqlitePool,
    input: NewForkedRepository<'_>,
) -> Result<Repository> {
    // Read source policy and insert in one statement. Visibility-floor triggers
    // preserve/narrow descendants after source changes; no HTTP backend branch.
    Ok(sqlx::query_as(r#"INSERT INTO repositories(owner_kind,owner_user_id,name,description,is_private,disk_path,is_fork,forked_from_id,default_branch,created_by_id)
        SELECT 'user',?1,?2,?3,(?4 OR source.is_private OR source.visibility_locked),?5,1,?6,?7,?1
        FROM repositories source WHERE source.id=?6 RETURNING *"#)
        .bind(input.owner_id).bind(input.name).bind(input.description).bind(input.is_private)
        .bind(input.disk_path).bind(input.forked_from).bind(input.default_branch).fetch_one(pool).await?)
}

pub async fn get_user_fork(
    pool: &SqlitePool,
    user_id: i64,
    forked_from: i64,
) -> Result<Option<Repository>> {
    Ok(sqlx::query_as("SELECT * FROM repositories WHERE owner_kind='user' AND owner_user_id=?1 AND forked_from_id=?2")
        .bind(user_id).bind(forked_from).fetch_optional(pool).await?)
}

pub async fn list_forks(
    pool: &SqlitePool,
    repo_id: i64,
    limit: i64,
    offset: i64,
) -> Result<Vec<Repository>> {
    ensure!(offset >= 0, "Page offset must be nonnegative");
    Ok(sqlx::query_as(
        "SELECT * FROM repositories WHERE forked_from_id=?1 ORDER BY created_at DESC, id DESC LIMIT ?2 OFFSET ?3",
    )
    .bind(repo_id)
    .bind(limit.clamp(1, 100))
    .bind(offset)
    .fetch_all(pool)
    .await?)
}

pub async fn search_repositories(
    pool: &SqlitePool,
    query: &str,
    limit: i64,
) -> Result<Vec<Repository>> {
    ensure!(limit >= 0, "Result limit must be nonnegative");
    if limit == 0 {
        return Ok(Vec::new());
    }
    // Each row is read with its owner's name, which the matcher needs.
    let rows = match RepositorySearch::parse(query) {
        RepositorySearch::Text(text) => {
            let pattern = Pattern::contains(text);
            // Read public repositories through the ordered index so SQLite does not sort them all
            // before the matching budget applies.
            let rows = sqlx::query(
                "SELECT repositories.*, COALESCE(users.username, organizations.name) AS owner_name \
                 FROM repositories INDEXED BY idx_repositories_public_search \
                 LEFT JOIN users ON repositories.owner_kind='user' AND repositories.owner_user_id=users.id \
                 LEFT JOIN organizations ON repositories.owner_kind='org' AND repositories.owner_org_id=organizations.id \
                 WHERE NOT repositories.is_private \
                 ORDER BY repositories.stars_count DESC,repositories.updated_at DESC",
            )
            .fetch(pool)
            .map(repository_with_owner);
            collect_matches(rows, &pattern, limit, |(repo, owner)| {
                [
                    Some(repo.name.as_str()),
                    repo.description.as_deref(),
                    Some(owner.as_str()),
                ]
            })
            .await?
        }
        RepositorySearch::Owned { owner, name } => {
            let pattern = Pattern::contains(name);
            let rows = sqlx::query(
                "SELECT repositories.*, COALESCE(users.username, organizations.name) AS owner_name \
                 FROM repositories \
                 LEFT JOIN users ON repositories.owner_kind='user' AND repositories.owner_user_id=users.id \
                 LEFT JOIN organizations ON repositories.owner_kind='org' AND repositories.owner_org_id=organizations.id \
                 WHERE NOT repositories.is_private \
                   AND COALESCE(users.username, organizations.name) = ?1 \
                 ORDER BY repositories.stars_count DESC,repositories.updated_at DESC",
            )
            .bind(owner)
            .fetch(pool)
            .map(repository_with_owner);
            collect_matches(rows, &pattern, limit, |(repo, _)| {
                [Some(repo.name.as_str()), None, None]
            })
            .await?
        }
    };
    Ok(rows.into_iter().map(|(repo, _)| repo).collect())
}

fn repository_with_owner(
    row: std::result::Result<SqliteRow, sqlx::Error>,
) -> std::result::Result<(Repository, String), sqlx::Error> {
    let row = row?;
    let owner: String = row.try_get("owner_name")?;
    Ok((Repository::from_row(&row)?, owner))
}

pub async fn search_issues(pool: &SqlitePool, query: &str, limit: i64) -> Result<Vec<Issue>> {
    ensure!(limit >= 0, "Result limit must be nonnegative");
    if limit == 0 {
        return Ok(Vec::new());
    }
    let pattern = Pattern::contains(query);
    // Keep issues as the outer ordered scan so SQLite cannot sort all public issues
    // before yielding the first row to the matching budget.
    let rows = sqlx::query_as::<_, Issue>(
        "SELECT i.* FROM issues i INDEXED BY idx_issues_updated_at CROSS JOIN repositories r ON r.id=i.repo_id WHERE NOT r.is_private ORDER BY i.updated_at DESC",
    )
    .fetch(pool);
    collect_matches(rows, &pattern, limit, |issue| {
        [Some(issue.title.as_str()), Some(issue.body.as_str()), None]
    })
    .await
}

pub async fn search_users(pool: &SqlitePool, query: &str, limit: i64) -> Result<Vec<User>> {
    ensure!(limit >= 0, "Result limit must be nonnegative");
    if limit == 0 {
        return Ok(Vec::new());
    }
    let pattern = Pattern::contains(query);
    let rows = sqlx::query_as::<_, User>("SELECT * FROM users ORDER BY username").fetch(pool);
    collect_matches(rows, &pattern, limit, |user| {
        [
            Some(user.username.as_str()),
            user.display_name.as_deref(),
            None,
        ]
    })
    .await
}
