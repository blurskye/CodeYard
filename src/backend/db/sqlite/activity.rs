use crate::backend::db::activity::ActivityRow;
use anyhow::Result;
use sqlx::SqlitePool;

pub async fn record_repository_push(
    pool: &SqlitePool,
    repo_id: i64,
    actor_id: i64,
    branch: &str,
    commit_count: i32,
) -> Result<()> {
    sqlx::query(
        "INSERT INTO repository_pushes (repo_id, actor_id, branch, commit_count) VALUES (?1, ?2, ?3, ?4)",
    )
    .bind(repo_id)
    .bind(actor_id)
    .bind(branch)
    .bind(commit_count)
    .execute(pool)
    .await?;
    Ok(())
}

/// Activity of one user, newest first, over every repository. Callers filter by what the
/// viewer may read.
pub async fn list_user_activity(
    pool: &SqlitePool,
    user_id: i64,
    limit: i64,
    offset: i64,
) -> Result<Vec<ActivityRow>> {
    Ok(sqlx::query_as::<_, ActivityRow>(
        r#"
        WITH activity AS (
            SELECT 'push' AS kind, p.id AS item_id, p.repo_id, p.created_at,
                   NULL AS number, NULL AS title, p.branch, p.commit_count
            FROM repository_pushes p
            WHERE p.actor_id = ?1
            UNION ALL
            SELECT 'repository_created', r.id, r.id, r.created_at,
                   NULL, NULL, NULL, NULL
            FROM repositories r
            WHERE r.created_by_id = ?1
               OR (r.created_by_id IS NULL AND r.owner_kind = 'user' AND r.owner_user_id = ?1)
            UNION ALL
            SELECT 'issue_opened', i.id, i.repo_id, i.created_at,
                   i.number, i.title, NULL, NULL
            FROM issues i
            WHERE i.author_id = ?1
            UNION ALL
            SELECT 'pull_request_opened', p.id, p.repo_id, p.created_at,
                   p.number, p.title, NULL, NULL
            FROM pull_requests p
            WHERE p.author_id = ?1
            UNION ALL
            SELECT 'pull_request_merged', p.id, p.repo_id, p.merged_at,
                   p.number, p.title, NULL, NULL
            FROM pull_requests p
            WHERE p.merged_by_id = ?1 AND p.merged_at IS NOT NULL
            UNION ALL
            SELECT 'issue_comment', c.id, i.repo_id, c.created_at,
                   i.number, i.title, NULL, NULL
            FROM issue_comments c
            JOIN issues i ON i.id = c.issue_id
            WHERE c.author_id = ?1
            UNION ALL
            SELECT 'pull_request_comment', c.id, p.repo_id, c.created_at,
                   p.number, p.title, NULL, NULL
            FROM pr_comments c
            JOIN pull_requests p ON p.id = c.pull_request_id
            WHERE c.author_id = ?1
        )
        SELECT a.kind, a.item_id, a.repo_id, a.created_at, a.number, a.title, a.branch,
               a.commit_count, COALESCE(users.username, organizations.name) AS owner_name,
               repositories.name AS repo_name
        FROM activity a
        JOIN repositories ON repositories.id = a.repo_id
        LEFT JOIN users ON repositories.owner_kind = 'user' AND repositories.owner_user_id = users.id
        LEFT JOIN organizations ON repositories.owner_kind = 'org' AND repositories.owner_org_id = organizations.id
        ORDER BY a.created_at DESC, a.kind, a.item_id DESC
        LIMIT ?2 OFFSET ?3
        "#,
    )
    .bind(user_id)
    .bind(limit)
    .bind(offset)
    .fetch_all(pool)
    .await?)
}
