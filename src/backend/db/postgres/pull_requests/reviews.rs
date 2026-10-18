use crate::backend::db::models::{PullRequestComment, PullRequestReview, User};
use crate::backend::db::postgres::discussions::{self, AUTHORITY_DENIED};
use crate::backend::db::pull_requests::reviews::DiscussionItem;
use crate::backend::db::pull_requests::reviews::DiscussionRef;
use crate::backend::db::pull_requests::reviews::NewPullRequestComment;
use crate::backend::db::pull_requests::reviews::PullRequestCommentEntry;
use crate::backend::db::pull_requests::reviews::PullRequestInlineComments;
use crate::backend::db::pull_requests::reviews::PullRequestReviewDecision;
use crate::backend::db::pull_requests::reviews::PullRequestReviewEntry;
use crate::backend::db::pull_requests::reviews::MAX_REVIEW_COMMENTS;
use anyhow::Result;
use sqlx::PgPool;
use std::collections::HashMap;

pub async fn add_pull_request_comment(
    pool: &PgPool,
    input: NewPullRequestComment<'_>,
) -> Result<PullRequestComment> {
    insert_comment(pool, input, false).await
}

pub async fn add_pending_pull_request_comment(
    pool: &PgPool,
    input: NewPullRequestComment<'_>,
) -> Result<PullRequestComment> {
    insert_comment(pool, input, true).await
}

/// A reply joins the review of the thread it answers, so `pending` only applies to a new thread.
async fn insert_comment(
    pool: &PgPool,
    input: NewPullRequestComment<'_>,
    pending: bool,
) -> Result<PullRequestComment> {
    let mut tx = pool.begin().await?;
    discussions::lock_pull_request_contribution(&mut tx, input.author_id, input.pr_id).await?;
    let review_id = match input.in_reply_to_comment_id {
        Some(parent) => {
            // Another author's pending comment is not a parent, because its review is private.
            let inherited: Option<Option<i64>> = sqlx::query_scalar(
                "SELECT c.review_id FROM pr_comments c
                 WHERE c.id=$1 AND c.pull_request_id=$2
                   AND NOT EXISTS (SELECT 1 FROM pr_reviews v WHERE v.id=c.review_id AND v.state='pending' AND v.author_id<>$3)",
            )
            .bind(parent)
            .bind(input.pr_id)
            .bind(input.author_id)
            .fetch_optional(&mut *tx)
            .await?;
            anyhow::ensure!(inherited.is_some(), AUTHORITY_DENIED);
            inherited.flatten()
        }
        None if pending => {
            // The service checked that the pull request is open before the write; a close can land
            // between the two, so the check is repeated here.
            let state: String = sqlx::query_scalar("SELECT state FROM pull_requests WHERE id=$1")
                .bind(input.pr_id)
                .fetch_one(&mut *tx)
                .await?;
            anyhow::ensure!(
                state == "open",
                crate::backend::db::discussions::DiscussionWriteRefusal::InvalidReview
            );
            Some(ensure_pending_review(&mut tx, input.pr_id, input.author_id).await?)
        }
        None => None,
    };
    if let Some(review_id) = review_id {
        let held: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM pr_comments WHERE review_id=$1")
            .bind(review_id)
            .fetch_one(&mut *tx)
            .await?;
        anyhow::ensure!(
            held < MAX_REVIEW_COMMENTS,
            crate::backend::db::discussions::DiscussionWriteRefusal::ReviewFull
        );
    }
    let comment = sqlx::query_as::<_, PullRequestComment>(
        r#"
        INSERT INTO pr_comments (pull_request_id, author_id, body, path, line, side, commit_sha, in_reply_to_comment_id, review_id)
        VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)
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
    .bind(review_id)
    .fetch_one(&mut *tx)
    .await?;
    sqlx::query("UPDATE pull_requests SET updated_at=GREATEST(clock_timestamp(),updated_at+INTERVAL '1 microsecond') WHERE id=$1")
        .bind(input.pr_id).execute(&mut *tx).await?;
    tx.commit().await?;

    Ok(comment)
}

