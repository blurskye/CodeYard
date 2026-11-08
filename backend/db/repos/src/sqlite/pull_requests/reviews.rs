use crate::pull_requests::reviews::DiscussionItem;
use crate::pull_requests::reviews::DiscussionRef;
use crate::pull_requests::reviews::NewPullRequestComment;
use crate::pull_requests::reviews::PullRequestCommentEntry;
use crate::pull_requests::reviews::PullRequestInlineComments;
use crate::pull_requests::reviews::PullRequestReviewDecision;
use crate::pull_requests::reviews::PullRequestReviewEntry;
use crate::pull_requests::reviews::MAX_REVIEW_COMMENTS;
use crate::sqlite::discussions::AUTHORITY_DENIED;
use anyhow::Result;
use codeyard_database::models::PullRequest;
use codeyard_database::models::PullRequestComment;
use codeyard_database::models::PullRequestReview;
use codeyard_database::models::User;
use codeyard_database::pool::SqlitePool;
use std::collections::HashMap;

pub async fn add_pull_request_comment(
    pool: &SqlitePool,
    input: NewPullRequestComment<'_>,
) -> Result<PullRequestComment> {
    insert_comment(pool, input, false, None)
        .await
        .map(|(comment, _)| comment)
}

pub async fn add_pending_pull_request_comment(
    pool: &SqlitePool,
    input: NewPullRequestComment<'_>,
) -> Result<PullRequestComment> {
    insert_comment(pool, input, true, None)
        .await
        .map(|(comment, _)| comment)
}

pub async fn add_comment_to_pending_review(
    pool: &SqlitePool,
    input: NewPullRequestComment<'_>,
    review_id: i64,
) -> Result<PullRequestComment> {
    insert_comment(pool, input, true, Some(review_id))
        .await
        .map(|(comment, _)| comment)
}

/// A reply joins the review of the thread it answers, so `pending` only applies to a new thread.
pub async fn insert_comment(
    pool: &SqlitePool,
    input: NewPullRequestComment<'_>,
    pending: bool,
    expected_review: Option<i64>,
) -> Result<(PullRequestComment, bool)> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    crate::sqlite::discussions::lock_pull_request_contribution(
        &mut tx,
        input.author_id,
        input.pr_id,
    )
    .await?;
    if let Some((head, base)) = input.expected_revisions {
        let current: (String, String) =
            sqlx::query_as("SELECT head_sha, base_sha FROM pull_requests WHERE id=?1")
                .bind(input.pr_id)
                .fetch_one(&mut *tx)
                .await?;
        anyhow::ensure!(
            current.0 == head && current.1 == base,
            codeyard_database::queries::discussions::DiscussionWriteRefusal::RevisionChanged
        );
    }
    let review_id = match input.in_reply_to_comment_id {
        Some(parent) => {
            // Another author's pending comment is not a parent, because its review is private.
            let inherited: Option<Option<i64>> = sqlx::query_scalar(
                "SELECT c.review_id FROM pr_comments c
                 WHERE c.id=?1 AND c.pull_request_id=?2
                   AND NOT EXISTS (SELECT 1 FROM pr_reviews v WHERE v.id=c.review_id AND v.state='pending' AND v.author_id<>?3)",
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
            let state: String = sqlx::query_scalar("SELECT state FROM pull_requests WHERE id=?1")
                .bind(input.pr_id)
                .fetch_one(&mut *tx)
                .await?;
            anyhow::ensure!(
                state == "open",
                codeyard_database::queries::discussions::DiscussionWriteRefusal::InvalidReview
            );
            Some(ensure_pending_review(&mut tx, input.pr_id, input.author_id).await?)
        }
        None => None,
    };
    if let Some(expected) = expected_review {
        anyhow::ensure!(
            review_id == Some(expected),
            codeyard_database::queries::discussions::DiscussionWriteRefusal::InvalidReview
        );
    }
    if let Some(review_id) = review_id {
        let held: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM pr_comments c JOIN pr_reviews v ON v.id=c.review_id WHERE c.review_id=?1 AND v.state='pending'")
            .bind(review_id)
            .fetch_one(&mut *tx)
            .await?;
        anyhow::ensure!(
            held < MAX_REVIEW_COMMENTS,
            codeyard_database::queries::discussions::DiscussionWriteRefusal::ReviewFull
        );
    }
    let comment = insert_comment_on(&mut tx, &input, review_id).await?;
    let deferred: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM pr_reviews WHERE id=?1 AND state='pending')",
    )
    .bind(review_id)
    .fetch_one(&mut *tx)
    .await?;
    touch_revision(&mut tx, input.pr_id).await?;
    tx.commit().await?;
    Ok((comment, deferred))
}

async fn insert_comment_on(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    input: &NewPullRequestComment<'_>,
    review_id: Option<i64>,
) -> Result<PullRequestComment> {
    let comment = sqlx::query_as::<_, PullRequestComment>(
        r#"
        INSERT INTO pr_comments (pull_request_id, author_id, body, path, line, side, commit_sha, in_reply_to_comment_id, review_id,start_line,start_side,diff_base_sha)
        VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9,?10,?11,CASE WHEN ?4 IS NOT NULL THEN COALESCE((SELECT diff_base_sha FROM pr_comments WHERE id=?8),(SELECT COALESCE(fork_sha,base_sha) FROM pull_requests WHERE id=?1)) END)
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
    .bind(input.start_line)
    .bind(input.start_side)
    .fetch_one(&mut **tx)
    .await?;
    Ok(comment)
}

/// The author's pending review on a pull request, started when there is none.
async fn ensure_pending_review(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    pr_id: i64,
    author_id: i64,
) -> Result<i64> {
    let pending: Option<i64> = sqlx::query_scalar(
        "SELECT id FROM pr_reviews WHERE pull_request_id=?1 AND author_id=?2 AND state='pending'",
    )
    .bind(pr_id)
    .bind(author_id)
    .fetch_optional(&mut **tx)
    .await?;
    match pending {
        Some(id) => Ok(id),
        None => Ok(sqlx::query_scalar(
            "INSERT INTO pr_reviews (pull_request_id, author_id, state, commit_sha) SELECT id, ?2, 'pending', head_sha FROM pull_requests WHERE id=?1 RETURNING id",
        )
        .bind(pr_id)
        .bind(author_id)
        .fetch_one(&mut **tx)
        .await?),
    }
}

