use crate::backend::db::discussions::{ListFilter, StateCounts, SHOWN_PR_COMMENT};
use crate::backend::db::models::PullRequest;
use crate::backend::db::pull_requests::records::PullRequestListEntry;
use crate::backend::db::pull_requests::records::{
    can_grant_maintainer_edit, decide_join, DuplicateOpenPullRequest, JoinCheck, NewPullRequest,
    PullRequestRevisions, PullRequestUpdate, ReadyOutcome, StackJoin, StackRefusal,
};
use crate::backend::db::pull_requests::records::{PullRequestRelation, ViewerPullRequestEntry};
use anyhow::Result;
use sqlx::SqlitePool;

pub async fn commit_association_candidates(
    pool: &SqlitePool,
    repo: i64,
    before: i64,
    limit: i64,
) -> Result<Vec<PullRequest>> {
    anyhow::ensure!(
        (1..=512).contains(&limit),
        "Pull request association batch exceeds its limit"
    );
    Ok(sqlx::query_as("SELECT * FROM pull_requests WHERE repo_id=?1 AND id<?2 AND (state='open' OR merged) ORDER BY id DESC LIMIT ?3")
        .bind(repo).bind(before).bind(limit).fetch_all(pool).await?)
}

/// The branches and commits that the open pull requests of a repository record.
pub async fn list_open_pull_request_revisions(
    pool: &SqlitePool,
    repo_id: i64,
) -> Result<Vec<PullRequestRevisions>> {
    let rows = sqlx::query_as::<_, PullRequestRevisions>(
        "SELECT id, repo_id, number, author_id, head_publisher_id, head_repo_id, head_branch, head_sha, base_branch, base_sha, stack_id FROM pull_requests WHERE (repo_id = ?1 OR head_repo_id = ?1) AND state = 'open' ORDER BY id",
    )
    .bind(repo_id)
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

/// The open pull requests that use one of `branches` in this repository: the ones whose
/// base branch here is one of them, and the ones whose head branch here is one of them.
pub async fn list_open_pull_request_revisions_on_branches(
    pool: &SqlitePool,
    repo_id: i64,
    branches: &[&str],
) -> Result<Vec<PullRequestRevisions>> {
    let mut query = sqlx::QueryBuilder::<sqlx::Sqlite>::new(
        "SELECT id, repo_id, number, author_id, head_publisher_id, head_repo_id, head_branch, head_sha, base_branch, base_sha, stack_id FROM pull_requests WHERE state = 'open' AND ((repo_id = ",
    );
    query.push_bind(repo_id).push(" AND base_branch IN (");
    let mut names = query.separated(",");
    for branch in branches {
        names.push_bind(*branch);
    }
    query
        .push(")) OR (head_repo_id = ")
        .push_bind(repo_id)
        .push(" AND head_branch IN (");
    let mut names = query.separated(",");
    for branch in branches {
        names.push_bind(*branch);
    }
    query.push("))) ORDER BY id");
    Ok(query.build_query_as().fetch_all(pool).await?)
}

/// The earliest open pull summary for each local head branch, optionally targeting `base`.
pub async fn list_open_pull_requests_from_branches(
    pool: &SqlitePool,
    repo_id: i64,
    base: Option<&str>,
) -> Result<Vec<crate::backend::db::pull_requests::records::BranchPullRequest>> {
    let rows = sqlx::query_as::<_, crate::backend::db::pull_requests::records::BranchPullRequest>(
        "SELECT head_branch, number, title FROM (SELECT head_branch, number, title, ROW_NUMBER() OVER (PARTITION BY head_branch ORDER BY number) AS rank FROM pull_requests WHERE repo_id = ?1 AND head_repo_id = ?1 AND state = 'open' AND (?2 IS NULL OR base_branch = ?2)) p WHERE rank = 1 ORDER BY number",
    )
    .bind(repo_id)
    .bind(base)
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

pub async fn find_open_pull_request_from_branches(
    pool: &SqlitePool,
    repo_id: i64,
    base: &str,
    head: &str,
) -> Result<Option<crate::backend::db::pull_requests::records::BranchPullRequest>> {
    Ok(sqlx::query_as("SELECT number,title,head_branch FROM pull_requests WHERE repo_id=?1 AND head_repo_id=?1 AND state='open' AND base_branch=?2 AND head_branch=?3 ORDER BY number LIMIT 1")
        .bind(repo_id).bind(base).bind(head).fetch_optional(pool).await?)
}

pub async fn merged_above(
    pool: &SqlitePool,
    repo_id: i64,
    merge_sha: &str,
    head_branch: &str,
) -> Result<bool> {
    Ok(sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM pull_requests WHERE repo_id=?1 AND merged AND merge_commit_sha=?2 AND base_branch=?3)")
        .bind(repo_id).bind(merge_sha).bind(head_branch).fetch_one(pool).await?)
}

/// Records a new head commit only while the open pull request still has the
/// expected head. Base movement is independent, so one side cannot starve the other.
pub async fn move_open_pull_request_head(
    pool: &SqlitePool,
    pull_request_id: i64,
    old_head: &str,
    new_head: &str,
    head_repo: Option<i64>,
    head_branch: &str,
) -> Result<bool> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let stored: Option<String> = sqlx::query_scalar(
        "SELECT updated_at FROM pull_requests WHERE id = ?1 AND state = 'open' AND head_sha = ?2 AND head_repo_id IS ?3 AND head_branch=?4",
    )
    .bind(pull_request_id)
    .bind(old_head)
    .bind(head_repo)
    .bind(head_branch)
    .fetch_optional(&mut *tx)
    .await?;
    let Some(stored) = stored else {
        return Ok(false);
    };
    let next = crate::backend::db::sqlite::time::next_revision(
        crate::backend::db::sqlite::time::decode(&stored)?,
    )?;
    let changed = sqlx::query(
        "UPDATE pull_requests SET head_sha = ?2, updated_at = ?3 WHERE id = ?1 AND state = 'open' AND head_sha = ?4",
    )
    .bind(pull_request_id)
    .bind(new_head)
    .bind(next)
    .bind(old_head)
    .execute(&mut *tx)
    .await?
    .rows_affected();
    if changed == 1 && old_head != new_head {
        // Refresh callers do not attest the actual pusher's base write authority.
        sqlx::query("DELETE FROM pull_request_auto_merges WHERE pull_request_id=?1")
            .bind(pull_request_id).execute(&mut *tx).await?;
    }
    tx.commit().await?;
    Ok(changed == 1)
}

