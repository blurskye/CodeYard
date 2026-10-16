use super::models::{Issue, IssueComment};
use crate::backend::db::discussions::{contains_pattern, ListFilter, StateCounts};
use crate::backend::db::issues::IssueCommentEntry;
use crate::backend::db::issues::IssueListEntry;
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
    let order = filter.sort.order_by("issue_comments", "issue_id");
    let issues = sqlx::query_as::<_, IssueListEntry>(&format!(
        r"SELECT i.*, {columns} FROM issues i LEFT JOIN users u ON u.id=i.author_id
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
        &format!("SELECT i.*, {columns} FROM issue_comments i LEFT JOIN users u ON u.id=i.author_id WHERE i.issue_id = ?1 ORDER BY i.created_at, i.id LIMIT ?2 OFFSET ?3"),
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