/// Comments of pending reviews are left out of every list except the author's own.
pub async fn list_pull_request_comments(
    pool: &SqlitePool,
    pr_id: i64,
    limit: i64,
    offset: i64,
) -> Result<Vec<PullRequestCommentEntry>> {
    let columns = codeyard_db_identity::sqlite::users::accounts::AUTHOR_DISPLAY_COLUMNS;
    // A path over the budget reads as absent, so it cannot expand a discussion page.
    let max_path_bytes = crate::pull_requests::reviews::MAX_COMMENT_PATH_BYTES;
    let comments = sqlx::query_as::<_, PullRequestCommentEntry>(
        sqlx::AssertSqlSafe(format!("SELECT i.id, i.pull_request_id, i.author_id, i.body,
            CASE WHEN length(CAST(i.path AS BLOB)) <= {max_path_bytes} THEN i.path END AS path,
            i.line, i.side, i.commit_sha, i.in_reply_to_comment_id, i.created_at, i.updated_at,
            i.resolved_at, i.resolved_by_id, i.review_id,
            i.start_line,i.start_side,{columns} FROM pr_comments i JOIN users u ON u.id=i.author_id
            WHERE i.pull_request_id = ?1
              AND NOT EXISTS (SELECT 1 FROM pr_reviews v WHERE v.id = i.review_id AND v.state = 'pending')
            ORDER BY i.created_at, i.id LIMIT ?2 OFFSET ?3")),
    )
    .bind(pr_id)
    .bind(limit)
    .bind(offset)
    .fetch_all(pool)
    .await?;

    Ok(comments)
}

pub async fn conversation_comment_position(pool: &SqlitePool, comment: i64) -> Result<i64> {
    Ok(sqlx::query_scalar("SELECT COUNT(*) FROM pr_comments i JOIN pr_comments target ON target.pull_request_id=i.pull_request_id WHERE target.id=?1 AND i.path IS NULL AND NOT EXISTS(SELECT 1 FROM pr_reviews v WHERE v.id=i.review_id AND v.state='pending') AND (i.created_at,i.id)<=(target.created_at,target.id)").bind(comment).fetch_one(pool).await?)
}

pub async fn list_pull_request_conversation_comments(
    pool: &SqlitePool,
    pr_id: i64,
    limit: i64,
    offset: i64,
) -> Result<Vec<PullRequestCommentEntry>> {
    list_pull_request_conversation_comments_ordered(pool, pr_id, limit, offset, false, false).await
}

pub async fn list_pull_request_conversation_comments_ordered(
    pool: &SqlitePool,
    pr_id: i64,
    limit: i64,
    offset: i64,
    updated: bool,
    descending: bool,
) -> Result<Vec<PullRequestCommentEntry>> {
    let order = if updated { "updated_at" } else { "created_at" };
    let direction = if descending { "DESC" } else { "ASC" };
    let columns = codeyard_db_identity::sqlite::users::accounts::AUTHOR_DISPLAY_COLUMNS;
    // A path over the budget reads as absent, so it cannot expand a discussion page.
    let max_path_bytes = crate::pull_requests::reviews::MAX_COMMENT_PATH_BYTES;
    let comments = sqlx::query_as::<_, PullRequestCommentEntry>(
        sqlx::AssertSqlSafe(format!("SELECT i.id, i.pull_request_id, i.author_id, i.body,
            CASE WHEN length(CAST(i.path AS BLOB)) <= {max_path_bytes} THEN i.path END AS path,
            i.line, i.side, i.commit_sha, i.in_reply_to_comment_id, i.created_at, i.updated_at,
            i.resolved_at, i.resolved_by_id, i.review_id,
            i.start_line,i.start_side,{columns} FROM pr_comments i JOIN users u ON u.id=i.author_id
            WHERE i.pull_request_id = ?1 AND i.path IS NULL
              AND NOT EXISTS (SELECT 1 FROM pr_reviews v WHERE v.id = i.review_id AND v.state = 'pending')
            ORDER BY i.{order} {direction}, i.id {direction} LIMIT ?2 OFFSET ?3")),
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
    pool: &SqlitePool,
    pr_id: i64,
) -> Result<PullRequestInlineComments> {
    use crate::pull_requests::reviews::budgeted_comment_ids;
    use crate::pull_requests::reviews::MAX_INLINE_COMMENTS;
    use crate::pull_requests::reviews::MAX_INLINE_COMMENT_BYTES;
    let mut tx = pool.begin().await?;
    let rows: Vec<(i64, i64)> = sqlx::query_as("SELECT i.id,6*(length(CAST(i.body AS BLOB))+COALESCE(MIN(length(CAST(i.path AS BLOB)),4096),0))+4096 FROM pr_comments i WHERE i.pull_request_id=?1 AND i.path IS NOT NULL AND NOT EXISTS(SELECT 1 FROM pr_reviews v WHERE v.id=i.review_id AND v.state='pending') ORDER BY i.created_at,i.id LIMIT ?2")
        .bind(pr_id).bind((MAX_INLINE_COMMENTS+1) as i64).fetch_all(&mut *tx).await?;
    let ids = budgeted_comment_ids(&rows, MAX_INLINE_COMMENTS, MAX_INLINE_COMMENT_BYTES)?;
    let truncated = ids.len() < rows.len();
    let count = rows.len().min(MAX_INLINE_COMMENTS);
    let comments = comments_in(&mut *tx, "i.id", &ids).await?;
    tx.commit().await?;
    Ok(PullRequestInlineComments {
        comments,
        truncated,
        count,
    })
}

/// The inline comments of the author's pending review, oldest first.
pub async fn list_pending_review_comments(
    pool: &SqlitePool,
    pr_id: i64,
    author_id: i64,
) -> Result<PullRequestInlineComments> {
    use crate::pull_requests::reviews::budgeted_comment_ids;
    use crate::pull_requests::reviews::MAX_REVIEW_COMMENT_PAGE_BYTES;
    let mut tx = pool.begin().await?;
    let rows: Vec<(i64, i64)> = sqlx::query_as("SELECT i.id,6*(length(CAST(i.body AS BLOB))+COALESCE(MIN(length(CAST(i.path AS BLOB)),4096),0))+4096 FROM pr_comments i JOIN pr_reviews v ON v.id=i.review_id WHERE i.pull_request_id=?1 AND v.state='pending' AND v.author_id=?2 ORDER BY i.created_at,i.id LIMIT ?3")
        .bind(pr_id).bind(author_id).bind(MAX_REVIEW_COMMENTS+1).fetch_all(&mut *tx).await?;
    let ids = budgeted_comment_ids(
        &rows,
        MAX_REVIEW_COMMENTS as usize,
        MAX_REVIEW_COMMENT_PAGE_BYTES,
    )?;
    let truncated = ids.len() < rows.len();
    let count = rows.len().min(MAX_REVIEW_COMMENTS as usize);
    let comments = comments_in(&mut *tx, "i.id", &ids).await?;
    tx.commit().await?;
    Ok(PullRequestInlineComments {
        comments,
        truncated,
        count,
    })
}

/// Every comment filed in one of `review_ids`, oldest first.
pub async fn list_review_comments(
    pool: &SqlitePool,
    review_ids: &[i64],
) -> Result<Vec<PullRequestCommentEntry>> {
    debug_assert!(review_ids.len() <= 1000);
    comments_in(pool, "i.review_id", review_ids).await
}

pub async fn review_comment_position(
    pool: &SqlitePool,
    review_ids: &[i64],
    comment_id: i64,
) -> Result<(i64, i64)> {
    if review_ids.is_empty() {
        return Ok((0, 0));
    }
    let mut query = sqlx::QueryBuilder::<sqlx::Sqlite>::new(
        "SELECT MAX(COUNT(*) - 1, 0), COALESCE(SUM(6 * (length(CAST(c.body AS BLOB)) + COALESCE(MIN(length(CAST(c.path AS BLOB)), 4096), 0)) + 4096), 0) FROM pr_comments c WHERE c.review_id IN (",
    );
    let mut ids = query.separated(",");
    for id in review_ids {
        ids.push_bind(id);
    }
    query.push(") AND (c.created_at,c.id) <= (SELECT created_at,id FROM pr_comments WHERE id=");
    query.push_bind(comment_id).push(")");
    Ok(query.build_query_as().fetch_one(pool).await?)
}

/// Select sizes before bodies, so nested reviews cannot bypass the discussion page limit.
pub async fn list_review_comments_page(
    pool: &SqlitePool,
    review_ids: &[i64],
    offset: i64,
) -> Result<crate::pull_requests::reviews::ReviewCommentPage> {
    use crate::pull_requests::reviews::budgeted_comment_ids;
    use crate::pull_requests::reviews::ReviewCommentPage;
    use crate::pull_requests::reviews::MAX_REVIEW_COMMENTS_PAGE;
    use crate::pull_requests::reviews::MAX_REVIEW_COMMENT_PAGE_BYTES;
    if review_ids.is_empty() {
        return Ok(ReviewCommentPage {
            comments: Vec::new(),
            has_more: false,
        });
    }
    let mut tx = pool.begin().await?;
    let mut query = sqlx::QueryBuilder::<sqlx::Sqlite>::new("SELECT id, (6 * (length(CAST(body AS BLOB)) + COALESCE(MIN(length(CAST(path AS BLOB)), 4096), 0)) + 4096) FROM pr_comments WHERE review_id IN (");
    let mut ids = query.separated(",");
    for id in review_ids {
        ids.push_bind(id);
    }
    query
        .push(") ORDER BY created_at, id LIMIT ")
        .push_bind((MAX_REVIEW_COMMENTS_PAGE + 1) as i64)
        .push(" OFFSET ")
        .push_bind(offset);
    let rows: Vec<(i64, i64)> = query.build_query_as().fetch_all(&mut *tx).await?;
    let total = rows.len();
    let ids = budgeted_comment_ids(
        &rows,
        MAX_REVIEW_COMMENTS_PAGE,
        MAX_REVIEW_COMMENT_PAGE_BYTES,
    )?;
    let has_more = ids.len() < total;
    let comments = comments_in(&mut *tx, "i.id", &ids).await?;
    tx.commit().await?;
    Ok(ReviewCommentPage { comments, has_more })
}

/// The comments whose `column` is one of `ids`, oldest first. `column` is a column name written in
/// this module, never input.
async fn comments_in<'e>(
    pool: impl sqlx::Executor<'e, Database = sqlx::Sqlite>,
    column: &str,
    ids: &[i64],
) -> Result<Vec<PullRequestCommentEntry>> {
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    let columns = codeyard_db_identity::sqlite::users::accounts::AUTHOR_DISPLAY_COLUMNS;
    let max_path_bytes = crate::pull_requests::reviews::MAX_COMMENT_PATH_BYTES;
    let mut query = sqlx::QueryBuilder::<sqlx::Sqlite>::new(format!(
        "SELECT i.id, i.pull_request_id, i.author_id, i.body,
            CASE WHEN length(CAST(i.path AS BLOB)) <= {max_path_bytes} THEN i.path END AS path,
            i.line, i.side, i.commit_sha, i.in_reply_to_comment_id, i.created_at, i.updated_at,
            i.resolved_at, i.resolved_by_id, i.review_id,
            i.start_line,i.start_side,{columns} FROM pr_comments i JOIN users u ON u.id = i.author_id
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
async fn reviews_with_ids(pool: &SqlitePool, ids: &[i64]) -> Result<Vec<PullRequestReviewEntry>> {
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    let columns = codeyard_db_identity::sqlite::users::accounts::AUTHOR_DISPLAY_COLUMNS;
    let mut query = sqlx::QueryBuilder::<sqlx::Sqlite>::new(format!(
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
    pool: &SqlitePool,
    pr_id: i64,
    limit: i64,
    offset: i64,
) -> Result<Vec<DiscussionItem>> {
    let keys: Vec<(i64, i64)> = sqlx::query_as(
        "SELECT items.kind, items.id FROM (
            SELECT 0 AS kind, c.id, c.created_at FROM pr_comments c
            WHERE c.pull_request_id = ?1 AND c.review_id IS NULL
            UNION ALL
            SELECT 1, v.id, v.created_at FROM pr_reviews v
            WHERE v.pull_request_id = ?1 AND v.state <> 'pending'
        ) items ORDER BY items.created_at, items.kind, items.id LIMIT ?2 OFFSET ?3",
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
/// Items compare by time, then kind, then id. The item's own time is read in the query, so the
/// comparison uses the stored text.
pub async fn count_discussion_items_through(
    pool: &SqlitePool,
    pr_id: i64,
    item: DiscussionRef,
) -> Result<i64> {
    let (kind, id): (i64, i64) = match item {
        DiscussionRef::Comment(id) => (0, id),
        DiscussionRef::Review(id) => (1, id),
    };
    Ok(sqlx::query_scalar(
        "WITH items(kind, id, created_at) AS (
            SELECT 0, c.id, c.created_at FROM pr_comments c
            WHERE c.pull_request_id = ?1 AND c.review_id IS NULL
            UNION ALL
            SELECT 1, v.id, v.created_at FROM pr_reviews v
            WHERE v.pull_request_id = ?1 AND v.state <> 'pending'
        ), target(created_at) AS (
            SELECT created_at FROM pr_comments WHERE ?2 = 0 AND id = ?3 AND pull_request_id = ?1
            UNION ALL
            SELECT created_at FROM pr_reviews WHERE ?2 = 1 AND id = ?3 AND pull_request_id = ?1
        )
        SELECT COUNT(*) FROM items, target
        WHERE items.created_at < target.created_at
           OR (items.created_at = target.created_at
               AND (items.kind < ?2 OR (items.kind = ?2 AND items.id <= ?3)))",
    )
    .bind(pr_id)
    .bind(kind)
    .bind(id)
    .fetch_one(pool)
    .await?)
}

/// The author's pending review on a pull request, if it has one.
pub async fn get_pending_review(
    pool: &SqlitePool,
    pr_id: i64,
    author_id: i64,
) -> Result<Option<PullRequestReview>> {
    let review = sqlx::query_as::<_, PullRequestReview>(
        "SELECT * FROM pr_reviews WHERE pull_request_id = ?1 AND author_id = ?2 AND state = 'pending'",
    )
    .bind(pr_id)
    .bind(author_id)
    .fetch_optional(pool)
    .await?;
    Ok(review)
}

/// Deletes the author's pending review. Its comments go with it, and the pull request's revision
/// moves only when there was a review to delete.
pub async fn discard_pending_review(pool: &SqlitePool, pr_id: i64, author_id: i64) -> Result<bool> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    crate::sqlite::discussions::lock_pull_request_contribution(&mut tx, author_id, pr_id).await?;
    let result = sqlx::query(
        "DELETE FROM pr_reviews WHERE pull_request_id = ?1 AND author_id = ?2 AND state = 'pending'",
    )
    .bind(pr_id)
    .bind(author_id)
    .execute(&mut *tx)
    .await?;
    let discarded = result.rows_affected() > 0;
    if discarded {
        touch_revision(&mut tx, pr_id).await?;
    }
    tx.commit().await?;
    Ok(discarded)
}

pub async fn pull_request_comment_counts(pool: &SqlitePool, pr_id: i64) -> Result<(i64, i64)> {
    Ok(sqlx::query_as("SELECT COALESCE(SUM(CASE WHEN c.path IS NULL THEN 1 ELSE 0 END),0), COALESCE(SUM(CASE WHEN c.path IS NOT NULL THEN 1 ELSE 0 END),0) FROM pr_comments c WHERE c.pull_request_id = ?1 AND NOT EXISTS (SELECT 1 FROM pr_reviews v WHERE v.id = c.review_id AND v.state = 'pending')")
        .bind(pr_id).fetch_one(pool).await?)
}

pub async fn count_pull_request_conversation_comments_for(
    pool: &SqlitePool,
    pr_ids: &[i64],
) -> Result<Vec<(i64, i64)>> {
    anyhow::ensure!(pr_ids.len() <= 500, "Pull request batch exceeds its limit");
    if pr_ids.is_empty() {
        return Ok(Vec::new());
    }
    Ok(sqlx::query_as(
        "SELECT c.pull_request_id, COUNT(*) FROM pr_comments c WHERE c.pull_request_id IN (SELECT value FROM json_each(?1)) AND c.path IS NULL
           AND NOT EXISTS (SELECT 1 FROM pr_reviews v WHERE v.id = c.review_id AND v.state = 'pending')
         GROUP BY c.pull_request_id",
    )
    .bind(serde_json::to_string(pr_ids)?)
    .fetch_all(pool)
    .await?)
}

pub async fn count_pull_request_comments(pool: &SqlitePool, pr_id: i64) -> Result<i64> {
    let count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM pr_comments i WHERE i.pull_request_id = ?1
           AND NOT EXISTS (SELECT 1 FROM pr_reviews v WHERE v.id = i.review_id AND v.state = 'pending')",
    )
    .bind(pr_id)
    .fetch_one(pool)
    .await?;
    Ok(count)
}

pub async fn count_pull_request_conversation_comments(
    pool: &SqlitePool,
    pr_id: i64,
) -> Result<i64> {
    let count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM pr_comments i WHERE i.pull_request_id = ?1 AND i.path IS NULL
           AND NOT EXISTS (SELECT 1 FROM pr_reviews v WHERE v.id = i.review_id AND v.state = 'pending')",
    )
    .bind(pr_id)
    .fetch_one(pool)
    .await?;
    Ok(count)
}

/// The comment count of each pull request in `pr_ids`. A pull request with no comments is absent.
pub async fn count_pull_request_comments_for(
    pool: &SqlitePool,
    pr_ids: &[i64],
) -> Result<Vec<(i64, i64)>> {
    debug_assert!(pr_ids.len() <= 500);
    if pr_ids.is_empty() {
        return Ok(Vec::new());
    }
    let mut query = sqlx::QueryBuilder::<sqlx::Sqlite>::new(
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
    pool: &SqlitePool,
    comment_id: i64,
) -> Result<Option<PullRequestComment>> {
    let comment =
        sqlx::query_as::<_, PullRequestComment>("SELECT * FROM pr_comments WHERE id = ?1")
            .bind(comment_id)
            .fetch_optional(pool)
            .await?;

    Ok(comment)
}

pub async fn get_visible_pull_request_comment(
    pool: &SqlitePool,
    comment_id: i64,
    viewer: i64,
) -> Result<Option<PullRequestComment>> {
    let comment = sqlx::query_as::<_, PullRequestComment>(
        "SELECT c.* FROM pr_comments c LEFT JOIN pr_reviews v ON v.id = c.review_id
         WHERE c.id = ?1 AND (c.review_id IS NULL
            OR (v.id IS NOT NULL AND (v.state <> 'pending' OR v.author_id = ?2)))",
    )
    .bind(comment_id)
    .bind(viewer)
    .fetch_optional(pool)
    .await?;
    Ok(comment)
}

/// Review conversations on a file that nobody has resolved: root comments with a path.
pub async fn count_unresolved_conversations(pool: &SqlitePool, pr_id: i64) -> Result<i64> {
    let count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM pr_comments
         WHERE pull_request_id = ?1 AND in_reply_to_comment_id IS NULL
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
    pool: &SqlitePool,
    pr_id: i64,
    comment_id: i64,
    resolved: bool,
    resolver_id: i64,
) -> Result<Option<PullRequestComment>> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    crate::sqlite::discussions::lock_comment_resolution_authority(&mut tx, resolver_id, pr_id)
        .await?;
    let comment = sqlx::query_as::<_, PullRequestComment>(
        "UPDATE pr_comments SET resolved_at = CASE WHEN ?3 THEN strftime('%Y-%m-%dT%H:%M:%f000Z','now') END, resolved_by_id = CASE WHEN ?3 THEN ?4 END
         WHERE id = ?1 AND pull_request_id = ?2
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
    pool: &SqlitePool,
    pr_id: i64,
    author_id: i64,
    credential: &codeyard_database::queries::users::accounts::AccountCredential<'_>,
    state: &str,
    body: Option<&str>,
    commit_sha: Option<&str>,
) -> Result<PullRequestReview> {
    save_pull_request_review(
        pool, pr_id, author_id, credential, state, body, commit_sha, None,
    )
    .await
}

#[expect(
    clippy::too_many_arguments,
    reason = "Each argument is one field or check of the saved review."
)]
pub async fn save_pull_request_review(
    pool: &SqlitePool,
    pr_id: i64,
    author_id: i64,
    credential: &codeyard_database::queries::users::accounts::AccountCredential<'_>,
    state: &str,
    body: Option<&str>,
    commit_sha: Option<&str>,
    expected_pending: Option<Option<i64>>,
) -> Result<PullRequestReview> {
    save_review_batch(
        pool,
        crate::pull_requests::reviews::ReviewBatch {
            pr_id,
            author_id,
            state: Some(state),
            body,
            commit_sha,
            expected_pending,
            create_pending: false,
            comments: &[],
            token: None,
            credential,
        },
    )
    .await
    .map(|(review, _)| review)
}

pub async fn save_review_batch(
    pool: &SqlitePool,
    input: crate::pull_requests::reviews::ReviewBatch<'_>,
) -> Result<(PullRequestReview, Vec<i64>)> {
    let (pr_id, author_id, state, body, commit_sha, expected_pending) = (
        input.pr_id,
        input.author_id,
        input.state.unwrap_or("commented"),
        input.body,
        input.commit_sha,
        input.expected_pending,
    );
    let commit_sha = commit_sha.map(str::to_ascii_lowercase);
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    // Any reader may review; the merge gate counts only writers' approvals.
    crate::sqlite::discussions::lock_pull_request_contribution(&mut tx, author_id, pr_id).await?;
    let (author, current_state, current_head) =
        crate::sqlite::discussions::read_pull_request_review_state(&mut tx, pr_id).await?;
    anyhow::ensure!(
        codeyard_database::queries::sqlite::users::transitions::credential_live(
            &mut tx,
            author_id,
            input.credential,
            true
        )
        .await?,
        codeyard_database::queries::discussions::DiscussionWriteRefusal::Denied
    );
    // `Some(None)`: the form saw no pending draft, so a draft that appeared since is not submitted unseen.
    if expected_pending == Some(None) {
        let draft: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM pr_reviews WHERE pull_request_id=?1 AND author_id=?2 AND state='pending')")
            .bind(pr_id).bind(author_id).fetch_one(&mut *tx).await?;
        anyhow::ensure!(
            !draft,
            codeyard_database::queries::discussions::DiscussionWriteRefusal::InvalidReview
        );
    }
    let pinned: bool = if let Some(Some(id)) = expected_pending {
        sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM pr_reviews WHERE id=?1 AND pull_request_id=?2 AND author_id=?3 AND state='pending' AND commit_sha=?4)")
            .bind(id).bind(pr_id).bind(author_id).bind(commit_sha.as_deref()).fetch_one(&mut *tx).await?
    } else {
        false
    };
    anyhow::ensure!(
        current_state == "open"
            && matches!(state, "approved" | "changes_requested" | "commented")
            && (pinned
                || (expected_pending.flatten().is_none()
                    && commit_sha.as_deref() == Some(current_head.as_str())))
            && (state == "commented" || author != author_id),
        codeyard_database::queries::discussions::DiscussionWriteRefusal::InvalidReview
    );
    // An Actions run may approve only where its repository or organization allows approvals.
    if state == "approved" {
        let bot: bool =
            sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM users WHERE id=?1 AND username=?2)")
                .bind(author_id)
                .bind(codeyard_database::queries::ci_tokens::BOT)
                .fetch_one(&mut *tx)
                .await?;
        if bot {
            let repo: i64 = sqlx::query_scalar("SELECT repo_id FROM pull_requests WHERE id=?1")
                .bind(pr_id)
                .fetch_one(&mut *tx)
                .await?;
            let (own, org) =
                codeyard_database::queries::sqlite::action_settings::snapshot(&mut tx, repo)
                    .await?;
            anyhow::ensure!(
                codeyard_database::queries::action_settings::can_approve_reviews(&own, &org),
                codeyard_database::queries::discussions::DiscussionWriteRefusal::Denied
            );
        }
    }
    let (repo, base): (i64, String) =
        sqlx::query_as("SELECT repo_id,base_sha FROM pull_requests WHERE id=?1")
            .bind(pr_id)
            .fetch_one(&mut *tx)
            .await?;
    if let Some(token) = input.token {
        anyhow::ensure!(
            codeyard_db_identity::sqlite::users::tokens::repository_write_on(
                &mut tx,
                token,
                author_id,
                repo,
                "pull-requests"
            )
            .await?,
            codeyard_database::queries::discussions::DiscussionWriteRefusal::Denied
        );
    }
    if input.create_pending {
        anyhow::ensure!(
            input.comments.len() <= MAX_REVIEW_COMMENTS as usize,
            codeyard_database::queries::discussions::DiscussionWriteRefusal::ReviewFull
        );
        let exists: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM pr_reviews WHERE pull_request_id=?1 AND author_id=?2 AND state='pending')").bind(pr_id).bind(author_id).fetch_one(&mut *tx).await?;
        anyhow::ensure!(
            !exists,
            codeyard_database::queries::discussions::DiscussionWriteRefusal::InvalidReview
        );
        let draft: PullRequestReview = sqlx::query_as("INSERT INTO pr_reviews(pull_request_id,author_id,state,body,commit_sha) VALUES(?1,?2,'pending',?3,?4) RETURNING *").bind(pr_id).bind(author_id).bind(body).bind(commit_sha.as_deref()).fetch_one(&mut *tx).await?;
        for comment in input.comments {
            anyhow::ensure!(
                comment.pr_id == pr_id
                    && comment.author_id == author_id
                    && comment.in_reply_to_comment_id.is_none()
                    && comment.path.is_some()
                    && comment.expected_revisions == Some((current_head.as_str(), base.as_str())),
                codeyard_database::queries::discussions::DiscussionWriteRefusal::RevisionChanged
            );
            insert_comment_on(&mut tx, comment, Some(draft.id)).await?;
        }
        if input.state.is_none() {
            touch_revision(&mut tx, pr_id).await?;
            tx.commit().await?;
            return Ok((draft, Vec::new()));
        }
    } else {
        anyhow::ensure!(
            input.comments.is_empty(),
            codeyard_database::queries::discussions::DiscussionWriteRefusal::InvalidReview
        );
    }
    let pending_body: Option<Option<String>> = sqlx::query_scalar(
        "SELECT body FROM pr_reviews WHERE pull_request_id=?1 AND author_id=?2 AND state='pending'",
    )
    .bind(pr_id)
    .bind(author_id)
    .fetch_optional(&mut *tx)
    .await?;
    let body = body.or_else(|| pending_body.as_ref().and_then(|body| body.as_deref()));
    // Resolve omitted text and validate the nonempty review under the same lock as submission.
    if state != "approved" && body.is_none_or(|body| body.trim().is_empty()) {
        let inline: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM pr_comments c JOIN pr_reviews v ON v.id = c.review_id
             WHERE v.pull_request_id=?1 AND v.author_id=?2 AND v.state='pending'",
        )
        .bind(pr_id)
        .bind(author_id)
        .fetch_one(&mut *tx)
        .await?;
        anyhow::ensure!(
            inline > 0,
            codeyard_database::queries::discussions::DiscussionWriteRefusal::EmptyReview
        );
    }
    // The author's pending review, if any, is submitted with its comments by this review. It keeps
    // the commit it was drafted on, so after a push it counts for that commit, never the new head.
    let submitted = sqlx::query_as::<_, PullRequestReview>(
        "UPDATE pr_reviews SET state = ?3, body = ?4, commit_sha = COALESCE(commit_sha, ?5),
            created_at = strftime('%Y-%m-%dT%H:%M:%f000Z','now')
         WHERE pull_request_id = ?1 AND author_id = ?2 AND state = 'pending'
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
                VALUES (?1, ?2, ?3, ?4, ?5)
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
    touch_revision(&mut tx, pr_id).await?;
    // Consuming a request belongs to the same admitted review transaction.
    if author_id != author {
        sqlx::query("DELETE FROM pr_team_reviewers WHERE pull_request_id=?1 AND team_id IN (SELECT m.team_id FROM effective_team_memberships m JOIN teams t ON t.id=m.team_id JOIN org_memberships o ON o.org_id=t.org_id AND o.user_id=m.user_id WHERE m.user_id=?2 AND m.role IN ('member','maintainer') AND o.role IN ('owner','admin','member'))")
            .bind(pr_id).bind(author_id).execute(&mut *tx).await?;
    }
    sqlx::query("DELETE FROM pr_reviewers WHERE pull_request_id=?1 AND user_id=?2")
        .bind(pr_id)
        .bind(author_id)
        .execute(&mut *tx)
        .await?;
    let published: Vec<i64> =
        sqlx::query_scalar("SELECT id FROM pr_comments WHERE review_id=?1 ORDER BY id")
            .bind(review.id)
            .fetch_all(&mut *tx)
            .await?;
    tx.commit().await?;

    Ok((review, published))
}

pub async fn create_pending_review(
    pool: &SqlitePool,
    pr_id: i64,
    author_id: i64,
    body: Option<&str>,
    commit_sha: &str,
) -> Result<PullRequestReview> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    crate::sqlite::discussions::lock_pull_request_contribution(&mut tx, author_id, pr_id).await?;
    let (_, state, head) =
        crate::sqlite::discussions::read_pull_request_review_state(&mut tx, pr_id).await?;
    let exists: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM pr_reviews WHERE pull_request_id=?1 AND author_id=?2 AND state='pending')").bind(pr_id).bind(author_id).fetch_one(&mut *tx).await?;
    anyhow::ensure!(
        state == "open" && head.eq_ignore_ascii_case(commit_sha) && !exists,
        codeyard_database::queries::discussions::DiscussionWriteRefusal::InvalidReview
    );
    let review = sqlx::query_as("INSERT INTO pr_reviews (pull_request_id,author_id,state,body,commit_sha) VALUES (?1,?2,'pending',?3,?4) RETURNING *")
        .bind(pr_id).bind(author_id).bind(body).bind(&head).fetch_one(&mut *tx).await?;
    touch_revision(&mut tx, pr_id).await?;
    tx.commit().await?;
    Ok(review)
}