pub async fn move_open_pull_request_base(
    pool: &SqlitePool,
    pull_request_id: i64,
    old_base: &str,
    new_base: &str,
    base_branch: &str,
    fork: Option<&str>,
) -> Result<bool> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let stored: Option<String> = sqlx::query_scalar(
        "SELECT updated_at FROM pull_requests WHERE id = ?1 AND state = 'open' AND base_sha = ?2 AND base_branch = ?3",
    )
    .bind(pull_request_id)
    .bind(old_base)
    .bind(base_branch)
    .fetch_optional(&mut *tx)
    .await?;
    let Some(stored) = stored else {
        return Ok(false);
    };
    let next = crate::backend::db::sqlite::time::next_revision(
        crate::backend::db::sqlite::time::decode(&stored)?,
    )?;
    let changed = sqlx::query(
        "UPDATE pull_requests SET base_sha = ?2, fork_sha = COALESCE(?6, fork_sha), updated_at = ?3 WHERE id = ?1 AND state = 'open' AND base_sha = ?4 AND base_branch = ?5",
    )
    .bind(pull_request_id)
    .bind(new_base)
    .bind(next)
    .bind(old_base)
    .bind(base_branch)
    .bind(fork)
    .execute(&mut *tx)
    .await?
    .rows_affected();
    tx.commit().await?;
    Ok(changed == 1)
}

/// Approves workflow execution for this exact head, while the actor can write.
pub async fn approve_pull_request_workflows(
    pool: &SqlitePool,
    pr_id: i64,
    actor_id: i64,
    head: &str,
) -> Result<bool> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let repo: Option<i64> = sqlx::query_scalar("SELECT repo_id FROM pull_requests WHERE id = ?1")
        .bind(pr_id)
        .fetch_optional(&mut *tx)
        .await?;
    let Some(repo) = repo else { return Ok(false) };
    if !crate::backend::db::sqlite::discussions::lock_write_authority(&mut tx, actor_id, repo)
        .await?
    {
        return Ok(false);
    }
    let changed = sqlx::query("UPDATE pull_requests SET workflows_approved_sha = ?2 WHERE id = ?1 AND head_sha = ?2 AND state = 'open' AND NOT is_draft")
        .bind(pr_id).bind(head).execute(&mut *tx).await?.rows_affected() == 1;
    tx.commit().await?;
    Ok(changed)
}

/// Takes an open draft out of draft. Writers may do it, and so may the author while
/// they can still take part in the repository.
pub async fn mark_pull_request_ready(
    pool: &SqlitePool,
    pr_id: i64,
    actor_id: i64,
    expected: Option<&str>,
) -> Result<ReadyOutcome> {
    set_pull_request_draft(pool, pr_id, actor_id, expected, false).await
}

pub async fn set_pull_request_draft(
    pool: &SqlitePool,
    pr_id: i64,
    actor_id: i64,
    expected: Option<&str>,
    is_draft: bool,
) -> Result<ReadyOutcome> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let row: Option<(i64, i64, String, bool, String)> = sqlx::query_as(
        "SELECT repo_id, author_id, state, is_draft, updated_at FROM pull_requests WHERE id = ?1",
    )
    .bind(pr_id)
    .fetch_optional(&mut *tx)
    .await?;
    let Some((repo, author, state, draft, updated)) = row else {
        return Ok(ReadyOutcome::Missing);
    };
    let writer =
        crate::backend::db::sqlite::discussions::lock_write_authority(&mut tx, actor_id, repo)
            .await?;
    let author_may = !writer
        && author == actor_id
        && crate::backend::db::sqlite::discussions::lock_contribution_authority(
            &mut tx, actor_id, repo,
        )
        .await?;
    if !writer && !author_may {
        return Ok(ReadyOutcome::Denied);
    }
    if expected.is_some_and(|revision| {
        crate::backend::db::sqlite::time::decode(&updated)
            .map_or(true, |updated| revision != updated.to_rfc3339())
    }) {
        return Ok(ReadyOutcome::Changed);
    }
    if state != "open" {
        return Ok(ReadyOutcome::NotOpen);
    }
    if draft == is_draft {
        return Ok(ReadyOutcome::NotDraft);
    }
    // Converting to a draft never adds to a cap, so only a return to ready is checked.
    if !is_draft {
        super::super::pull_caps::check(&mut tx, author, repo, false, true, Some(pr_id)).await?;
    }
    let next = crate::backend::db::sqlite::time::next_revision(
        crate::backend::db::sqlite::time::decode(&updated)?,
    )?;
    let pr = sqlx::query_as::<_, PullRequest>(
        "UPDATE pull_requests SET is_draft = ?3, updated_at = ?2 WHERE id = ?1 RETURNING *",
    )
    .bind(pr_id)
    .bind(next)
    .bind(is_draft)
    .fetch_one(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(ReadyOutcome::Ready(Box::new(pr)))
}

