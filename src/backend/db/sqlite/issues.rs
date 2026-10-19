use super::models::{Issue, IssueComment};
use crate::backend::db::discussions::{contains_pattern, ListFilter, StateCounts};
use crate::backend::db::issues::IssueCommentEntry;
use crate::backend::db::issues::IssueListEntry;
use crate::backend::db::issues::{IssueRelation, ViewerIssueEntry};
use anyhow::Result;
use sqlx::SqlitePool;

pub async fn create_issue(
    pool: &SqlitePool,
    repo_id: i64,
    author_id: i64,
    title: &str,
    body: &str,
) -> Result<Issue> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    super::discussions::require_contribution_authority(&mut tx, author_id, repo_id).await?;
    let number = super::discussions::allocate_number(&mut tx, repo_id).await?;
    let issue = sqlx::query_as::<_, Issue>(
        r#"
        INSERT INTO issues (repo_id, number, author_id, title, body)
        VALUES (?1, ?5, ?2, ?3, ?4)
        RETURNING *
        "#,
    )
    .bind(repo_id)
    .bind(author_id)
    .bind(title)
    .bind(body)
    .bind(number)
    .fetch_one(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(issue)
}

pub async fn get_issue(pool: &SqlitePool, repo_id: i64, number: i32) -> Result<Option<Issue>> {
    let issue =
        sqlx::query_as::<_, Issue>("SELECT * FROM issues WHERE repo_id = ?1 AND number = ?2")
            .bind(repo_id)
            .bind(number)
            .fetch_optional(pool)
            .await?;

    Ok(issue)
}

pub async fn get_issue_by_id(pool: &SqlitePool, id: i64) -> Result<Option<Issue>> {
    let issue = sqlx::query_as::<_, Issue>("SELECT * FROM issues WHERE id = ?1")
        .bind(id)
        .fetch_optional(pool)
        .await?;

    Ok(issue)
}