/// The author's pending review on a pull request, started when there is none.
async fn ensure_pending_review(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    pr_id: i64,
    author_id: i64,
) -> Result<i64> {
    let pending: Option<i64> = sqlx::query_scalar(
        "SELECT id FROM pr_reviews WHERE pull_request_id=$1 AND author_id=$2 AND state='pending'",
    )
    .bind(pr_id)
    .bind(author_id)
    .fetch_optional(&mut **tx)
    .await?;
    match pending {
        Some(id) => Ok(id),
        None => Ok(sqlx::query_scalar(
            "INSERT INTO pr_reviews (pull_request_id, author_id, state) VALUES ($1, $2, 'pending') RETURNING id",
        )
        .bind(pr_id)
        .bind(author_id)
        .fetch_one(&mut **tx)
        .await?),
    }
}

/// Comments of pending reviews are left out of every list except the author's own.
pub async fn list_pull_request_comments(
    pool: &PgPool,
    pr_id: i64,
    limit: i64,
    offset: i64,
) -> Result<Vec<PullRequestCommentEntry>> {
    let columns = crate::backend::db::postgres::users::accounts::AUTHOR_DISPLAY_COLUMNS;
    // A path over the budget reads as absent, so it cannot expand a discussion page.
    let max_path_bytes = crate::backend::db::pull_requests::reviews::MAX_COMMENT_PATH_BYTES;
    let comments = sqlx::query_as::<_, PullRequestCommentEntry>(
        &format!("SELECT i.id, i.pull_request_id, i.author_id, i.body,
            CASE WHEN octet_length(i.path) <= {max_path_bytes} THEN i.path END AS path,
            i.line, i.side, i.commit_sha, i.in_reply_to_comment_id, i.created_at, i.updated_at,
            i.resolved_at, i.resolved_by_id, i.review_id,
            {columns} FROM pr_comments i JOIN users u ON u.id=i.author_id
            WHERE i.pull_request_id = $1
              AND NOT EXISTS (SELECT 1 FROM pr_reviews v WHERE v.id = i.review_id AND v.state = 'pending')
            ORDER BY i.created_at, i.id LIMIT $2 OFFSET $3"),
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
            i.resolved_at, i.resolved_by_id, i.review_id,
            {columns} FROM pr_comments i JOIN users u ON u.id=i.author_id
            WHERE i.pull_request_id = $1 AND i.path IS NOT NULL
              AND NOT EXISTS (SELECT 1 FROM pr_reviews v WHERE v.id = i.review_id AND v.state = 'pending')
            ORDER BY i.created_at, i.id LIMIT $2"),
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

/// The inline comments of the author's pending review, oldest first.
pub async fn list_pending_review_comments(
    pool: &PgPool,
    pr_id: i64,
    author_id: i64,
) -> Result<Vec<PullRequestCommentEntry>> {
    let columns = crate::backend::db::postgres::users::accounts::AUTHOR_DISPLAY_COLUMNS;
    let max_path_bytes = crate::backend::db::pull_requests::reviews::MAX_COMMENT_PATH_BYTES;
    let comments = sqlx::query_as::<_, PullRequestCommentEntry>(&format!(
        "SELECT i.id, i.pull_request_id, i.author_id, i.body,
            CASE WHEN octet_length(i.path) <= {max_path_bytes} THEN i.path END AS path,
            i.line, i.side, i.commit_sha, i.in_reply_to_comment_id, i.created_at, i.updated_at,
            i.resolved_at, i.resolved_by_id, i.review_id,
            {columns} FROM pr_comments i
            JOIN pr_reviews v ON v.id = i.review_id
            JOIN users u ON u.id = i.author_id
            WHERE i.pull_request_id = $1 AND v.state = 'pending' AND v.author_id = $2
            ORDER BY i.created_at, i.id"
    ))
    .bind(pr_id)
    .bind(author_id)
    .fetch_all(pool)
    .await?;
    Ok(comments)
}

/// Every comment filed in one of `review_ids`, oldest first.
pub async fn list_review_comments(
    pool: &PgPool,
    review_ids: &[i64],
) -> Result<Vec<PullRequestCommentEntry>> {
    debug_assert!(review_ids.len() <= 1000);
    comments_in(pool, "i.review_id", review_ids).await
}

/// The comments whose `column` is one of `ids`, oldest first. `column` is a column name written in
/// this module, never input.
async fn comments_in(
    pool: &PgPool,
    column: &str,
    ids: &[i64],
) -> Result<Vec<PullRequestCommentEntry>> {
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    let columns = crate::backend::db::postgres::users::accounts::AUTHOR_DISPLAY_COLUMNS;
    let max_path_bytes = crate::backend::db::pull_requests::reviews::MAX_COMMENT_PATH_BYTES;
    let mut query = sqlx::QueryBuilder::<sqlx::Postgres>::new(format!(
        "SELECT i.id, i.pull_request_id, i.author_id, i.body,
            CASE WHEN octet_length(i.path) <= {max_path_bytes} THEN i.path END AS path,
            i.line, i.side, i.commit_sha, i.in_reply_to_comment_id, i.created_at, i.updated_at,
            i.resolved_at, i.resolved_by_id, i.review_id,
            {columns} FROM pr_comments i JOIN users u ON u.id = i.author_id
            WHERE {column} IN ("
    ));
    let mut bound = query.separated(",");
    for id in ids {
        bound.push_bind(id);
    }
    query.push(") ORDER BY i.created_at, i.id");
    Ok(query.build_query_as().fetch_all(pool).await?)
}

/// The submitted reviews with these ids.
async fn reviews_with_ids(pool: &PgPool, ids: &[i64]) -> Result<Vec<PullRequestReviewEntry>> {
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    let columns = crate::backend::db::postgres::users::accounts::AUTHOR_DISPLAY_COLUMNS;
    let mut query = sqlx::QueryBuilder::<sqlx::Postgres>::new(format!(
        "SELECT i.*, {columns} FROM pr_reviews i JOIN users u ON u.id = i.author_id
            WHERE i.id IN ("
    ));
    let mut bound = query.separated(",");
    for id in ids {
        bound.push_bind(id);
    }
    query.push(")");
    Ok(query.build_query_as().fetch_all(pool).await?)
}

/// One page of the Conversation: the first `limit` items from `offset`. The page is read as
/// (kind, id) keys in order, then each kind is loaded by id. Kind 0 is a comment, kind 1 a review.
pub async fn list_discussion_items(
    pool: &PgPool,
    pr_id: i64,
    limit: i64,
    offset: i64,
) -> Result<Vec<DiscussionItem>> {
    let keys: Vec<(i64, i64)> = sqlx::query_as(
        "SELECT items.kind, items.id FROM (
            SELECT 0::bigint AS kind, c.id, c.created_at FROM pr_comments c
            WHERE c.pull_request_id = $1 AND c.review_id IS NULL
            UNION ALL
            SELECT 1::bigint, v.id, v.created_at FROM pr_reviews v
            WHERE v.pull_request_id = $1 AND v.state <> 'pending'
        ) items ORDER BY items.created_at, items.kind, items.id LIMIT $2 OFFSET $3",
    )
    .bind(pr_id)
    .bind(limit)
    .bind(offset)
    .fetch_all(pool)
    .await?;
    let comment_ids: Vec<i64> = keys
        .iter()
        .filter(|(kind, _)| *kind == 0)
        .map(|(_, id)| *id)
        .collect();
    let review_ids: Vec<i64> = keys
        .iter()
        .filter(|(kind, _)| *kind == 1)
        .map(|(_, id)| *id)
        .collect();
    let mut comments: HashMap<i64, PullRequestCommentEntry> =
        comments_in(pool, "i.id", &comment_ids)
            .await?
            .into_iter()
            .map(|entry| (entry.comment.id, entry))
            .collect();
    let mut reviews: HashMap<i64, PullRequestReviewEntry> = reviews_with_ids(pool, &review_ids)
        .await?
        .into_iter()
        .map(|entry| (entry.review.id, entry))
        .collect();
    Ok(keys
        .into_iter()
        .filter_map(|(kind, id)| match kind {
            0 => comments.remove(&id).map(DiscussionItem::Comment),
            _ => reviews.remove(&id).map(DiscussionItem::Review),
        })
        .collect())
}

/// The number of Conversation items up to and including `item`, in `list_discussion_items` order.
/// Items compare by time, then kind, then id. The item's own time is read in the query.
pub async fn count_discussion_items_through(
    pool: &PgPool,
    pr_id: i64,
    item: DiscussionRef,
) -> Result<i64> {
    let (kind, id): (i64, i64) = match item {
        DiscussionRef::Comment(id) => (0, id),
        DiscussionRef::Review(id) => (1, id),
    };
    Ok(sqlx::query_scalar(
        "WITH items(kind, id, created_at) AS (
            SELECT 0::bigint, c.id, c.created_at FROM pr_comments c
            WHERE c.pull_request_id = $1 AND c.review_id IS NULL
            UNION ALL
            SELECT 1::bigint, v.id, v.created_at FROM pr_reviews v
            WHERE v.pull_request_id = $1 AND v.state <> 'pending'
        ), target(created_at) AS (
            SELECT created_at FROM pr_comments WHERE $2::bigint = 0 AND id = $3 AND pull_request_id = $1
            UNION ALL
            SELECT created_at FROM pr_reviews WHERE $2::bigint = 1 AND id = $3 AND pull_request_id = $1
        )
        SELECT COUNT(*) FROM items, target
        WHERE items.created_at < target.created_at
           OR (items.created_at = target.created_at
               AND (items.kind < $2 OR (items.kind = $2 AND items.id <= $3)))",
    )
    .bind(pr_id)
    .bind(kind)
    .bind(id)
    .fetch_one(pool)
    .await?)
}

/// The author's pending review on a pull request, if it has one.
pub async fn get_pending_review(
    pool: &PgPool,
    pr_id: i64,
    author_id: i64,
) -> Result<Option<PullRequestReview>> {
    let review = sqlx::query_as::<_, PullRequestReview>(
        "SELECT * FROM pr_reviews WHERE pull_request_id = $1 AND author_id = $2 AND state = 'pending'",
    )
    .bind(pr_id)
    .bind(author_id)
    .fetch_optional(pool)
    .await?;
    Ok(review)
}

/// Deletes the author's pending review. Its comments go with it, and the pull request's revision
/// moves only when there was a review to delete.
pub async fn discard_pending_review(pool: &PgPool, pr_id: i64, author_id: i64) -> Result<bool> {
    let mut tx = pool.begin().await?;
    discussions::lock_pull_request_contribution(&mut tx, author_id, pr_id).await?;
    let result = sqlx::query(
        "DELETE FROM pr_reviews WHERE pull_request_id = $1 AND author_id = $2 AND state = 'pending'",
    )
    .bind(pr_id)
    .bind(author_id)
    .execute(&mut *tx)
    .await?;
    let discarded = result.rows_affected() > 0;
    if discarded {
        sqlx::query("UPDATE pull_requests SET updated_at=GREATEST(clock_timestamp(),updated_at+INTERVAL '1 microsecond') WHERE id=$1")
            .bind(pr_id)
            .execute(&mut *tx)
            .await?;
    }
    tx.commit().await?;
    Ok(discarded)
}

pub async fn count_pull_request_comments(pool: &PgPool, pr_id: i64) -> Result<i64> {
    let count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM pr_comments i WHERE i.pull_request_id = $1
           AND NOT EXISTS (SELECT 1 FROM pr_reviews v WHERE v.id = i.review_id AND v.state = 'pending')",
    )
    .bind(pr_id)
    .fetch_one(pool)
    .await?;
    Ok(count)
}

/// The comment count of each pull request in `pr_ids`. A pull request with no comments is absent.
pub async fn count_pull_request_comments_for(
    pool: &PgPool,
    pr_ids: &[i64],
) -> Result<Vec<(i64, i64)>> {
    debug_assert!(pr_ids.len() <= 500);
    if pr_ids.is_empty() {
        return Ok(Vec::new());
    }
    let mut query = sqlx::QueryBuilder::<sqlx::Postgres>::new(
        "SELECT i.pull_request_id, COUNT(*) FROM pr_comments i WHERE i.pull_request_id IN (",
    );
    let mut ids = query.separated(",");
    for id in pr_ids {
        ids.push_bind(id);
    }
    query.push(") AND NOT EXISTS (SELECT 1 FROM pr_reviews v WHERE v.id = i.review_id AND v.state = 'pending') GROUP BY i.pull_request_id");
    Ok(query.build_query_as().fetch_all(pool).await?)
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
           AND path IS NOT NULL AND resolved_at IS NULL
           AND NOT EXISTS (SELECT 1 FROM pr_reviews v WHERE v.id = pr_comments.review_id AND v.state = 'pending')",
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
           AND NOT EXISTS (SELECT 1 FROM pr_reviews v WHERE v.id = pr_comments.review_id AND v.state = 'pending')
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
    // Without a summary, the review needs inline comments of its own. The service checked this
    // earlier; a discard in between would leave an empty review, so the count is read here.
    if state != "approved" && body.is_none_or(|body| body.trim().is_empty()) {
        let inline: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM pr_comments c JOIN pr_reviews v ON v.id = c.review_id
             WHERE v.pull_request_id=$1 AND v.author_id=$2 AND v.state='pending'",
        )
        .bind(pr_id)
        .bind(author_id)
        .fetch_one(&mut *tx)
        .await?;
        anyhow::ensure!(
            inline > 0,
            crate::backend::db::discussions::DiscussionWriteRefusal::InvalidReview
        );
    }
    // The author's pending review, if any, is submitted with its comments by this review.
    let submitted = sqlx::query_as::<_, PullRequestReview>(
        "UPDATE pr_reviews SET state = $3, body = $4, commit_sha = $5, created_at = NOW()
         WHERE pull_request_id = $1 AND author_id = $2 AND state = 'pending'
         RETURNING *",
    )
    .bind(pr_id)
    .bind(author_id)
    .bind(state)
    .bind(body)
    .bind(commit_sha.as_deref())
    .fetch_optional(&mut *tx)
    .await?;
    let review = match submitted {
        Some(review) => review,
        None => {
            sqlx::query_as::<_, PullRequestReview>(
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
            .await?
        }
    };
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
        &format!("SELECT i.*, {columns} FROM pr_reviews i JOIN users u ON u.id=i.author_id WHERE i.pull_request_id = $1 AND i.state <> 'pending' ORDER BY i.created_at, i.id LIMIT $2 OFFSET $3"),
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
        ) d JOIN users u ON u.id = d.author_id
        WHERE d.latest = 1
        ORDER BY d.created_at, d.id
        "#,
    )
    .bind(pr_id)
    .fetch_all(pool)
    .await?;

    Ok(decisions)
}

/// The latest decision of each reviewer on each pull request in `pr_ids`, as the pull request ID,
/// the decision and the commit it was made on. Comments and pending reviews are left out, as
/// `list_pull_request_review_decisions` leaves them out.
pub async fn list_pull_request_review_states(
    pool: &PgPool,
    pr_ids: &[i64],
) -> Result<Vec<(i64, String, Option<String>)>> {
    debug_assert!(pr_ids.len() <= 500);
    if pr_ids.is_empty() {
        return Ok(Vec::new());
    }
    let mut query = sqlx::QueryBuilder::<sqlx::Postgres>::new(
        "SELECT pull_request_id, state, commit_sha FROM (
            SELECT pull_request_id, state, commit_sha,
                ROW_NUMBER() OVER (PARTITION BY pull_request_id, author_id ORDER BY created_at DESC, id DESC) AS latest
            FROM pr_reviews
            WHERE state NOT IN ('commented', 'pending') AND pull_request_id IN (",
    );
    let mut ids = query.separated(",");
    for id in pr_ids {
        ids.push_bind(id);
    }
    query.push(")) WHERE latest = 1");
    Ok(query.build_query_as().fetch_all(pool).await?)
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