pub async fn create_pull_request(
    pool: &SqlitePool,
    input: NewPullRequest<'_>,
) -> Result<PullRequest> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    crate::backend::db::sqlite::discussions::require_contribution_authority(
        &mut tx,
        input.author_id,
        input.repo_id,
    )
    .await?;
    let source = match input.issue {
        Some(number) => Some(
            super::conversion::read_source(&mut tx, input.author_id, input.repo_id, number).await?,
        ),
        None => None,
    };
    let head_id = input.head_repo_id.unwrap_or(input.repo_id);
    let head = sqlx::query_as::<_, crate::backend::db::models::Repository>(
        "SELECT * FROM repositories WHERE id = ?1",
    )
    .bind(head_id)
    .fetch_optional(&mut *tx)
    .await?
    .ok_or(crate::backend::db::sqlite::discussions::AUTHORITY_DENIED)?;
    let access = crate::backend::db::sqlite::repositories::access::user_repo_access_in_transaction(
        &mut tx,
        input.author_id,
        head_id,
    )
    .await?;
    let personal_admin: bool = sqlx::query_scalar("SELECT is_admin FROM users WHERE id = ?1")
        .bind(input.author_id)
        .fetch_one(&mut *tx)
        .await?;
    if !(head.id == input.repo_id || head.forked_from_id == Some(input.repo_id))
        || (head.is_private && !access.can_read() && !(head.owner_kind == "user" && personal_admin))
    {
        return Err(crate::backend::db::sqlite::discussions::AUTHORITY_DENIED.into());
    }
    anyhow::ensure!(
        !input.maintainer_can_modify
            || can_grant_maintainer_edit(input.author_id, input.repo_id, &head),
        crate::backend::db::sqlite::discussions::AUTHORITY_DENIED
    );
    let duplicate: Option<i32> = sqlx::query_scalar(
        "SELECT number FROM pull_requests WHERE repo_id = ?1 AND head_repo_id = ?2 AND head_branch = ?3 AND base_branch = ?4 AND state = 'open' LIMIT 1",
    )
    .bind(input.repo_id)
    .bind(input.head_repo_id.unwrap_or(input.repo_id))
    .bind(input.head_branch)
    .bind(input.base_branch)
    .fetch_optional(&mut *tx)
    .await?;
    if let Some(number) = duplicate {
        return Err(DuplicateOpenPullRequest { number }.into());
    }
    super::super::pull_caps::check(
        &mut tx,
        source
            .as_ref()
            .map_or(input.author_id, |issue| issue.author_id),
        input.repo_id,
        input.is_draft,
        false,
        None,
    )
    .await?;
    let number = match &source {
        Some(issue) => issue.number,
        None => {
            crate::backend::db::sqlite::discussions::allocate_number(&mut tx, input.repo_id).await?
        }
    };
    let pr = sqlx::query_as::<_, PullRequest>(
        r#"
        INSERT INTO pull_requests (repo_id, number, author_id, title, body, head_branch, head_sha, base_branch, base_sha, is_draft, head_repo_id, maintainer_can_modify, maintainer_edit_granted_by_id,source_issue_id,created_at,milestone_id,head_publisher_id)
        VALUES (?1, ?10, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?11, ?12, ?13,?14,COALESCE(?15,strftime('%Y-%m-%dT%H:%M:%f000Z','now')),?16,?17)
        RETURNING *
        "#,
    )
    .bind(input.repo_id)
    .bind(source.as_ref().map_or(input.author_id, |issue| issue.author_id))
    .bind(source.as_ref().map_or(input.title, |issue| issue.title.as_str()))
    .bind(source.as_ref().map_or(input.body, |issue| issue.body.as_str()))
    .bind(input.head_branch)
    .bind(input.head_sha)
    .bind(input.base_branch)
    .bind(input.base_sha)
    .bind(input.is_draft)
    .bind(number)
    .bind(input.head_repo_id.unwrap_or(input.repo_id))
    .bind(input.maintainer_can_modify)
    .bind(input.maintainer_can_modify.then_some(input.author_id))
    .bind(source.as_ref().map(|issue| issue.id))
    .bind(source.as_ref().map(|issue| super::super::time::encode(issue.created_at)))
    .bind(source.as_ref().and_then(|issue| issue.milestone_id))
    .bind(input.author_id)
    .fetch_one(&mut *tx)
    .await?;
    if let Some(issue) = source.as_ref() {
        super::conversion::move_source(&mut tx, issue, &pr, input.author_id).await?;
    }
    tx.commit().await?;
    Ok(pr)
}

pub async fn get_pull_request(
    pool: &SqlitePool,
    repo_id: i64,
    number: i32,
) -> Result<Option<PullRequest>> {
    let pr = sqlx::query_as::<_, PullRequest>(
        "SELECT * FROM pull_requests WHERE repo_id = ?1 AND number = ?2",
    )
    .bind(repo_id)
    .bind(number)
    .fetch_optional(pool)
    .await?;

    Ok(pr)
}

pub async fn get_converted_pull_request(
    pool: &SqlitePool,
    repo_id: i64,
    number: i32,
) -> Result<Option<PullRequest>> {
    Ok(sqlx::query_as("SELECT * FROM pull_requests WHERE repo_id=?1 AND number=?2 AND source_issue_id IS NOT NULL")
        .bind(repo_id).bind(number).fetch_optional(pool).await?)
}

pub async fn get_pull_requests_by_ids(
    pool: &SqlitePool,
    repo_id: i64,
    ids: &[i64],
) -> Result<Vec<PullRequest>> {
    anyhow::ensure!(ids.len() <= 500, "Pull request batch exceeds its limit");
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    Ok(sqlx::query_as(
        "SELECT * FROM pull_requests WHERE repo_id = ?1 AND id IN (SELECT value FROM json_each(?2))",
    )
    .bind(repo_id)
    .bind(serde_json::to_string(ids)?)
    .fetch_all(pool)
    .await?)
}

pub async fn get_pull_request_by_id(pool: &SqlitePool, id: i64) -> Result<Option<PullRequest>> {
    let pr = sqlx::query_as::<_, PullRequest>("SELECT * FROM pull_requests WHERE id = ?1")
        .bind(id)
        .fetch_optional(pool)
        .await?;

    Ok(pr)
}

pub async fn list_filtered_pull_requests(
    pool: &SqlitePool,
    repo_id: i64,
    filter: &crate::backend::db::pull_requests::records::PullRequestFilter<'_>,
    offset: i64,
    limit: i64,
) -> Result<Vec<PullRequest>> {
    let readable = super::super::repositories::access::readable_condition();
    let rows = sqlx::query_as::<_, PullRequest>(&format!(
        "SELECT p.* FROM pull_requests p LEFT JOIN repositories h ON h.id = p.head_repo_id
         LEFT JOIN users u ON u.id = h.owner_user_id LEFT JOIN organizations o ON o.id = h.owner_org_id
         WHERE p.repo_id = ?1 AND (?2 IS NULL OR p.state = ?2 OR (?2 = 'closed' AND p.state = 'merged'))
         AND (?3 IS NULL OR p.base_branch = ?3) AND (?4 IS NULL OR COALESCE(u.username, o.name) = ?4 COLLATE NOCASE)
         AND (?5 IS NULL OR p.head_branch = ?5)
         AND (p.head_repo_id IS NULL OR EXISTS(SELECT 1 FROM repositories r LEFT JOIN users u ON u.id=?10 WHERE r.id=p.head_repo_id AND {readable}))
         AND (NOT ?6 OR (p.state = 'open' AND julianday(p.created_at) < julianday(?7) AND julianday(p.updated_at) >= julianday(?7)))
         ORDER BY {} LIMIT ?8 OFFSET ?9", filter.order()
    ))
    .bind(repo_id).bind(filter.state).bind(filter.base).bind(filter.head_owner).bind(filter.head_branch)
    .bind(filter.sort == "long-running").bind(filter.month_ago).bind(limit).bind(offset).bind(filter.viewer_id)
    .fetch_all(pool).await?;
    Ok(rows)
}