pub async fn list_repo_issues(
    pool: &SqlitePool,
    repo_id: i64,
    filter: &ListFilter<'_>,
    limit: i64,
) -> Result<Vec<IssueListEntry>> {
    let columns = crate::backend::db::sqlite::users::accounts::AUTHOR_DISPLAY_COLUMNS;
    anyhow::ensure!(limit >= 0, "Result limit must be nonnegative");
    let (assignee_mode, assignee_id) = filter.assignee.binding();
    let (milestone_mode, milestone_id) = filter.milestone.binding();
    let labels = super::discussions::label_names_json(filter.labels)?;
    let order = filter.sort.order_by("issue_comments", "issue_id", "");
    let issues = sqlx::query_as::<_, IssueListEntry>(&format!(
        r"SELECT i.*, {columns} FROM issues i JOIN users u ON u.id=i.author_id
        WHERE i.repo_id = ?1 AND (?2 IS NULL OR i.state = ?2)
            AND (?3 IS NULL OR i.title LIKE ?3 ESCAPE '\' OR i.body LIKE ?3 ESCAPE '\')
            AND (?5 IS NULL OR i.author_id = ?5)
            AND (?6 = 0 OR (?6 = 1 AND NOT EXISTS (SELECT 1 FROM issue_assignees a WHERE a.issue_id = i.id))
                OR (?6 = 2 AND EXISTS (SELECT 1 FROM issue_assignees a WHERE a.issue_id = i.id AND a.user_id = ?7)))
            AND (?8 = 0 OR (?8 = 1 AND i.milestone_id IS NULL) OR (?8 = 2 AND i.milestone_id = ?9))
            AND (?10 IS NULL OR (SELECT COUNT(DISTINCT LOWER(l.name)) FROM issue_labels il JOIN labels l ON l.id=il.label_id
                WHERE il.issue_id = i.id AND LOWER(l.name) IN (SELECT LOWER(value) FROM json_each(?10)))
                = (SELECT COUNT(DISTINCT LOWER(value)) FROM json_each(?10)))
        ORDER BY {order} LIMIT ?4"
    ))
    .bind(repo_id)
    .bind(filter.state)
    .bind(filter.search.map(contains_pattern))
    .bind(limit)
    .bind(filter.author_id)
    .bind(assignee_mode)
    .bind(assignee_id)
    .bind(milestone_mode)
    .bind(milestone_id)
    .bind(labels)
    .fetch_all(pool)
    .await?;

    Ok(issues)
}

pub async fn count_repo_issues(
    pool: &SqlitePool,
    repo_id: i64,
    search: Option<&str>,
) -> Result<StateCounts> {
    let rows = sqlx::query_as::<_, (String, i64)>(
        r"SELECT i.state, COUNT(*) FROM issues i WHERE i.repo_id = ?1
            AND (?2 IS NULL OR i.title LIKE ?2 ESCAPE '\' OR i.body LIKE ?2 ESCAPE '\')
        GROUP BY i.state",
    )
    .bind(repo_id)
    .bind(search.map(contains_pattern))
    .fetch_all(pool)
    .await?;
    Ok(StateCounts::from_rows(rows))
}

pub async fn update_issue(
    pool: &SqlitePool,
    issue_id: i64,
    edit: crate::backend::db::discussions::DiscussionEdit<'_>,
) -> Result<Option<Issue>> {
    if !edit.has_exact_revision() {
        return Ok(None);
    }
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let repo: Option<i64> = sqlx::query_scalar("SELECT repo_id FROM issues WHERE id = ?1")
        .bind(issue_id)
        .fetch_optional(&mut *tx)
        .await?;
    let Some(repo) = repo else {
        return Ok(None);
    };
    if !super::discussions::lock_write_authority(&mut tx, edit.actor_id, repo).await? {
        return Ok(None);
    }
    let stored: Option<(String, String)> =
        sqlx::query_as("SELECT state,updated_at FROM issues WHERE id=?1")
            .bind(issue_id)
            .fetch_optional(&mut *tx)
            .await?;
    let Some((state, stored_revision)) = stored else {
        return Ok(None);
    };
    let previous = super::time::decode(&stored_revision)?;
    if state != edit.expected_state || previous != edit.expected_updated {
        return Ok(None);
    }
    let next = super::time::next_revision(previous)?;
    let issue = sqlx::query_as::<_, Issue>(
        r#"UPDATE issues SET
            title = COALESCE(?2, title), body = COALESCE(?3, body),
            state = COALESCE(?4, state),
            closed_at = CASE WHEN ?4 = 'open' THEN NULL WHEN ?4 = 'closed' AND state <> 'closed' THEN strftime('%Y-%m-%dT%H:%M:%f000Z','now') ELSE closed_at END,
            closed_by_id = CASE WHEN ?4 = 'open' THEN NULL WHEN ?4 = 'closed' AND state <> 'closed' THEN ?7 ELSE closed_by_id END,
            updated_at = ?8
        WHERE id = ?1 AND state = ?5 AND updated_at = ?6 AND repo_id = ?9
            AND (?4 IS NULL OR ?4 IN ('open', 'closed'))
        RETURNING *"#,
    ).bind(issue_id).bind(edit.title).bind(edit.body).bind(edit.state)
        .bind(edit.expected_state).bind(stored_revision).bind(edit.actor_id).bind(next).bind(repo)
        .fetch_optional(&mut *tx).await?;
    tx.commit().await?;
    Ok(issue)
}

pub async fn add_issue_comment(
    pool: &SqlitePool,
    issue_id: i64,
    author_id: i64,
    body: &str,
) -> Result<IssueComment> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    super::discussions::lock_issue_contribution(&mut tx, author_id, issue_id).await?;
    let revision: String = sqlx::query_scalar("SELECT updated_at FROM issues WHERE id=?1")
        .bind(issue_id)
        .fetch_one(&mut *tx)
        .await?;
    let next = super::time::next_revision(super::time::decode(&revision)?)?;
    let comment = sqlx::query_as::<_, IssueComment>(
        r#"
        INSERT INTO issue_comments (issue_id, author_id, body)
        VALUES (?1, ?2, ?3)
        RETURNING *
        "#,
    )
    .bind(issue_id)
    .bind(author_id)
    .bind(body)
    .fetch_one(&mut *tx)
    .await?;

    sqlx::query("UPDATE issues SET updated_at=?2 WHERE id=?1 AND updated_at=?3")
        .bind(issue_id)
        .bind(next)
        .bind(revision)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(comment)
}

pub async fn list_issue_comments(
    pool: &SqlitePool,
    issue_id: i64,
    limit: i64,
    offset: i64,
) -> Result<Vec<IssueCommentEntry>> {
    let columns = crate::backend::db::sqlite::users::accounts::AUTHOR_DISPLAY_COLUMNS;
    let comments = sqlx::query_as::<_, IssueCommentEntry>(
        &format!("SELECT i.*, {columns} FROM issue_comments i JOIN users u ON u.id=i.author_id WHERE i.issue_id = ?1 ORDER BY i.created_at, i.id LIMIT ?2 OFFSET ?3"),
    )
    .bind(issue_id)
    .bind(limit)
    .bind(offset)
    .fetch_all(pool)
    .await?;

    Ok(comments)
}

/// One-based position in the same (created_at, id) order as the discussion list.
pub async fn issue_comment_position(pool: &SqlitePool, comment_id: i64) -> Result<i64> {
    Ok(sqlx::query_scalar(
        "SELECT COUNT(*) FROM issue_comments i JOIN issue_comments target ON target.issue_id=i.issue_id WHERE target.id=?1 AND (i.created_at, i.id) <= (target.created_at, target.id)",
    )
    .bind(comment_id)
    .fetch_one(pool)
    .await?)
}

/// Comment counts for one page of issues. Callers pass at most a page of ids.
pub async fn count_issue_comments(pool: &SqlitePool, issue_ids: &[i64]) -> Result<Vec<(i64, i64)>> {
    debug_assert!(issue_ids.len() <= 500);
    if issue_ids.is_empty() {
        return Ok(Vec::new());
    }
    let mut query = sqlx::QueryBuilder::<sqlx::Sqlite>::new(
        "SELECT issue_id, COUNT(*) FROM issue_comments WHERE issue_id IN (",
    );
    let mut ids = query.separated(",");
    for id in issue_ids {
        ids.push_bind(id);
    }
    query.push(") GROUP BY issue_id");
    Ok(query.build_query_as().fetch_all(pool).await?)
}

/// The condition that picks a viewer's issues. `?1` is the viewer.
fn viewer_issue_sql(relation: IssueRelation) -> &'static str {
    match relation {
        IssueRelation::Assigned => {
            "EXISTS (SELECT 1 FROM issue_assignees a WHERE a.issue_id = i.id AND a.user_id = ?1)"
        }
        IssueRelation::Created => "i.author_id = ?1",
    }
}