pub async fn edit_review_body(
    pool: &SqlitePool,
    pr_id: i64,
    author_id: i64,
    review_id: i64,
    body: &str,
) -> Result<Option<PullRequestReview>> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    crate::sqlite::discussions::lock_pull_request_contribution(&mut tx, author_id, pr_id).await?;
    let review = sqlx::query_as("UPDATE pr_reviews SET body=?4 WHERE id=?1 AND pull_request_id=?2 AND author_id=?3 RETURNING *")
        .bind(review_id).bind(pr_id).bind(author_id).bind(body).fetch_optional(&mut *tx).await?;
    if review.is_some() {
        touch_revision(&mut tx, pr_id).await?;
    }
    tx.commit().await?;
    Ok(review)
}

pub async fn delete_pending_review_by_id(
    pool: &SqlitePool,
    pr_id: i64,
    author_id: i64,
    review_id: i64,
) -> Result<Option<PullRequestReview>> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    crate::sqlite::discussions::lock_pull_request_contribution(&mut tx, author_id, pr_id).await?;
    let review = sqlx::query_as("DELETE FROM pr_reviews WHERE id=?1 AND pull_request_id=?2 AND author_id=?3 AND state='pending' RETURNING *")
        .bind(review_id).bind(pr_id).bind(author_id).fetch_optional(&mut *tx).await?;
    if review.is_some() {
        touch_revision(&mut tx, pr_id).await?;
    }
    tx.commit().await?;
    Ok(review)
}

