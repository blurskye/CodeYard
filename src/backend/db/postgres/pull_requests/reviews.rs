use crate::backend::db::models::{PullRequestComment, PullRequestReview, User};
use crate::backend::db::postgres::discussions::{self, AUTHORITY_DENIED};
use crate::backend::db::pull_requests::reviews::NewPullRequestComment;
use crate::backend::db::pull_requests::reviews::PullRequestCommentEntry;
use crate::backend::db::pull_requests::reviews::PullRequestInlineComments;
use crate::backend::db::pull_requests::reviews::PullRequestReviewDecision;
use crate::backend::db::pull_requests::reviews::PullRequestReviewEntry;
use anyhow::Result;
use sqlx::PgPool;

pub async fn add_pull_request_comment(
    pool: &PgPool,
    input: NewPullRequestComment<'_>,
) -> Result<PullRequestComment> {
    let mut tx = pool.begin().await?;
    discussions::lock_pull_request_contribution(&mut tx, input.author_id, input.pr_id).await?;
    if let Some(parent) = input.in_reply_to_comment_id {
        let scoped: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM pr_comments WHERE id=$1 AND pull_request_id=$2)",
        )
        .bind(parent)
        .bind(input.pr_id)
        .fetch_one(&mut *tx)
        .await?;
        anyhow::ensure!(scoped, AUTHORITY_DENIED);
    }
    let comment = sqlx::query_as::<_, PullRequestComment>(
        r#"
        INSERT INTO pr_comments (pull_request_id, author_id, body, path, line, side, commit_sha, in_reply_to_comment_id)
        VALUES ($1, $2, $3, $4, $5, $6, $7, $8)
        RETURNING *
        "#,
    )
    .bind(input.pr_id)
    .bind(input.author_id)
    .bind(input.body)
    .bind(input.path)
    .bind(input.line)
    .bind(input.side)
    .bind(input.commit_sha)
    .bind(input.in_reply_to_comment_id)
    .fetch_one(&mut *tx)
    .await?;
    sqlx::query("UPDATE pull_requests SET updated_at=GREATEST(clock_timestamp(),updated_at+INTERVAL '1 microsecond') WHERE id=$1")
        .bind(input.pr_id).execute(&mut *tx).await?;
    tx.commit().await?;

    Ok(comment)
}