pub async fn list_repo_pull_requests(
    pool: &SqlitePool,
    repo_id: i64,
    filter: &ListFilter<'_>,
    limit: i64,
) -> Result<Vec<PullRequestListEntry>> {
    let columns = crate::backend::db::sqlite::users::accounts::AUTHOR_DISPLAY_COLUMNS;
    let (assignee_mode, assignee_id) = filter.assignee.binding();
    let (milestone_mode, milestone_id) = filter.milestone.binding();
    let labels = super::super::discussions::label_names_json(filter.labels)?;
    let order = filter
        .sort
        .order_by("pr_comments", "pull_request_id", SHOWN_PR_COMMENT);
    let prs = sqlx::query_as::<_, PullRequestListEntry>(&format!(
        r"SELECT i.*, {columns}, h.id AS head_display_id, h.name AS head_display_name, CASE WHEN h.owner_kind='user' AND h.owner_org_id IS NULL THEN hu.username WHEN h.owner_kind='org' AND h.owner_user_id IS NULL THEN ho.name END AS head_display_owner FROM pull_requests i JOIN users u ON u.id=i.author_id LEFT JOIN repositories h ON h.id=i.head_repo_id LEFT JOIN users hu ON h.owner_kind='user' AND h.owner_user_id=hu.id LEFT JOIN organizations ho ON h.owner_kind='org' AND h.owner_org_id=ho.id
        WHERE i.repo_id = ?1 AND (?2 IS NULL OR i.state = ?2 OR (?11 AND ?2 = 'closed' AND i.state = 'merged'))
            AND (?3 IS NULL OR i.title REGEXP ?3 OR i.body REGEXP ?3)
            AND (?5 IS NULL OR i.author_id = ?5)
            AND (?6 = 0 OR (?6 = 1 AND NOT EXISTS (SELECT 1 FROM pr_assignees a WHERE a.pull_request_id = i.id))
                OR (?6 = 2 AND EXISTS (SELECT 1 FROM pr_assignees a WHERE a.pull_request_id = i.id AND a.user_id = ?7)))
            AND (?8 = 0 OR (?8 = 1 AND i.milestone_id IS NULL) OR (?8 = 2 AND i.milestone_id = ?9))
            AND (?10 IS NULL OR (SELECT COUNT(DISTINCT LOWER(l.name)) FROM pr_labels pl JOIN labels l ON l.id=pl.label_id
                WHERE pl.pull_request_id = i.id AND LOWER(l.name) IN (SELECT LOWER(value) FROM json_each(?10)))
                = (SELECT COUNT(DISTINCT LOWER(value)) FROM json_each(?10)))
            AND (?13 IS NULL OR i.head_branch = ?13)
        ORDER BY {order}, i.id DESC LIMIT ?4 OFFSET ?12"
    ))
    .bind(repo_id)
    .bind(filter.state)
    .bind(filter.search.map(super::super::search::contains))
    .bind(limit)
    .bind(filter.author_id)
    .bind(assignee_mode)
    .bind(assignee_id)
    .bind(milestone_mode)
    .bind(milestone_id)
    .bind(labels)
    .bind(filter.include_merged)
    .bind(filter.offset)
    .bind(filter.head_ref)
    .fetch_all(pool)
    .await?;

    Ok(prs)
}

/// The condition that picks a viewer's pull requests. `?1` is the viewer.
fn viewer_pull_request_sql(relation: PullRequestRelation) -> String {
    let requested = "(EXISTS (SELECT 1 FROM pr_reviewers a WHERE a.pull_request_id=p.id AND a.user_id=?1) OR (p.author_id<>?1 AND EXISTS(SELECT 1 FROM pr_team_reviewers q JOIN teams t ON t.id=q.team_id JOIN effective_team_memberships m ON m.team_id=t.id JOIN effective_team_repo_access a ON a.team_id=t.id AND a.repo_id=p.repo_id JOIN repositories b ON b.id=p.repo_id AND b.owner_kind='org' AND b.owner_org_id=t.org_id AND b.owner_user_id IS NULL WHERE q.pull_request_id=p.id AND m.user_id=?1 AND a.permission BETWEEN 3 AND 5)))";
    match relation {
        PullRequestRelation::Assigned => "p.id IN (SELECT pull_request_id FROM pr_assignees WHERE user_id=?1)".to_owned(),
        PullRequestRelation::Created => "p.author_id=?1".to_owned(),
        PullRequestRelation::ReviewRequested => requested.to_owned(),
        PullRequestRelation::Involved => format!("(p.author_id=?1 OR p.id IN (SELECT pull_request_id FROM pr_assignees WHERE user_id=?1) OR {requested})"),
    }
}

/// The pull requests in repositories `viewer_id` can read that the viewer was assigned, opened, or
/// was asked to review. Visibility is applied before the limit, as for issues. A merged pull
/// request is closed, so the closed list includes it.
pub async fn list_viewer_pull_requests(
    pool: &SqlitePool,
    viewer_id: i64,
    relation: PullRequestRelation,
    open: bool,
    limit: i64,
) -> Result<Vec<ViewerPullRequestEntry>> {
    anyhow::ensure!(limit >= 0, "Result limit must be nonnegative");
    let access = crate::backend::db::sqlite::repositories::access::USER_REPO_ACCESS_SQL;
    let relation_sql = viewer_pull_request_sql(relation);
    let prs = sqlx::query_as::<_, ViewerPullRequestEntry>(&format!(
        r"SELECT p.*, r.name AS repo_name, COALESCE(ou.username, og.name) AS repo_owner
        FROM pull_requests p
        JOIN repositories r ON r.id = p.repo_id
        LEFT JOIN users ou ON ou.id = r.owner_user_id
        LEFT JOIN organizations og ON og.id = r.owner_org_id
        JOIN users u ON u.id = ?1
        WHERE (p.state = 'open') = ?2 AND {relation_sql}
            AND NOT u.is_banned AND NOT u.must_change_password
            AND ((r.owner_kind = 'user' AND u.is_admin) OR ({access}) >= 1)
        ORDER BY p.created_at DESC, p.id DESC LIMIT ?3"
    ))
    .bind(viewer_id)
    .bind(open)
    .bind(limit)
    .fetch_all(pool)
    .await?;
    Ok(prs)
}