pub async fn list_visible_review_comments(
    pool: &SqlitePool,
    filter: &crate::pull_requests::reviews::ReviewCommentFilter,
) -> Result<Vec<PullRequestComment>> {
    Ok(sqlx::query_as("SELECT c.* FROM pr_comments c JOIN pull_requests p ON p.id=c.pull_request_id
        WHERE p.repo_id=?1 AND (?2 IS NULL OR p.id=?2) AND (?3 IS NULL OR c.review_id=?3) AND c.path IS NOT NULL
        AND NOT EXISTS(SELECT 1 FROM pr_reviews v WHERE v.id=c.review_id AND v.state='pending' AND (?4 IS NULL OR v.author_id<>?4))
        AND (?7 IS NULL OR c.updated_at>?7)
        ORDER BY CASE WHEN ?6 THEN CASE WHEN ?5 THEN c.updated_at ELSE c.created_at END END ASC,
        CASE WHEN NOT ?6 THEN CASE WHEN ?5 THEN c.updated_at ELSE c.created_at END END DESC,
        CASE WHEN ?6 THEN c.id END ASC, CASE WHEN NOT ?6 THEN c.id END DESC LIMIT ?8 OFFSET ?9")
        .bind(filter.repo_id).bind(filter.pull_id).bind(filter.review_id).bind(filter.viewer).bind(filter.updated).bind(filter.ascending)
        .bind(filter.since.map(codeyard_database::queries::sqlite::time::encode)).bind(filter.limit).bind(filter.offset).fetch_all(pool).await?)
}

pub async fn filter_reviews(
    pool: &SqlitePool,
    query: &crate::pull_requests::reviews::ReviewQuery,
    limit: i64,
    offset: i64,
) -> Result<Vec<PullRequestReview>> {
    Ok(sqlx::query_as(sqlx::AssertSqlSafe(
        crate::pull_requests::reviews::ReviewQuery::sql(false, false),
    ))
    .bind(query.pull_id)
    .bind(query.viewer)
    .bind(serde_json::to_string(&query.states)?)
    .bind(&query.author)
    .bind(query.latest)
    .bind(limit)
    .bind(offset)
    .fetch_all(pool)
    .await?)
}
pub async fn count_filtered_reviews(
    pool: &SqlitePool,
    query: &crate::pull_requests::reviews::ReviewQuery,
) -> Result<i64> {
    Ok(sqlx::query_scalar(sqlx::AssertSqlSafe(
        crate::pull_requests::reviews::ReviewQuery::sql(false, true),
    ))
    .bind(query.pull_id)
    .bind(query.viewer)
    .bind(serde_json::to_string(&query.states)?)
    .bind(&query.author)
    .bind(query.latest)
    .fetch_one(pool)
    .await?)
}
pub async fn list_visible_pull_request_reviews(
    pool: &SqlitePool,
    pr_id: i64,
    viewer: Option<i64>,
    limit: i64,
    offset: i64,
) -> Result<Vec<PullRequestReview>> {
    filter_reviews(
        pool,
        &crate::pull_requests::reviews::ReviewQuery {
            pull_id: pr_id,
            viewer,
            ..Default::default()
        },
        limit,
        offset,
    )
    .await
}

pub async fn list_pull_request_reviews(
    pool: &SqlitePool,
    pr_id: i64,
    limit: i64,
    offset: i64,
) -> Result<Vec<PullRequestReviewEntry>> {
    let columns = codeyard_db_identity::sqlite::users::accounts::AUTHOR_DISPLAY_COLUMNS;
    let reviews = sqlx::query_as::<_, PullRequestReviewEntry>(
        sqlx::AssertSqlSafe(format!("SELECT i.*, {columns} FROM pr_reviews i JOIN users u ON u.id=i.author_id WHERE i.pull_request_id = ?1 AND i.state <> 'pending' ORDER BY i.created_at, i.id LIMIT ?2 OFFSET ?3")),
    )
    .bind(pr_id)
    .bind(limit)
    .bind(offset)
    .fetch_all(pool)
    .await?;

    Ok(reviews)
}

pub async fn list_pull_request_review_decisions(
    pool: &SqlitePool,
    pr_id: i64,
) -> Result<Vec<PullRequestReviewDecision>> {
    let decisions = sqlx::query_as::<_, PullRequestReviewDecision>(
        r#"
        SELECT d.author_id, d.state, d.commit_sha, u.username FROM (
            SELECT author_id, state, commit_sha, created_at, id,
                ROW_NUMBER() OVER (PARTITION BY author_id ORDER BY created_at DESC, id DESC) AS latest
            FROM pr_reviews
            WHERE pull_request_id = ?1 AND state NOT IN ('commented', 'pending')
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
    pool: &SqlitePool,
    pr_ids: &[i64],
) -> Result<Vec<(i64, String, Option<String>)>> {
    debug_assert!(pr_ids.len() <= 500);
    if pr_ids.is_empty() {
        return Ok(Vec::new());
    }
    let mut query = sqlx::QueryBuilder::<sqlx::Sqlite>::new(
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
    pool: &SqlitePool,
    review_id: i64,
) -> Result<Option<PullRequestReview>> {
    let review = sqlx::query_as::<_, PullRequestReview>("SELECT * FROM pr_reviews WHERE id = ?1")
        .bind(review_id)
        .fetch_optional(pool)
        .await?;

    Ok(review)
}

pub async fn dismiss_pull_request_review(
    pool: &SqlitePool,
    review_id: i64,
    dismissed_by_id: i64,
    reason: Option<&str>,
) -> Result<PullRequestReview> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let pr_id = crate::sqlite::discussions::lock_review_dismissal_authority(
        &mut tx,
        dismissed_by_id,
        review_id,
    )
    .await?;
    let review = sqlx::query_as::<_, PullRequestReview>(
        r#"
        UPDATE pr_reviews
        SET state = 'dismissed', dismissed_by_id = ?1, dismissed_reason = ?2, dismissed_at = strftime('%Y-%m-%dT%H:%M:%f000Z','now')
        WHERE id = ?3 AND pull_request_id = ?4 AND state IN ('approved', 'changes_requested')
        RETURNING *
        "#,
    )
    .bind(dismissed_by_id)
    .bind(reason)
    .bind(review_id)
    .bind(pr_id)
    .fetch_one(&mut *tx)
    .await?;
    touch_revision(&mut tx, review.pull_request_id).await?;
    tx.commit().await?;

    Ok(review)
}

async fn touch_revision(tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>, pr_id: i64) -> Result<()> {
    let stored: String = sqlx::query_scalar("SELECT updated_at FROM pull_requests WHERE id=?1")
        .bind(pr_id)
        .fetch_one(&mut **tx)
        .await?;
    let next = codeyard_database::queries::sqlite::time::next_revision(
        codeyard_database::queries::sqlite::time::decode(&stored)?,
    )?;
    sqlx::query("UPDATE pull_requests SET updated_at=?2 WHERE id=?1 AND updated_at=?3")
        .bind(pr_id)
        .bind(next)
        .bind(stored)
        .execute(&mut **tx)
        .await?;
    Ok(())
}

pub async fn request_pull_request_reviewer(
    pool: &SqlitePool,
    pr_id: i64,
    actor_id: i64,
    user_id: i64,
) -> Result<bool> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let (author, state, _) =
        crate::sqlite::discussions::lock_pull_request_review_authority(&mut tx, actor_id, pr_id)
            .await?;
    anyhow::ensure!(
        state == "open",
        codeyard_database::queries::discussions::DiscussionWriteRefusal::InvalidReview
    );
    let repo_id: i64 = sqlx::query_scalar("SELECT repo_id FROM pull_requests WHERE id=?1")
        .bind(pr_id)
        .fetch_one(&mut *tx)
        .await?;
    // A request must not disclose a private discussion to an ineligible target.
    anyhow::ensure!(
        user_id != author
            && codeyard_database::queries::sqlite::discussions::lock_write_authority(
                &mut tx, user_id, repo_id
            )
            .await?,
        AUTHORITY_DENIED
    );
    let result = sqlx::query(
        "INSERT INTO pr_reviewers (pull_request_id, user_id) VALUES (?1, ?2) ON CONFLICT DO NOTHING",
    )
    .bind(pr_id)
    .bind(user_id)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(result.rows_affected() > 0)
}

/// Only the CODEOWNERS resolver calls this; manual requests use writer authority above.
pub async fn request_code_owner_reviews(
    pool: &SqlitePool,
    pull: &PullRequest,
    actor_id: i64,
    owners: &[i64],
) -> Result<Vec<User>> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    crate::sqlite::discussions::require_contribution_authority(&mut tx, actor_id, pull.repo_id)
        .await?;
    let current: Option<PullRequest> =
        sqlx::query_as("SELECT * FROM pull_requests WHERE id = ?1 AND repo_id = ?2")
            .bind(pull.id)
            .bind(pull.repo_id)
            .fetch_optional(&mut *tx)
            .await?;
    let Some(current) = current.filter(|current| {
        current.state == "open"
            && !current.is_draft
            && current.head_sha == pull.head_sha
            && current.base_sha == pull.base_sha
    }) else {
        return Ok(Vec::new());
    };
    anyhow::ensure!(
        owners.len() <= codeyard_types::repositories::MAX_REVIEWERS,
        "Too many code owners"
    );
    let access = codeyard_database::queries::sqlite::repositories::access::USER_REPO_ACCESS_SQL;
    let added: Vec<i64> = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
        "INSERT INTO pr_reviewers (pull_request_id, user_id, as_code_owner)
         SELECT ?1, u.id, 1 FROM repositories r CROSS JOIN users u
         WHERE r.id=?2 AND u.id IN (SELECT value FROM json_each(?3)) AND u.id<>?4
           AND NOT r.is_archived AND NOT u.is_banned AND NOT u.must_change_password
           AND ((r.owner_kind='user' AND u.is_admin) OR ({access})>=3)
           AND NOT COALESCE((SELECT state IN ('approved','changes_requested') AND commit_sha=?5 FROM pr_reviews WHERE pull_request_id=?1 AND author_id=u.id AND state NOT IN ('commented','pending') ORDER BY created_at DESC,id DESC LIMIT 1),FALSE)
         ON CONFLICT DO NOTHING RETURNING user_id"
    ))).bind(pull.id).bind(pull.repo_id).bind(serde_json::to_string(owners)?).bind(current.author_id).bind(&current.head_sha)
        .fetch_all(&mut *tx).await?;
    let users = sqlx::query_as::<_, User>(
        "SELECT * FROM users WHERE id IN (SELECT value FROM json_each(?1))",
    )
    .bind(serde_json::to_string(&added)?)
    .fetch_all(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(users)
}