pub async fn list_pull_request_comments(
    pool: &PgPool,
    pr_id: i64,
    limit: i64,
    offset: i64,
) -> Result<Vec<PullRequestCommentEntry>> {
    let columns = crate::backend::db::postgres::users::accounts::AUTHOR_DISPLAY_COLUMNS;
    // Legacy paths stay stored, but never expand a discussion page beyond its path budget.
    let max_path_bytes = crate::backend::db::pull_requests::reviews::MAX_COMMENT_PATH_BYTES;
    let comments = sqlx::query_as::<_, PullRequestCommentEntry>(
        &format!("SELECT i.id, i.pull_request_id, i.author_id, i.body,
            CASE WHEN octet_length(i.path) <= {max_path_bytes} THEN i.path END AS path,
            i.line, i.side, i.commit_sha, i.in_reply_to_comment_id, i.created_at, i.updated_at,
            i.resolved_at, i.resolved_by_id,
            {columns} FROM pr_comments i LEFT JOIN users u ON u.id=i.author_id WHERE i.pull_request_id = $1 ORDER BY i.created_at, i.id LIMIT $2 OFFSET $3"),
    )
    .bind(pr_id)
    .bind(limit)
    .bind(offset)
    .fetch_all(pool)
    .await?;

    Ok(comments)
}

/// Comments that sit on a file, oldest first, capped at `MAX_INLINE_COMMENTS`.
pub async fn list_pull_request_inline_comments(
    pool: &PgPool,
    pr_id: i64,
) -> Result<PullRequestInlineComments> {
    let columns = crate::backend::db::postgres::users::accounts::AUTHOR_DISPLAY_COLUMNS;
    let max_path_bytes = crate::backend::db::pull_requests::reviews::MAX_COMMENT_PATH_BYTES;
    let max = crate::backend::db::pull_requests::reviews::MAX_INLINE_COMMENTS;
    // One row past the cap shows that the list is incomplete without loading the rest.
    let mut comments = sqlx::query_as::<_, PullRequestCommentEntry>(
        &format!("SELECT i.id, i.pull_request_id, i.author_id, i.body,
            CASE WHEN octet_length(i.path) <= {max_path_bytes} THEN i.path END AS path,
            i.line, i.side, i.commit_sha, i.in_reply_to_comment_id, i.created_at, i.updated_at,
            i.resolved_at, i.resolved_by_id,
            {columns} FROM pr_comments i LEFT JOIN users u ON u.id=i.author_id WHERE i.pull_request_id = $1 AND i.path IS NOT NULL ORDER BY i.created_at, i.id LIMIT $2"),
    )
    .bind(pr_id)
    .bind(i64::try_from(max + 1)?)
    .fetch_all(pool)
    .await?;
    let truncated = comments.len() > max;
    comments.truncate(max);
    Ok(PullRequestInlineComments {
        comments,
        truncated,
    })
}

pub async fn count_pull_request_comments(pool: &PgPool, pr_id: i64) -> Result<i64> {
    let count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM pr_comments WHERE pull_request_id = $1")
            .bind(pr_id)
            .fetch_one(pool)
            .await?;
    Ok(count)
}

pub async fn get_pull_request_comment_by_id(
    pool: &PgPool,
    comment_id: i64,
) -> Result<Option<PullRequestComment>> {
    let comment =
        sqlx::query_as::<_, PullRequestComment>("SELECT * FROM pr_comments WHERE id = $1")
            .bind(comment_id)
            .fetch_optional(pool)
            .await?;

    Ok(comment)
}

/// Review conversations on a file that nobody has resolved: root comments with a path.
pub async fn count_unresolved_conversations(pool: &PgPool, pr_id: i64) -> Result<i64> {
    let count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM pr_comments
         WHERE pull_request_id = $1 AND in_reply_to_comment_id IS NULL
           AND path IS NOT NULL AND resolved_at IS NULL",
    )
    .bind(pr_id)
    .fetch_one(pool)
    .await?;
    Ok(count)
}

/// Marks a root review comment of the pull request resolved by `resolver_id`, or open again.
/// `None` when the pull request has no such comment. The resolver's authority is checked in
/// the same transaction as the update.
pub async fn set_pull_request_comment_resolved(
    pool: &PgPool,
    pr_id: i64,
    comment_id: i64,
    resolved: bool,
    resolver_id: i64,
) -> Result<Option<PullRequestComment>> {
    let mut tx = pool.begin().await?;
    discussions::lock_comment_resolution_authority(&mut tx, resolver_id, pr_id).await?;
    let comment = sqlx::query_as::<_, PullRequestComment>(
        "UPDATE pr_comments SET resolved_at = CASE WHEN $3 THEN NOW() END, resolved_by_id = CASE WHEN $3 THEN $4 END
         WHERE id = $1 AND pull_request_id = $2
           AND in_reply_to_comment_id IS NULL AND path IS NOT NULL
         RETURNING *",
    )
    .bind(comment_id)
    .bind(pr_id)
    .bind(resolved)
    .bind(resolver_id)
    .fetch_optional(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(comment)
}

pub async fn add_pull_request_review(
    pool: &PgPool,
    pr_id: i64,
    author_id: i64,
    state: &str,
    body: Option<&str>,
    commit_sha: Option<&str>,
) -> Result<PullRequestReview> {
    let commit_sha = commit_sha.map(str::to_ascii_lowercase);
    let mut tx = pool.begin().await?;
    // A plain comment needs read access; a vote needs write access.
    let (author, current_state, current_head) = if state == "commented" {
        discussions::lock_pull_request_contribution(&mut tx, author_id, pr_id).await?;
        discussions::read_pull_request_review_state(&mut tx, pr_id).await?
    } else {
        discussions::lock_pull_request_review_authority(&mut tx, author_id, pr_id).await?
    };
    anyhow::ensure!(
        current_state == "open"
            && matches!(state, "approved" | "changes_requested" | "commented")
            && commit_sha.as_deref() == Some(current_head.as_str())
            && (state == "commented" || author != author_id),
        crate::backend::db::discussions::DiscussionWriteRefusal::InvalidReview
    );
    let review = sqlx::query_as::<_, PullRequestReview>(
        r#"
        INSERT INTO pr_reviews (pull_request_id, author_id, state, body, commit_sha)
        VALUES ($1, $2, $3, $4, $5)
        RETURNING *
        "#,
    )
    .bind(pr_id)
    .bind(author_id)
    .bind(state)
    .bind(body)
    .bind(commit_sha.as_deref())
    .fetch_one(&mut *tx)
    .await?;
    sqlx::query("UPDATE pull_requests SET updated_at=GREATEST(clock_timestamp(),updated_at+INTERVAL '1 microsecond') WHERE id=$1")
        .bind(pr_id).execute(&mut *tx).await?;
    // Consuming a request belongs to the same admitted review transaction.
    sqlx::query("DELETE FROM pr_reviewers WHERE pull_request_id=$1 AND user_id=$2")
        .bind(pr_id)
        .bind(author_id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;

    Ok(review)
}

pub async fn list_pull_request_reviews(
    pool: &PgPool,
    pr_id: i64,
    limit: i64,
    offset: i64,
) -> Result<Vec<PullRequestReviewEntry>> {
    let columns = crate::backend::db::postgres::users::accounts::AUTHOR_DISPLAY_COLUMNS;
    let reviews = sqlx::query_as::<_, PullRequestReviewEntry>(
        &format!("SELECT i.*, {columns} FROM pr_reviews i LEFT JOIN users u ON u.id=i.author_id WHERE i.pull_request_id = $1 ORDER BY i.created_at, i.id LIMIT $2 OFFSET $3"),
    )
    .bind(pr_id)
    .bind(limit)
    .bind(offset)
    .fetch_all(pool)
    .await?;

    Ok(reviews)
}

pub async fn list_pull_request_review_decisions(
    pool: &PgPool,
    pr_id: i64,
) -> Result<Vec<PullRequestReviewDecision>> {
    let decisions = sqlx::query_as::<_, PullRequestReviewDecision>(
        r#"
        SELECT d.author_id, d.state, d.commit_sha, u.username FROM (
            SELECT author_id, state, commit_sha, created_at, id,
                ROW_NUMBER() OVER (PARTITION BY author_id ORDER BY created_at DESC, id DESC) AS latest
            FROM pr_reviews
            WHERE pull_request_id = $1 AND state NOT IN ('commented', 'pending')
        ) d LEFT JOIN users u ON u.id = d.author_id
        WHERE d.latest = 1
        ORDER BY d.created_at, d.id
        "#,
    )
    .bind(pr_id)
    .fetch_all(pool)
    .await?;

    Ok(decisions)
}

pub async fn get_pull_request_review_by_id(
    pool: &PgPool,
    review_id: i64,
) -> Result<Option<PullRequestReview>> {
    let review = sqlx::query_as::<_, PullRequestReview>("SELECT * FROM pr_reviews WHERE id = $1")
        .bind(review_id)
        .fetch_optional(pool)
        .await?;

    Ok(review)
}

pub async fn dismiss_pull_request_review(
    pool: &PgPool,
    review_id: i64,
    dismissed_by_id: i64,
    reason: Option<&str>,
) -> Result<PullRequestReview> {
    let mut tx = pool.begin().await?;
    let pr_id =
        discussions::lock_review_dismissal_authority(&mut tx, dismissed_by_id, review_id).await?;
    let review = sqlx::query_as::<_, PullRequestReview>(
        r#"
        UPDATE pr_reviews
        SET state = 'dismissed', dismissed_by_id = $1, dismissed_reason = $2, dismissed_at = NOW()
        WHERE id = $3 AND pull_request_id = $4
        RETURNING *
        "#,
    )
    .bind(dismissed_by_id)
    .bind(reason)
    .bind(review_id)
    .bind(pr_id)
    .fetch_one(&mut *tx)
    .await?;
    sqlx::query("UPDATE pull_requests SET updated_at=GREATEST(clock_timestamp(),updated_at+INTERVAL '1 microsecond') WHERE id=$1")
        .bind(review.pull_request_id).execute(&mut *tx).await?;
    tx.commit().await?;

    Ok(review)
}

pub async fn request_pull_request_reviewer(
    pool: &PgPool,
    pr_id: i64,
    actor_id: i64,
    user_id: i64,
) -> Result<bool> {
    let mut tx = pool.begin().await?;
    let (author, state, _) =
        discussions::lock_pull_request_review_authority(&mut tx, actor_id, pr_id).await?;
    anyhow::ensure!(
        state == "open",
        crate::backend::db::discussions::DiscussionWriteRefusal::InvalidReview
    );
    let repo_id: i64 = sqlx::query_scalar("SELECT repo_id FROM pull_requests WHERE id=$1")
        .bind(pr_id)
        .fetch_one(&mut *tx)
        .await?;
    // A request must not disclose a private discussion to an ineligible target.
    anyhow::ensure!(
        user_id != author && discussions::lock_write_authority(&mut tx, user_id, repo_id).await?,
        AUTHORITY_DENIED
    );
    let result = sqlx::query(
        "INSERT INTO pr_reviewers (pull_request_id, user_id) VALUES ($1, $2) ON CONFLICT DO NOTHING",
    )
    .bind(pr_id)
    .bind(user_id)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(result.rows_affected() > 0)
}

pub async fn remove_pull_request_reviewer(
    pool: &PgPool,
    pr_id: i64,
    actor_id: i64,
    user_id: i64,
) -> Result<bool> {
    let mut tx = pool.begin().await?;
    discussions::lock_pull_request_review_authority(&mut tx, actor_id, pr_id).await?;
    let result =
        sqlx::query("DELETE FROM pr_reviewers WHERE pull_request_id = $1 AND user_id = $2")
            .bind(pr_id)
            .bind(user_id)
            .execute(&mut *tx)
            .await?;
    tx.commit().await?;
    Ok(result.rows_affected() > 0)
}

pub async fn list_pull_request_reviewers(pool: &PgPool, pr_id: i64) -> Result<Vec<User>> {
    let users = sqlx::query_as::<_, User>(
        r#"
        SELECT u.* FROM users u
        INNER JOIN pr_reviewers r ON r.user_id = u.id
        WHERE r.pull_request_id = $1
        ORDER BY u.username
        "#,
    )
    .bind(pr_id)
    .fetch_all(pool)
    .await?;

    Ok(users)
}

impl<'r> sqlx::FromRow<'r, sqlx::postgres::PgRow> for PullRequestCommentEntry {
    fn from_row(row: &'r sqlx::postgres::PgRow) -> Result<Self, sqlx::Error> {
        Ok(Self {
            comment: PullRequestComment::from_row(row)?,
            author: crate::backend::db::postgres::users::accounts::author_display(row)?,
        })
    }
}

impl<'r> sqlx::FromRow<'r, sqlx::postgres::PgRow> for PullRequestReviewEntry {
    fn from_row(row: &'r sqlx::postgres::PgRow) -> Result<Self, sqlx::Error> {
        Ok(Self {
            review: PullRequestReview::from_row(row)?,
            author: crate::backend::db::postgres::users::accounts::author_display(row)?,
        })
    }
}
