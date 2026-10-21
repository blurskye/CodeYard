use crate::backend::db::discussions::{
    contains_pattern, ListFilter, StateCounts, SHOWN_PR_COMMENT,
};
use crate::backend::db::models::PullRequest;
use crate::backend::db::pull_requests::records::PullRequestListEntry;
use crate::backend::db::pull_requests::records::{
    decide_join, DuplicateOpenPullRequest, JoinCheck, NewBase, NewPullRequest,
    PullRequestRevisions, ReadyOutcome, StackJoin, StackRefusal,
};
use crate::backend::db::pull_requests::records::{PullRequestRelation, ViewerPullRequestEntry};
use anyhow::Result;
use sqlx::SqlitePool;

/// The branches and commits that the open pull requests of a repository record.
pub async fn list_open_pull_request_revisions(
    pool: &SqlitePool,
    repo_id: i64,
) -> Result<Vec<PullRequestRevisions>> {
    let rows = sqlx::query_as::<_, PullRequestRevisions>(
        "SELECT id, repo_id, number, author_id, head_repo_id, head_branch, head_sha, base_branch, base_sha, stack_id FROM pull_requests WHERE (repo_id = ?1 OR head_repo_id = ?1) AND state = 'open' ORDER BY id",
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
        "SELECT id, repo_id, number, author_id, head_repo_id, head_branch, head_sha, base_branch, base_sha, stack_id FROM pull_requests WHERE state = 'open' AND ((repo_id = ",
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

/// Records a new head commit only while the open pull request still has the
/// expected head. Base movement is independent, so one side cannot starve the other.
pub async fn move_open_pull_request_head(
    pool: &SqlitePool,
    pull_request_id: i64,
    old_head: &str,
    new_head: &str,
) -> Result<bool> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let stored: Option<String> = sqlx::query_scalar(
        "SELECT updated_at FROM pull_requests WHERE id = ?1 AND state = 'open' AND head_sha = ?2",
    )
    .bind(pull_request_id)
    .bind(old_head)
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
    if !draft {
        return Ok(ReadyOutcome::NotDraft);
    }
    let next = crate::backend::db::sqlite::time::next_revision(
        crate::backend::db::sqlite::time::decode(&updated)?,
    )?;
    let pr = sqlx::query_as::<_, PullRequest>(
        "UPDATE pull_requests SET is_draft = 0, updated_at = ?2 WHERE id = ?1 AND state = 'open' AND is_draft RETURNING *",
    )
    .bind(pr_id)
    .bind(next)
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
    let number =
        crate::backend::db::sqlite::discussions::allocate_number(&mut tx, input.repo_id).await?;
    let pr = sqlx::query_as::<_, PullRequest>(
        r#"
        INSERT INTO pull_requests (repo_id, number, author_id, title, body, head_branch, head_sha, base_branch, base_sha, is_draft, head_repo_id)
        VALUES (?1, ?10, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?11)
        RETURNING *
        "#,
    )
    .bind(input.repo_id)
    .bind(input.author_id)
    .bind(input.title)
    .bind(input.body)
    .bind(input.head_branch)
    .bind(input.head_sha)
    .bind(input.base_branch)
    .bind(input.base_sha)
    .bind(input.is_draft)
    .bind(number)
    .bind(input.head_repo_id.unwrap_or(input.repo_id))
    .fetch_one(&mut *tx)
    .await?;
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

pub async fn get_pull_request_by_id(pool: &SqlitePool, id: i64) -> Result<Option<PullRequest>> {
    let pr = sqlx::query_as::<_, PullRequest>("SELECT * FROM pull_requests WHERE id = ?1")
        .bind(id)
        .fetch_optional(pool)
        .await?;

    Ok(pr)
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
            AND (?3 IS NULL OR i.title LIKE ?3 ESCAPE '\' OR i.body LIKE ?3 ESCAPE '\')
            AND (?5 IS NULL OR i.author_id = ?5)
            AND (?6 = 0 OR (?6 = 1 AND NOT EXISTS (SELECT 1 FROM pr_assignees a WHERE a.pull_request_id = i.id))
                OR (?6 = 2 AND EXISTS (SELECT 1 FROM pr_assignees a WHERE a.pull_request_id = i.id AND a.user_id = ?7)))
            AND (?8 = 0 OR (?8 = 1 AND i.milestone_id IS NULL) OR (?8 = 2 AND i.milestone_id = ?9))
            AND (?10 IS NULL OR (SELECT COUNT(DISTINCT LOWER(l.name)) FROM pr_labels pl JOIN labels l ON l.id=pl.label_id
                WHERE pl.pull_request_id = i.id AND LOWER(l.name) IN (SELECT LOWER(value) FROM json_each(?10)))
                = (SELECT COUNT(DISTINCT LOWER(value)) FROM json_each(?10)))
        ORDER BY {order}, i.id DESC LIMIT ?4 OFFSET ?12"
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
    .bind(filter.include_merged)
    .bind(filter.offset)
    .fetch_all(pool)
    .await?;

    Ok(prs)
}

/// The condition that picks a viewer's pull requests. `?1` is the viewer.
fn viewer_pull_request_sql(relation: PullRequestRelation) -> &'static str {
    match relation {
        PullRequestRelation::Assigned => {
            "EXISTS (SELECT 1 FROM pr_assignees a WHERE a.pull_request_id = p.id AND a.user_id = ?1)"
        }
        PullRequestRelation::Created => "p.author_id = ?1",
        PullRequestRelation::ReviewRequested => {
            "EXISTS (SELECT 1 FROM pr_reviewers a WHERE a.pull_request_id = p.id AND a.user_id = ?1)"
        }
        PullRequestRelation::Involved => {
            "(p.author_id = ?1 \
             OR EXISTS (SELECT 1 FROM pr_assignees a WHERE a.pull_request_id = p.id AND a.user_id = ?1) \
             OR EXISTS (SELECT 1 FROM pr_reviewers a WHERE a.pull_request_id = p.id AND a.user_id = ?1))"
        }
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
    let (assignee_mode, assignee_id) = filter.assignee.binding();
    let (milestone_mode, milestone_id) = filter.milestone.binding();
    let labels = super::super::discussions::label_names_json(filter.labels)?;
    let rows = sqlx::query_as::<_, (String, i64)>(
        r"SELECT CASE WHEN i.state = 'merged' THEN 'closed' ELSE i.state END AS bucket, COUNT(*) FROM pull_requests i WHERE i.repo_id = ?1
            AND (?2 IS NULL OR i.title LIKE ?2 ESCAPE '\' OR i.body LIKE ?2 ESCAPE '\')
            AND (?3 IS NULL OR i.author_id = ?3)
            AND (?4 = 0 OR (?4 = 1 AND NOT EXISTS (SELECT 1 FROM pr_assignees a WHERE a.pull_request_id = i.id))
                OR (?4 = 2 AND EXISTS (SELECT 1 FROM pr_assignees a WHERE a.pull_request_id = i.id AND a.user_id = ?5)))
            AND (?6 = 0 OR (?6 = 1 AND i.milestone_id IS NULL) OR (?6 = 2 AND i.milestone_id = ?7))
            AND (?8 IS NULL OR (SELECT COUNT(DISTINCT LOWER(l.name)) FROM pr_labels pl JOIN labels l ON l.id=pl.label_id
                WHERE pl.pull_request_id = i.id AND LOWER(l.name) IN (SELECT LOWER(value) FROM json_each(?8)))
                = (SELECT COUNT(DISTINCT LOWER(value)) FROM json_each(?8)))
        GROUP BY bucket",
    )
    .bind(repo_id)
    .bind(filter.search.map(contains_pattern))
    .bind(filter.author_id)
    .bind(assignee_mode)
    .bind(assignee_id)
    .bind(milestone_mode)
    .bind(milestone_id)
    .bind(labels)
    .fetch_all(pool)
    .await?;
    Ok(StateCounts::from_rows(rows))
}

pub async fn update_pull_request(
    pool: &SqlitePool,
    pr_id: i64,
    edit: crate::backend::db::discussions::DiscussionEdit<'_>,
    base: Option<NewBase<'_>>,
) -> Result<Option<PullRequest>> {
    if !edit.has_exact_revision() {
        return Ok(None);
    }
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let repo: Option<i64> = sqlx::query_scalar("SELECT repo_id FROM pull_requests WHERE id = ?1")
        .bind(pr_id)
        .fetch_optional(&mut *tx)
        .await?;
    let Some(repo) = repo else {
        return Ok(None);
    };
    if !crate::backend::db::sqlite::discussions::lock_write_authority(&mut tx, edit.actor_id, repo)
        .await?
    {
        return Ok(None);
    }
    let stored: Option<(String, String)> =
        sqlx::query_as("SELECT state,updated_at FROM pull_requests WHERE id=?1")
            .bind(pr_id)
            .fetch_optional(&mut *tx)
            .await?;
    let Some((state, stored_revision)) = stored else {
        return Ok(None);
    };
    let previous = crate::backend::db::sqlite::time::decode(&stored_revision)?;
    if state != edit.expected_state || previous != edit.expected_updated {
        return Ok(None);
    }
    // Reopening, or moving the base, must not leave two open pull requests for one head and base.
    let final_open = edit.state.unwrap_or(edit.expected_state) == "open";
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
            updated_at = ?7
        WHERE id = ?1 AND state = ?5 AND updated_at = ?6 AND repo_id = ?8
            AND (?4 IS NULL OR (state <> 'merged' AND ?4 IN ('open', 'closed')))
            AND (?9 IS NULL OR stack_id IS NULL OR (state = 'closed' AND ?4 = 'open'))
        RETURNING *"#,
    ).bind(pr_id).bind(edit.title).bind(edit.body).bind(edit.state)
        .bind(edit.expected_state).bind(stored_revision).bind(next).bind(repo)
        .bind(base.as_ref().map(|base| base.branch)).bind(base.as_ref().map(|base| base.sha))
        .fetch_optional(&mut *tx).await?;
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
        let changed = sqlx::query("UPDATE pull_requests SET state='merged',merged=1,merged_by_id=?2,merge_commit_sha=?3,merged_at=?4,closed_at=?4,updated_at=?4,head_sha=?5,base_sha=?6 WHERE id=?1 AND state='open' AND NOT merged AND base_branch=?7")
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
    if plan.parent_starts_stack {
        sqlx::query("UPDATE pull_requests SET stack_id = ?2 WHERE id = ?1 AND stack_id IS NULL")
            .bind(plan.parent_id)
            .bind(plan.stack_id)
            .execute(&mut *tx)
            .await?;
    }
    sqlx::query("UPDATE pull_requests SET stack_id = ?2 WHERE id = ?1 AND stack_id IS NULL")
        .bind(pull_request_id)
        .bind(plan.stack_id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(StackJoin::Joined(plan.stack_id))
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
            "UPDATE pull_requests SET base_branch = ?2, base_sha = ?3, fork_sha = COALESCE(fork_sha, base_sha), updated_at = ?4 WHERE id = ?1 AND state = 'open' AND base_branch = ?5 AND updated_at = ?6 RETURNING *",
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
            "UPDATE pull_requests SET stack_id = NULL WHERE repo_id = ?1 AND stack_id = ?2 AND state <> 'merged' RETURNING *",
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