pub async fn remove_pull_request_reviewer(
    pool: &SqlitePool,
    pr_id: i64,
    actor_id: i64,
    user_id: i64,
) -> Result<bool> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    crate::sqlite::discussions::lock_pull_request_review_authority(&mut tx, actor_id, pr_id)
        .await?;
    let result =
        sqlx::query("DELETE FROM pr_reviewers WHERE pull_request_id = ?1 AND user_id = ?2")
            .bind(pr_id)
            .bind(user_id)
            .execute(&mut *tx)
            .await?;
    tx.commit().await?;
    Ok(result.rows_affected() > 0)
}

pub async fn review_request_origins(
    pool: &SqlitePool,
    pull: i64,
) -> Result<Vec<crate::pull_requests::reviews::ReviewRequestOrigin>> {
    Ok(sqlx::query_as("SELECT id,user_id,team_id,as_code_owner FROM (SELECT r.id,r.user_id,NULL AS team_id,r.as_code_owner,0 AS kind,u.username AS sort_name FROM pr_reviewers r JOIN users u ON u.id=r.user_id WHERE r.pull_request_id=?1 UNION ALL SELECT r.id,NULL,r.team_id,r.as_code_owner,1,t.name FROM pr_team_reviewers r JOIN teams t ON t.id=r.team_id WHERE r.pull_request_id=?1) requests ORDER BY kind,sort_name,COALESCE(user_id,team_id)").bind(pull).fetch_all(pool).await?)
}

