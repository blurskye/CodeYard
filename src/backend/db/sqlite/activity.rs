use crate::backend::db::activity::{ActivityBatch, ScannedActivity};
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

/// One bounded scan of activity, authorized in the same statement as its content.
pub async fn list_user_activity(
    pool: &SqlitePool,
    user_id: i64,
    viewer: Option<i64>,
    limit: i64,
    offset: i64,
) -> Result<ActivityBatch> {
    let readable = super::repositories::access::readable_condition();
    let rows = sqlx::query_as::<_, ScannedActivity>(&format!(
        r#"
        WITH activity AS (
            SELECT 'push' AS kind, p.id AS item_id, p.repo_id, p.created_at,
                   NULL AS number, NULL AS title, p.branch, p.commit_count
            FROM repository_pushes p
            WHERE p.actor_id = ?1 AND p.commit_count > 0
            UNION ALL
            SELECT 'repository_created', r.id, r.id, r.created_at,
                   NULL, NULL, NULL, NULL
            FROM repositories r
            WHERE r.created_by_id = ?1
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
            AND NOT EXISTS (SELECT 1 FROM pr_reviews v WHERE v.id = c.review_id AND v.state = 'pending')
        ), scanned AS (
            SELECT * FROM activity ORDER BY created_at DESC, kind, item_id DESC
            LIMIT ?2 OFFSET ?3
        )
        SELECT a.kind, a.item_id, a.repo_id, a.created_at, a.number, a.title, a.branch,
               a.commit_count, COALESCE(owner.username, organizations.name) AS owner_name,
               r.name AS repo_name, COALESCE(({readable}),FALSE) AS readable
        FROM scanned a
        JOIN repositories r ON r.id = a.repo_id
        LEFT JOIN users owner ON r.owner_kind = 'user' AND r.owner_user_id = owner.id
        LEFT JOIN organizations ON r.owner_kind = 'org' AND r.owner_org_id = organizations.id
        LEFT JOIN users u ON u.id = ?4
        ORDER BY a.created_at DESC, a.kind, a.item_id DESC
        "#,
    ))
    .bind(user_id)
    .bind(limit)
    .bind(offset)
    .bind(viewer)
    .fetch_all(pool)
    .await?;
    Ok(ActivityBatch::from_scan(rows))
}