/// Open and closed counts of the viewer's pull requests, with the same visibility as the list.
pub async fn count_viewer_pull_requests(
    pool: &SqlitePool,
    viewer_id: i64,
    relation: PullRequestRelation,
) -> Result<StateCounts> {
    let access = crate::backend::db::sqlite::repositories::access::USER_REPO_ACCESS_SQL;
    let relation_sql = viewer_pull_request_sql(relation);
    let rows = sqlx::query_as::<_, (String, i64)>(&format!(
        r"SELECT CASE WHEN p.state = 'open' THEN 'open' ELSE 'closed' END AS bucket, COUNT(*)
        FROM pull_requests p
        JOIN repositories r ON r.id = p.repo_id
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

/// Counts by state for the pull requests the filter picks; its state, sort and offset are ignored.
pub async fn count_repo_pull_requests(
    pool: &SqlitePool,
    repo_id: i64,
    filter: &ListFilter<'_>,
) -> Result<StateCounts> {
    Ok(StateCounts::from_rows(
        count_repo_pull_requests_by_state(pool, repo_id, filter)
            .await?
            .into_iter()
            .map(|(state, count)| {
                (
                    if state == "merged" {
                        "closed".into()
                    } else {
                        state
                    },
                    count,
                )
            })
            .collect(),
    ))
}

pub async fn count_repo_pull_requests_by_state(
    pool: &SqlitePool,
    repo_id: i64,
    filter: &ListFilter<'_>,
) -> Result<Vec<(String, i64)>> {
    let (assignee_mode, assignee_id) = filter.assignee.binding();
    let (milestone_mode, milestone_id) = filter.milestone.binding();
    let labels = super::super::discussions::label_names_json(filter.labels)?;
    let rows = sqlx::query_as::<_, (String, i64)>(
        r"SELECT i.state AS bucket, COUNT(*) FROM pull_requests i WHERE i.repo_id = ?1
            AND (?2 IS NULL OR i.title REGEXP ?2 OR i.body REGEXP ?2)
            AND (?3 IS NULL OR i.author_id = ?3)
            AND (?4 = 0 OR (?4 = 1 AND NOT EXISTS (SELECT 1 FROM pr_assignees a WHERE a.pull_request_id = i.id))
                OR (?4 = 2 AND EXISTS (SELECT 1 FROM pr_assignees a WHERE a.pull_request_id = i.id AND a.user_id = ?5)))
            AND (?6 = 0 OR (?6 = 1 AND i.milestone_id IS NULL) OR (?6 = 2 AND i.milestone_id = ?7))
            AND (?8 IS NULL OR (SELECT COUNT(DISTINCT LOWER(l.name)) FROM pr_labels pl JOIN labels l ON l.id=pl.label_id
                WHERE pl.pull_request_id = i.id AND LOWER(l.name) IN (SELECT LOWER(value) FROM json_each(?8)))
                = (SELECT COUNT(DISTINCT LOWER(value)) FROM json_each(?8)))
            AND (?9 IS NULL OR i.head_branch = ?9)
        GROUP BY bucket",
    )
    .bind(repo_id)
    .bind(filter.search.map(super::super::search::contains))
    .bind(filter.author_id)
    .bind(assignee_mode)
    .bind(assignee_id)
    .bind(milestone_mode)
    .bind(milestone_id)
    .bind(labels)
    .bind(filter.head_ref)
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

pub async fn update_pull_request(
    pool: &SqlitePool,
    pr_id: i64,
    edit: crate::backend::db::discussions::DiscussionEdit<'_>,
    changes: PullRequestUpdate<'_>,
) -> Result<Option<PullRequest>> {
    let PullRequestUpdate {
        base,
        maintainer_can_modify,
    } = changes;
    if !edit.has_exact_revision() {
        return Ok(None);
    }
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let scope: Option<(i64, i64, bool, Option<i64>)> = sqlx::query_as(
        "SELECT repo_id,author_id,is_draft,head_repo_id FROM pull_requests WHERE id=?1",
    )
    .bind(pr_id)
    .fetch_optional(&mut *tx)
    .await?;
    let Some((repo, author, draft, head_id)) = scope else {
        return Ok(None);
    };
    let writer =
        crate::backend::db::sqlite::discussions::lock_write_authority(&mut tx, edit.actor_id, repo)
            .await?;
    let head = if maintainer_can_modify.is_some() {
        sqlx::query_as::<_, crate::backend::db::models::Repository>(
            "SELECT * FROM repositories WHERE id=?1",
        )
        .bind(head_id)
        .fetch_optional(&mut *tx)
        .await?
    } else {
        None
    };
    let policy_only = maintainer_can_modify.is_some()
        && edit.title.is_none()
        && edit.body.is_none()
        && edit.state.is_none()
        && base.is_none();
    let head_owner = head
        .as_ref()
        .is_some_and(|head| head.is_personal_owner(edit.actor_id));
    let automatic_reopen = if let Some(base) = base.as_ref().filter(|base| base.automatic_reopen) {
        if edit.expected_state != "closed" || edit.state != Some("open") { return Ok(None); }
        let current: PullRequest = sqlx::query_as("SELECT * FROM pull_requests WHERE id=?1 AND repo_id=?2")
            .bind(pr_id).bind(repo).fetch_one(&mut *tx).await?;
        let members: Vec<PullRequest> = sqlx::query_as("SELECT * FROM pull_requests WHERE repo_id=?1 AND stack_id=?2 ORDER BY id")
            .bind(repo).bind(current.stack_id).fetch_all(&mut *tx).await?;
        if crate::backend::db::pull_requests::records::reopened_stack_base(&current, &members).as_deref() != Some(base.branch) { return Ok(None); }
        true
    } else { false };
    let author_edit = author == edit.actor_id && (base.is_none() || automatic_reopen) && maintainer_can_modify.is_none();
    if !writer
        && (!((policy_only && (author == edit.actor_id || head_owner)) || author_edit)
            || !crate::backend::db::sqlite::discussions::lock_contribution_authority(
                &mut tx,
                edit.actor_id,
                repo,
            )
            .await?)
    {
        return Ok(None);
    }
    if maintainer_can_modify == Some(true)
        && !head
            .as_ref()
            .is_some_and(|head| can_grant_maintainer_edit(edit.actor_id, repo, head))
    {
        return Ok(None);
    }
    let stored: Option<(String, String, String)> =
        sqlx::query_as("SELECT state,updated_at,title FROM pull_requests WHERE id=?1")
            .bind(pr_id)
            .fetch_optional(&mut *tx)
            .await?;
    let Some((state, stored_revision, old_title)) = stored else {
        return Ok(None);
    };
    let previous = crate::backend::db::sqlite::time::decode(&stored_revision)?;
    if state != edit.expected_state || previous != edit.expected_updated {
        return Ok(None);
    }
    // Reopening, or moving the base, must not leave two open pull requests for one head and base.
    let final_open = edit.state.unwrap_or(edit.expected_state) == "open";
    if state == "closed" && final_open {
        super::super::pull_caps::check(&mut tx, author, repo, draft, false, Some(pr_id)).await?;
    }
    if final_open && (state == "closed" || base.is_some()) {
        let duplicate: Option<i32> = sqlx::query_scalar(
            "SELECT o.number FROM pull_requests p JOIN pull_requests o \
             ON o.repo_id = p.repo_id AND o.head_repo_id = p.head_repo_id \
             AND o.head_branch = p.head_branch AND o.base_branch = COALESCE(?2, p.base_branch) \
             AND o.state = 'open' AND o.id <> p.id WHERE p.id = ?1 LIMIT 1",
        )
        .bind(pr_id)
        .bind(base.as_ref().map(|base| base.branch))
        .fetch_optional(&mut *tx)
        .await?;
        if let Some(number) = duplicate {
            return Err(DuplicateOpenPullRequest { number }.into());
        }
    }
    let next = crate::backend::db::sqlite::time::next_revision(previous)?;
    let pr = sqlx::query_as::<_, PullRequest>(
        r#"UPDATE pull_requests SET
            title = COALESCE(?2, title), body = COALESCE(?3, body),
            state = COALESCE(?4, state),
            base_branch = COALESCE(?9, base_branch), base_sha = COALESCE(?10, base_sha),
            fork_sha = CASE WHEN ?9 IS NULL THEN fork_sha WHEN stack_id IS NOT NULL THEN COALESCE(fork_sha, base_sha) ELSE NULL END,
            closed_at = CASE WHEN ?4 = 'open' THEN NULL WHEN ?4 = 'closed' AND state <> 'closed' THEN strftime('%Y-%m-%dT%H:%M:%f000Z','now') ELSE closed_at END,
            closed_by_id = CASE WHEN ?4 = 'open' THEN NULL WHEN ?4 = 'closed' AND state <> 'closed' THEN ?12 ELSE closed_by_id END,
            stack_version = stack_version + CASE WHEN ?4 IS NULL AND ?9 IS NULL THEN 0 ELSE 1 END,
            updated_at = ?7,
            maintainer_can_modify = COALESCE(?11,maintainer_can_modify),
            maintainer_edit_granted_by_id = CASE WHEN ?11 IS NULL THEN maintainer_edit_granted_by_id WHEN ?11 THEN ?12 ELSE NULL END
        WHERE id = ?1 AND state = ?5 AND updated_at = ?6 AND repo_id = ?8
            AND (?4 IS NULL OR (state <> 'merged' AND ?4 IN ('open', 'closed')))
            AND (?9 IS NULL OR stack_id IS NULL OR (state = 'closed' AND ?4 = 'open'))
        RETURNING *"#,
    ).bind(pr_id).bind(edit.title).bind(edit.body).bind(edit.state)
        .bind(edit.expected_state).bind(stored_revision).bind(next).bind(repo)
        .bind(base.as_ref().map(|base| base.branch)).bind(base.as_ref().map(|base| base.sha))
        .bind(maintainer_can_modify).bind(edit.actor_id)
        .fetch_optional(&mut *tx).await?;
    if let Some(pr) = pr.as_ref() {
        for (kind, detail, previous) in crate::backend::db::timeline::edit_events(&old_title, &state, &pr.title, &pr.state) {
            super::super::timeline::append(&mut tx, repo, "pull_request", pr_id, edit.actor_id, kind, detail, previous).await?;
        }
    }
    tx.commit().await?;
    Ok(pr)
}

/// Marks open pull requests merged by `merge_sha` and records the head and base commits that were
/// merged (each `PullRequest` holds them), even if the branches have moved on since. A row is
/// marked only while it is still open, unmerged and targets the base branch the merge went into.
/// All rows are marked in one transaction: if any row does not match, none is marked.
pub async fn mark_pull_requests_merged(
    pool: &SqlitePool,
    pulls: &[PullRequest],
    actor_id: i64,
    merge_sha: &str,
) -> Result<bool> {
    let now = crate::backend::db::sqlite::time::encode(crate::backend::db::sqlite::time::now());
    let mut ordered: Vec<&PullRequest> = pulls.iter().collect();
    ordered.sort_by_key(|pull| pull.id);
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    for pull in ordered {
        let changed = sqlx::query("UPDATE pull_requests SET state='merged',merged=1,merged_by_id=?2,closed_by_id=?2,merge_commit_sha=?3,merged_at=?4,closed_at=?4,updated_at=?4,head_sha=?5,base_sha=?6,stack_version=stack_version+1 WHERE id=?1 AND state='open' AND NOT merged AND base_branch=?7")
            .bind(pull.id)
            .bind(actor_id)
            .bind(merge_sha)
            .bind(&now)
            .bind(&pull.head_sha)
            .bind(&pull.base_sha)
            .bind(&pull.base_branch)
            .execute(&mut *tx)
            .await?
            .rows_affected();
        if changed != 1 {
            return Ok(false);
        }
        super::super::timeline::append(&mut tx, pull.repo_id, "pull_request", pull.id, actor_id, "merged", &pull.base_branch, "").await?;
        sqlx::query("UPDATE repository_ref_activities SET activity_type='pr_merge' WHERE id=(SELECT id FROM repository_ref_activities WHERE repo_id=?1 AND reference='refs/heads/'||?2 AND after_sha=?3 ORDER BY id DESC LIMIT 1) AND activity_type IN ('push','force_push')")
            .bind(pull.repo_id).bind(&pull.base_branch).bind(merge_sha).execute(&mut *tx).await?;
    }
    tx.commit().await?;
    Ok(true)
}

/// Reads what a join needs and decides it. The join and the pull request page both call this,
/// so they apply the same rules.
async fn read_join(conn: &mut sqlx::SqliteConnection, pull_request_id: i64) -> Result<JoinCheck> {
    let Some(pull) = sqlx::query_as::<_, PullRequest>("SELECT * FROM pull_requests WHERE id = ?1")
        .bind(pull_request_id)
        .fetch_optional(&mut *conn)
        .await?
    else {
        return Ok(JoinCheck::Done(StackJoin::Refused(StackRefusal::Missing)));
    };
    let parent = sqlx::query_as::<_, PullRequest>(
        "SELECT * FROM pull_requests WHERE repo_id = ?1 AND head_repo_id = ?1 AND head_branch = ?2 AND state = 'open' AND id <> ?3 ORDER BY number LIMIT 1",
    )
    .bind(pull.repo_id)
    .bind(pull.base_branch.as_str())
    .bind(pull.id)
    .fetch_optional(&mut *conn)
    .await?;
    let parent_stack =
        match parent.as_ref().and_then(|parent| parent.stack_id) {
            Some(stack) => sqlx::query_as::<_, PullRequest>(
                "SELECT * FROM pull_requests WHERE repo_id = ?1 AND stack_id = ?2 ORDER BY number",
            )
            .bind(pull.repo_id)
            .bind(stack)
            .fetch_all(&mut *conn)
            .await?,
            None => Vec::new(),
        };
    Ok(decide_join(&pull, parent.as_ref(), &parent_stack))
}

/// Asks a pull request to join the stack of the open pull request its base branch names, or to
/// start one with that pull request. The rules are checked and the join written in one
/// transaction.
pub async fn join_pull_request_stack(
    pool: &SqlitePool,
    actor_id: i64,
    pull_request_id: i64,
) -> Result<StackJoin> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let repo: Option<i64> = sqlx::query_scalar("SELECT repo_id FROM pull_requests WHERE id = ?1")
        .bind(pull_request_id)
        .fetch_optional(&mut *tx)
        .await?;
    let Some(repo) = repo else {
        return Ok(StackJoin::Refused(StackRefusal::Missing));
    };
    if !crate::backend::db::sqlite::discussions::lock_write_authority(&mut tx, actor_id, repo)
        .await?
    {
        return Ok(StackJoin::Refused(StackRefusal::Denied));
    }
    let plan = match read_join(&mut tx, pull_request_id).await? {
        JoinCheck::Ready(plan) => plan,
        JoinCheck::Done(answer) => return Ok(answer),
    };
    apply_join(&mut tx, &plan, pull_request_id).await?;
    tx.commit().await?;
    Ok(StackJoin::Joined(plan.stack_id))
}

async fn apply_join(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    plan: &crate::backend::db::pull_requests::records::JoinPlan,
    id: i64,
) -> Result<()> {
    sqlx::query("INSERT INTO pull_request_stacks(id,repo_id,number,base_branch) SELECT id,repo_id,number,base_branch FROM pull_requests WHERE id=?1 ON CONFLICT(id) DO UPDATE SET base_branch=excluded.base_branch").bind(plan.stack_id).execute(&mut **tx).await?;
    if plan.parent_starts_stack {
        sqlx::query("UPDATE pull_requests SET stack_id=?2 WHERE id=?1 AND stack_id IS NULL")
            .bind(plan.parent_id)
            .bind(plan.stack_id)
            .execute(&mut **tx)
            .await?;
    }
    sqlx::query("UPDATE pull_requests SET stack_id=?2,stack_version=stack_version+1 WHERE id=?1 AND stack_id IS NULL").bind(id).bind(plan.stack_id).execute(&mut **tx).await?;
    sqlx::query("UPDATE pull_requests SET stack_version=stack_version+1 WHERE id=?1")
        .bind(plan.parent_id)
        .execute(&mut **tx)
        .await?;
    Ok(())
}

pub async fn join_pull_request_stack_batch(
    pool: &SqlitePool,
    actor_id: i64,
    repo_id: i64,
    root: i64,
    pulls: &[i64],
    creating: bool,
) -> Result<StackJoin> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    if !crate::backend::db::sqlite::discussions::lock_write_authority(&mut tx, actor_id, repo_id)
        .await?
    {
        return Ok(StackJoin::Refused(StackRefusal::Denied));
    }
    if pulls.len() > crate::backend::db::pull_requests::records::MAX_STACK_LAYERS {
        return Ok(StackJoin::Refused(StackRefusal::TooLarge));
    }
    if pulls.len() < if creating { 2 } else { 1 }
        || pulls.iter().collect::<std::collections::HashSet<_>>().len() != pulls.len()
    {
        return Ok(StackJoin::Refused(StackRefusal::Changed));
    }
    if creating {
        let bottom = sqlx::query_as::<_, PullRequest>(
            "SELECT * FROM pull_requests WHERE id=?1 AND repo_id=?2",
        )
        .bind(root)
        .bind(repo_id)
        .fetch_optional(&mut *tx)
        .await?;
        if !bottom.is_some_and(|p| {
            p.state == "open" && p.stack_id.is_none() && p.head_repo_id == Some(repo_id)
        }) || pulls.first() != Some(&root)
        {
            return Ok(StackJoin::Refused(StackRefusal::AlreadyStacked));
        }
    } else {
        let exists: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM pull_requests WHERE repo_id=?1 AND stack_id=?2)",
        )
        .bind(repo_id)
        .bind(root)
        .fetch_one(&mut *tx)
        .await?;
        if !exists {
            return Ok(StackJoin::Refused(StackRefusal::Changed));
        }
    }
    let mut previous = creating.then_some(root);
    for id in pulls.iter().skip(if creating { 1 } else { 0 }) {
        let repo: Option<i64> = sqlx::query_scalar("SELECT repo_id FROM pull_requests WHERE id=?1")
            .bind(id)
            .fetch_optional(&mut *tx)
            .await?;
        if repo != Some(repo_id) {
            return Ok(StackJoin::Refused(StackRefusal::Missing));
        }
        let plan = match read_join(&mut tx, *id).await? {
            JoinCheck::Ready(plan) => plan,
            JoinCheck::Done(StackJoin::AlreadyJoined(_)) => {
                return Ok(StackJoin::Refused(StackRefusal::AlreadyStacked))
            }
            JoinCheck::Done(answer) => return Ok(answer),
        };
        if plan.stack_id != root || previous.is_some_and(|id| id != plan.parent_id) {
            return Ok(StackJoin::Refused(StackRefusal::NoParent));
        }
        apply_join(&mut tx, &plan, *id).await?;
        previous = Some(*id);
    }
    tx.commit().await?;
    Ok(StackJoin::Joined(root))
}