pub async fn list_pull_request_reviewers(pool: &SqlitePool, pr_id: i64) -> Result<Vec<User>> {
    let users = sqlx::query_as::<_, User>(
        r#"
        SELECT u.* FROM users u
        INNER JOIN pr_reviewers r ON r.user_id = u.id
        WHERE r.pull_request_id = ?1
        ORDER BY u.username
        "#,
    )
    .bind(pr_id)
    .fetch_all(pool)
    .await?;

    Ok(users)
}

pub async fn converted_issue_comment(
    pool: &SqlitePool,
    pr_id: i64,
    source_comment: i64,
) -> Result<Option<i64>> {
    Ok(sqlx::query_scalar(
        "SELECT id FROM pr_comments WHERE pull_request_id=?1 AND source_issue_comment_id=?2",
    )
    .bind(pr_id)
    .bind(source_comment)
    .fetch_optional(pool)
    .await?)
}

impl<'r> sqlx::FromRow<'r, sqlx::sqlite::SqliteRow> for PullRequestCommentEntry {
    fn from_row(row: &'r sqlx::sqlite::SqliteRow) -> Result<Self, sqlx::Error> {
        Ok(Self {
            comment: PullRequestComment::from_row(row)?,
            author: codeyard_db_identity::sqlite::users::accounts::author_display(row)?,
        })
    }
}