/// The issues in repositories `viewer_id` can read that the viewer was assigned or opened. The
/// visibility rule is the access projection the repository pages use, applied before the limit so
/// hidden rows do not take up a page.
pub async fn list_viewer_issues(
    pool: &SqlitePool,
    viewer_id: i64,
    relation: IssueRelation,
    open: bool,
    limit: i64,
) -> Result<Vec<ViewerIssueEntry>> {
    anyhow::ensure!(limit >= 0, "Result limit must be nonnegative");
    let access = super::repositories::access::USER_REPO_ACCESS_SQL;
    let relation_sql = viewer_issue_sql(relation);
    let issues = sqlx::query_as::<_, ViewerIssueEntry>(&format!(
        r"SELECT i.*, r.name AS repo_name, COALESCE(ou.username, og.name) AS repo_owner
        FROM issues i
        JOIN repositories r ON r.id = i.repo_id
        LEFT JOIN users ou ON ou.id = r.owner_user_id
        LEFT JOIN organizations og ON og.id = r.owner_org_id
        JOIN users u ON u.id = ?1
        WHERE (i.state = 'open') = ?2 AND {relation_sql}
            AND NOT u.is_banned AND NOT u.must_change_password
            AND ((r.owner_kind = 'user' AND u.is_admin) OR ({access}) >= 1)
        ORDER BY i.created_at DESC, i.id DESC LIMIT ?3"
    ))
    .bind(viewer_id)
    .bind(open)
    .bind(limit)
    .fetch_all(pool)
    .await?;
    Ok(issues)
}

/// Open and closed counts of the viewer's issues, with the same visibility as the list.
pub async fn count_viewer_issues(
    pool: &SqlitePool,
    viewer_id: i64,
    relation: IssueRelation,
) -> Result<StateCounts> {
    let access = super::repositories::access::USER_REPO_ACCESS_SQL;
    let relation_sql = viewer_issue_sql(relation);
    let rows = sqlx::query_as::<_, (String, i64)>(&format!(
        r"SELECT CASE WHEN i.state = 'open' THEN 'open' ELSE 'closed' END AS bucket, COUNT(*)
        FROM issues i
        JOIN repositories r ON r.id = i.repo_id
        JOIN users u ON u.id = ?1
        WHERE {relation_sql} AND NOT u.is_banned AND NOT u.must_change_password
            AND ((r.owner_kind = 'user' AND u.is_admin) OR ({access}) >= 1)
        GROUP BY bucket"
    ))
    .bind(viewer_id)
    .fetch_all(pool)
    .await?;
    Ok(StateCounts::from_rows(rows))
}