pub async fn unstack_named_stack(
    pool: &SqlitePool,
    actor_id: i64,
    repo_id: i64,
    stack_id: i64,
) -> Result<Option<Vec<PullRequest>>> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    if !crate::backend::db::sqlite::discussions::lock_write_authority(&mut tx, actor_id, repo_id)
        .await?
    {
        return Ok(None);
    }
    let rows=sqlx::query_as("UPDATE pull_requests SET stack_id=NULL,stack_version=stack_version+1 WHERE repo_id=?1 AND stack_id=?2 AND state<>'merged' RETURNING *").bind(repo_id).bind(stack_id).fetch_all(&mut *tx).await?;
    tx.commit().await?;
    Ok(Some(rows))
}

pub async fn list_pull_request_stacks(
    pool: &SqlitePool,
    repo_id: i64,
    pull_number: Option<i32>,
    limit: i64,
    offset: i64,
) -> Result<Vec<crate::backend::db::pull_requests::records::StackMetadata>> {
    Ok(sqlx::query_as("SELECT s.* FROM pull_request_stacks s WHERE s.repo_id=?1 AND EXISTS(SELECT 1 FROM pull_requests p WHERE p.repo_id=s.repo_id AND p.stack_id=s.id AND (?2 IS NULL OR p.number=?2)) ORDER BY s.number DESC LIMIT ?3 OFFSET ?4").bind(repo_id).bind(pull_number).bind(limit).bind(offset).fetch_all(pool).await?)
}
pub async fn get_pull_request_stack(
    pool: &SqlitePool,
    repo_id: i64,
    number: i32,
) -> Result<Option<crate::backend::db::pull_requests::records::StackMetadata>> {
    Ok(sqlx::query_as("SELECT s.* FROM pull_request_stacks s WHERE s.repo_id=?1 AND s.number=?2 AND EXISTS(SELECT 1 FROM pull_requests p WHERE p.repo_id=s.repo_id AND p.stack_id=s.id)").bind(repo_id).bind(number).fetch_optional(pool).await?)
}