impl<'r> sqlx::FromRow<'r, sqlx::sqlite::SqliteRow> for PullRequestReviewEntry {
    fn from_row(row: &'r sqlx::sqlite::SqliteRow) -> Result<Self, sqlx::Error> {
        Ok(Self {
            review: PullRequestReview::from_row(row)?,
            author: codeyard_db_identity::sqlite::users::accounts::author_display(row)?,
        })
    }
}

async fn private_pending_comment(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    comment: i64,
    actor: i64,
) -> Result<bool> {
    Ok(sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM pr_comments c JOIN pr_reviews v ON v.id=c.review_id WHERE c.id=?1 AND v.state='pending' AND v.author_id<>?2)")
        .bind(comment).bind(actor).fetch_one(&mut **tx).await?)
}

/// The scoped comment author; review privacy is checked separately before mutation.
async fn scoped_comment_author(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    repo_id: i64,
    pr_id: i64,
    comment_id: i64,
    writer: bool,
) -> Result<Option<i64>> {
    Ok(sqlx::query_scalar(
        "SELECT c.author_id FROM pr_comments c JOIN pull_requests p ON p.id = c.pull_request_id
         WHERE c.id = ?1 AND c.pull_request_id = ?2 AND p.repo_id = ?3 AND (NOT p.locked OR ?4)",
    )
    .bind(comment_id)
    .bind(pr_id)
    .bind(repo_id)
    .bind(writer)
    .fetch_optional(&mut **tx)
    .await?)
}