impl<'r> sqlx::FromRow<'r, sqlx::sqlite::SqliteRow> for ViewerIssueEntry {
    fn from_row(row: &'r sqlx::sqlite::SqliteRow) -> Result<Self, sqlx::Error> {
        use sqlx::Row;
        Ok(Self {
            issue: Issue::from_row(row)?,
            repo_owner: row.try_get("repo_owner")?,
            repo_name: row.try_get("repo_name")?,
        })
    }
}

impl<'r> sqlx::FromRow<'r, sqlx::sqlite::SqliteRow> for IssueListEntry {
    fn from_row(row: &'r sqlx::sqlite::SqliteRow) -> Result<Self, sqlx::Error> {
        Ok(Self {
            issue: Issue::from_row(row)?,
            author: crate::backend::db::sqlite::users::accounts::author_display(row)?,
        })
    }
}

impl<'r> sqlx::FromRow<'r, sqlx::sqlite::SqliteRow> for IssueCommentEntry {
    fn from_row(row: &'r sqlx::sqlite::SqliteRow) -> Result<Self, sqlx::Error> {
        Ok(Self {
            comment: IssueComment::from_row(row)?,
            author: crate::backend::db::sqlite::users::accounts::author_display(row)?,
        })
    }
}

/// Changes the body of a comment the actor wrote. Refused (None) when the actor is not the author,
/// the thread is locked, or the actor no longer may take part in the repository.
pub async fn edit_issue_comment(
    pool: &SqlitePool,
    actor_id: i64,
    repo_id: i64,
    issue_id: i64,
    comment_id: i64,
    body: &str,
    expected_body: &str,
) -> Result<Option<IssueComment>> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let author: Option<i64> = sqlx::query_scalar(
        "SELECT c.author_id FROM issue_comments c JOIN issues i ON i.id = c.issue_id
         WHERE c.id = ?1 AND c.issue_id = ?2 AND i.repo_id = ?3 AND NOT i.locked",
    )
    .bind(comment_id)
    .bind(issue_id)
    .bind(repo_id)
    .fetch_optional(&mut *tx)
    .await?;
    if author != Some(actor_id)
        || !super::discussions::lock_contribution_authority(&mut tx, actor_id, repo_id).await?
    {
        return Ok(None);
    }
    let comment = sqlx::query_as::<_, IssueComment>(
        "UPDATE issue_comments SET body = ?2, updated_at = strftime('%Y-%m-%dT%H:%M:%f000Z','now')
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

/// Deletes a comment with the notifications that link to it. The author may delete their own
/// comment; a repository maintainer may delete anyone's. Returns false when the delete is refused.
pub async fn delete_issue_comment(
    pool: &SqlitePool,
    actor_id: i64,
    repo_id: i64,
    issue_id: i64,
    comment_id: i64,
) -> Result<bool> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let author: Option<i64> = sqlx::query_scalar(
        "SELECT c.author_id FROM issue_comments c JOIN issues i ON i.id = c.issue_id
         WHERE c.id = ?1 AND c.issue_id = ?2 AND i.repo_id = ?3 AND NOT i.locked",
    )
    .bind(comment_id)
    .bind(issue_id)
    .bind(repo_id)
    .fetch_optional(&mut *tx)
    .await?;
    let Some(author) = author else {
        return Ok(false);
    };
    let allowed = if author == actor_id {
        super::discussions::lock_contribution_authority(&mut tx, actor_id, repo_id).await?
    } else {
        super::discussions::lock_maintain_authority(&mut tx, actor_id, repo_id).await?
    };
    if !allowed {
        return Ok(false);
    }
    sqlx::query(
        "DELETE FROM notifications WHERE repo_id = ?1 AND kind IN ('issue_comment', 'mention')
         AND json_extract(metadata, '$.comment_id') = ?2",
    )
    .bind(repo_id)
    .bind(comment_id)
    .execute(&mut *tx)
    .await?;
    let removed = sqlx::query("DELETE FROM issue_comments WHERE id = ?1")
        .bind(comment_id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(removed.rows_affected() == 1)
}

pub async fn get_issue_comment(pool: &SqlitePool, comment_id: i64) -> Result<Option<IssueComment>> {
    Ok(
        sqlx::query_as::<_, IssueComment>("SELECT * FROM issue_comments WHERE id = ?1")
            .bind(comment_id)
            .fetch_optional(pool)
            .await?,
    )
}