/// The number of the open pull request that this pull request could join right now, if any.
pub async fn join_pull_request_stack_candidate(
    pool: &SqlitePool,
    pull_request_id: i64,
) -> Result<Option<i32>> {
    let mut conn = pool.acquire().await?;
    Ok(match read_join(&mut conn, pull_request_id).await? {
        JoinCheck::Ready(plan) => Some(plan.parent_number),
        JoinCheck::Done(_) => None,
    })
}

/// After `from` merged into `to`, moves the open members of stack `stack_id` that are based on
/// `from` onto `to`, whose commit is `to_sha`. Only members whose head is in this repository move.
/// Returns the members that moved, as the move left them, and the numbers left on `from`: a member
/// whose head is `to`, or whose move would duplicate another open pull request.
pub async fn retarget_stacked_pull_requests(
    pool: &SqlitePool,
    actor_id: i64,
    repo_id: i64,
    stack_id: i64,
    from: &str,
    to: &str,
    to_sha: &str,
) -> Result<(Vec<PullRequest>, Vec<i32>)> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    if !crate::backend::db::sqlite::discussions::lock_write_authority(&mut tx, actor_id, repo_id)
        .await?
    {
        return Ok((Vec::new(), Vec::new()));
    }
    let candidates: Vec<(i64, i32, Option<i64>, String, String)> = sqlx::query_as(
        "SELECT id, number, head_repo_id, head_branch, updated_at FROM pull_requests WHERE repo_id = ?1 AND stack_id = ?2 AND head_repo_id = ?1 AND state = 'open' AND base_branch = ?3 ORDER BY number",
    )
    .bind(repo_id)
    .bind(stack_id)
    .bind(from)
    .fetch_all(&mut *tx)
    .await?;
    let mut moved = Vec::new();
    let mut left = Vec::new();
    for (id, number, head_repo_id, head_branch, stored) in candidates {
        if head_repo_id == Some(repo_id) && head_branch == to {
            left.push(number);
            continue;
        }
        let duplicate: Option<i32> = sqlx::query_scalar(
            "SELECT number FROM pull_requests WHERE repo_id = ?1 AND head_repo_id IS ?2 AND head_branch = ?3 AND base_branch = ?4 AND state = 'open' AND id <> ?5 LIMIT 1",
        )
        .bind(repo_id)
        .bind(head_repo_id)
        .bind(head_branch.as_str())
        .bind(to)
        .bind(id)
        .fetch_optional(&mut *tx)
        .await?;
        if duplicate.is_some() {
            left.push(number);
            continue;
        }
        let next = crate::backend::db::sqlite::time::next_revision(
            crate::backend::db::sqlite::time::decode(&stored)?,
        )?;
        // The row comes back from the statement that moves it, so it is what the move wrote.
        let row = sqlx::query_as::<_, PullRequest>(
            "UPDATE pull_requests SET base_branch = ?2, base_sha = ?3, fork_sha = COALESCE(fork_sha, base_sha), stack_version = stack_version + 1, updated_at = ?4 WHERE id = ?1 AND state = 'open' AND base_branch = ?5 AND updated_at = ?6 RETURNING *",
        )
        .bind(id)
        .bind(to)
        .bind(to_sha)
        .bind(next)
        .bind(from)
        .bind(stored.as_str())
        .fetch_optional(&mut *tx)
        .await?;
        match row {
            Some(pull) => moved.push(pull),
            None => left.push(number),
        }
    }
    tx.commit().await?;
    Ok((moved, left))
}