/// The author or a live repository writer may edit a visible comment.
pub async fn edit_pull_request_comment(
    pool: &SqlitePool,
    actor_id: i64,
    repo_id: i64,
    pr_id: i64,
    comment_id: i64,
    body: &str,
    expected_body: &str,
) -> Result<Option<PullRequestComment>> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    if !crate::sqlite::discussions::lock_contribution_authority(&mut tx, actor_id, repo_id).await? {
        return Ok(None);
    }
    let writer = codeyard_database::queries::sqlite::discussions::lock_write_authority(
        &mut tx, actor_id, repo_id,
    )
    .await?;
    let author = scoped_comment_author(&mut tx, repo_id, pr_id, comment_id, writer).await?;
    if author.is_none()
        || (author != Some(actor_id) && !writer)
        || private_pending_comment(&mut tx, comment_id, actor_id).await?
    {
        return Ok(None);
    }
    let comment = sqlx::query_as::<_, PullRequestComment>(
        "UPDATE pr_comments SET body = ?2, updated_at = strftime('%Y-%m-%dT%H:%M:%f000Z','now')
         WHERE id = ?1 AND body = ?3 RETURNING *",
    )
    .bind(comment_id)
    .bind(body)
    .bind(expected_body)
    .fetch_optional(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(comment)
}

/// Deletes a conversation comment with the notifications that link to it. The author may delete
/// their own comment; a repository writer may delete anyone's visible comment.
pub async fn delete_pull_request_comment(
    pool: &SqlitePool,
    actor_id: i64,
    repo_id: i64,
    pr_id: i64,
    comment_id: i64,
) -> Result<bool> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    if !crate::sqlite::discussions::lock_contribution_authority(&mut tx, actor_id, repo_id).await? {
        return Ok(false);
    }
    let writer = codeyard_database::queries::sqlite::discussions::lock_write_authority(
        &mut tx, actor_id, repo_id,
    )
    .await?;
    let Some(author) = scoped_comment_author(&mut tx, repo_id, pr_id, comment_id, writer).await?
    else {
        return Ok(false);
    };
    if (author != actor_id && !writer)
        || private_pending_comment(&mut tx, comment_id, actor_id).await?
    {
        return Ok(false);
    }
    // Replies are removed with their parent, so a comment with replies stays.
    let has_replies: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM pr_comments WHERE in_reply_to_comment_id = ?1)",
    )
    .bind(comment_id)
    .fetch_one(&mut *tx)
    .await?;
    if has_replies {
        return Ok(false);
    }
    sqlx::query(
        "DELETE FROM notifications WHERE repo_id = ?1
         AND ((kind IN ('pull_request_comment', 'pull_request_comment_reply')
               AND json_extract(metadata, '$.comment_id') = ?2)
              OR (kind = 'mention' AND json_extract(metadata, '$.pull_request_comment_id') = ?2))",
    )
    .bind(repo_id)
    .bind(comment_id)
    .execute(&mut *tx)
    .await?;
    sqlx::query("DELETE FROM reactions WHERE subject_kind = 'pr_comment' AND subject_id = ?1")
        .bind(comment_id)
        .execute(&mut *tx)
        .await?;
    let removed = sqlx::query("DELETE FROM pr_comments WHERE id = ?1")
        .bind(comment_id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(removed.rows_affected() == 1)
}
