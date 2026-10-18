use crate::backend::db::reactions::{ReactionCount, ReactionRefusal, Subject};
use anyhow::Result;
use sqlx::{Sqlite, SqlitePool, Transaction};

/// Adds the reaction, or removes it when the user already gave it. Returns whether it is now set.
/// BEGIN IMMEDIATE holds the write lock, so the checks and the change see the same state: a revoked
/// grant, an archived repository or a pending comment refuses the write.
pub async fn toggle_reaction(
    pool: &SqlitePool,
    actor_id: i64,
    repo_id: i64,
    item_id: i64,
    subject: Subject,
    subject_id: i64,
    content: &str,
) -> Result<bool> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    super::discussions::require_contribution_authority(&mut tx, actor_id, repo_id).await?;
    let item_table = match subject {
        Subject::Issue | Subject::IssueComment => "issues",
        Subject::PullRequest | Subject::PullRequestComment | Subject::PullRequestReview => {
            "pull_requests"
        }
    };
    let item: Option<i64> = sqlx::query_scalar(&format!(
        "SELECT id FROM {item_table} WHERE id = ?1 AND repo_id = ?2"
    ))
    .bind(item_id)
    .bind(repo_id)
    .fetch_optional(&mut *tx)
    .await?;
    anyhow::ensure!(item.is_some(), ReactionRefusal::NotFound);
    anyhow::ensure!(
        is_on_item(&mut tx, subject, subject_id, item_id).await?,
        ReactionRefusal::NotFound
    );
    let removed = sqlx::query(
        "DELETE FROM reactions WHERE user_id = ?1 AND subject_kind = ?2 AND subject_id = ?3 AND content = ?4",
    )
    .bind(actor_id)
    .bind(subject.name())
    .bind(subject_id)
    .bind(content)
    .execute(&mut *tx)
    .await?;
    let set = removed.rows_affected() == 0;
    if set {
        sqlx::query(
            "INSERT INTO reactions (repo_id, user_id, subject_kind, subject_id, content) VALUES (?1, ?2, ?3, ?4, ?5) ON CONFLICT DO NOTHING",
        )
        .bind(repo_id)
        .bind(actor_id)
        .bind(subject.name())
        .bind(subject_id)
        .bind(content)
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    Ok(set)
}

/// The reactions on `ids`, in the order they were first given.
pub async fn reaction_counts(
    pool: &SqlitePool,
    subject: &str,
    ids: &[i64],
    viewer_id: i64,
) -> Result<Vec<ReactionCount>> {
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    let rows: Vec<(i64, String, i64, i64)> = sqlx::query_as(
        r#"
        SELECT subject_id, content, COUNT(*), MAX(user_id = ?3)
        FROM reactions
        WHERE subject_kind = ?1 AND subject_id IN (SELECT value FROM json_each(?2))
        GROUP BY subject_id, content
        ORDER BY MIN(id)
        "#,
    )
    .bind(subject)
    .bind(serde_json::to_string(ids)?)
    .bind(viewer_id)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|(subject_id, content, count, mine)| ReactionCount {
            subject_id,
            content,
            count,
            mine: mine != 0,
        })
        .collect())
}

/// Whether the subject is the item itself, or a comment or review on it. Comments and reviews of a
/// pending review are left out, so nobody can react to them before they are submitted.
async fn is_on_item(
    tx: &mut Transaction<'_, Sqlite>,
    subject: Subject,
    id: i64,
    item_id: i64,
) -> Result<bool> {
    let sql = match subject {
        Subject::Issue | Subject::PullRequest => return Ok(id == item_id),
        Subject::IssueComment => "SELECT 1 FROM issue_comments WHERE id = ?1 AND issue_id = ?2",
        Subject::PullRequestComment => "SELECT 1 FROM pr_comments c WHERE c.id = ?1 AND c.pull_request_id = ?2 AND NOT EXISTS (SELECT 1 FROM pr_reviews v WHERE v.id = c.review_id AND v.state = 'pending')",
        Subject::PullRequestReview => "SELECT 1 FROM pr_reviews WHERE id = ?1 AND pull_request_id = ?2 AND state <> 'pending'",
    };
    let found: Option<i32> = sqlx::query_scalar(sql)
        .bind(id)
        .bind(item_id)
        .fetch_optional(&mut **tx)
        .await?;
    Ok(found.is_some())
}