pub async fn unstack_pull_requests(
    pool: &SqlitePool,
    actor_id: i64,
    pull_request_id: i64,
) -> Result<Option<Vec<PullRequest>>> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let repo: Option<i64> = sqlx::query_scalar("SELECT repo_id FROM pull_requests WHERE id = ?1")
        .bind(pull_request_id)
        .fetch_optional(&mut *tx)
        .await?;
    let Some(repo) = repo else {
        return Ok(None);
    };
    if !crate::backend::db::sqlite::discussions::lock_write_authority(&mut tx, actor_id, repo)
        .await?
    {
        return Ok(None);
    }
    // Read after the check, in the same transaction, so the stack is the one it is in now.
    let stack: Option<Option<i64>> =
        sqlx::query_scalar("SELECT stack_id FROM pull_requests WHERE id = ?1")
            .bind(pull_request_id)
            .fetch_optional(&mut *tx)
            .await?;
    let mut members = Vec::new();
    if let Some(Some(stack_id)) = stack {
        members = sqlx::query_as::<_, PullRequest>(
            "UPDATE pull_requests SET stack_id = NULL, stack_version = stack_version + 1 WHERE repo_id = ?1 AND stack_id = ?2 AND state <> 'merged' RETURNING *",
        )
        .bind(repo)
        .bind(stack_id)
        .fetch_all(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    Ok(Some(members))
}

pub async fn list_stack_members(
    pool: &SqlitePool,
    repo_id: i64,
    stack_id: i64,
) -> Result<Vec<PullRequest>> {
    let rows = sqlx::query_as::<_, PullRequest>(
        "SELECT * FROM pull_requests WHERE repo_id = ?1 AND stack_id = ?2 ORDER BY number",
    )
    .bind(repo_id)
    .bind(stack_id)
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

pub async fn list_stack_versions(
    pool: &SqlitePool,
    repo_id: i64,
    stack_id: i64,
) -> Result<Vec<(i64, i64)>> {
    let rows = sqlx::query_as::<_, (i64, i64)>(
        "SELECT id, stack_version FROM pull_requests WHERE repo_id = ?1 AND stack_id = ?2 ORDER BY id",
    )
    .bind(repo_id)
    .bind(stack_id)
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

impl<'r> sqlx::FromRow<'r, sqlx::sqlite::SqliteRow> for ViewerPullRequestEntry {
    fn from_row(row: &'r sqlx::sqlite::SqliteRow) -> Result<Self, sqlx::Error> {
        use sqlx::Row;
        Ok(Self {
            pull_request: PullRequest::from_row(row)?,
            repo_owner: row.try_get("repo_owner")?,
            repo_name: row.try_get("repo_name")?,
        })
    }
}

impl<'r> sqlx::FromRow<'r, sqlx::sqlite::SqliteRow> for PullRequestListEntry {
    fn from_row(row: &'r sqlx::sqlite::SqliteRow) -> Result<Self, sqlx::Error> {
        use sqlx::Row;
        let head_repository = match row.try_get::<Option<i64>, _>("head_display_id")? {
            Some(id) => Some(
                crate::backend::db::pull_requests::records::PullRequestHeadDisplay {
                    id,
                    name: row.try_get("head_display_name")?,
                    owner_name: row.try_get("head_display_owner")?,
                },
            ),
            None => None,
        };
        Ok(Self {
            pull_request: PullRequest::from_row(row)?,
            head_repository,
            author: crate::backend::db::sqlite::users::accounts::author_display(row)?,
        })
    }
}
