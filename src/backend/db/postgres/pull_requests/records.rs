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
use sqlx::PgPool;

/// The branches and commits that the open pull requests of a repository record.
pub async fn list_open_pull_request_revisions(
    pool: &PgPool,
    repo_id: i64,
) -> Result<Vec<PullRequestRevisions>> {
    let rows = sqlx::query_as::<_, PullRequestRevisions>(
        "SELECT id, repo_id, number, author_id, head_repo_id, head_branch, head_sha, base_branch, base_sha FROM pull_requests WHERE (repo_id = $1 OR head_repo_id = $1) AND state = 'open' ORDER BY id",
    )
    .bind(repo_id)
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

/// The open pull requests that use one of `branches` in this repository: the ones whose
/// base branch here is one of them, and the ones whose head branch here is one of them.
pub async fn list_open_pull_request_revisions_on_branches(
    pool: &PgPool,
    repo_id: i64,
    branches: &[&str],
) -> Result<Vec<PullRequestRevisions>> {
    let rows = sqlx::query_as::<_, PullRequestRevisions>(
        "SELECT id, repo_id, number, author_id, head_repo_id, head_branch, head_sha, base_branch, base_sha FROM pull_requests WHERE state = 'open' AND ((repo_id = $1 AND base_branch = ANY($2)) OR (head_repo_id = $1 AND head_branch = ANY($2))) ORDER BY id",
    )
    .bind(repo_id)
    .bind(branches)
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

/// The open pull requests whose head branch is one of this repository's branches, in number
/// order. A fork's pull requests are left out: their head branch is in the fork.
pub async fn list_open_pull_requests_from_branches(
    pool: &PgPool,
    repo_id: i64,
) -> Result<Vec<PullRequest>> {
    let rows = sqlx::query_as::<_, PullRequest>(
        "SELECT * FROM pull_requests WHERE repo_id = $1 AND head_repo_id = $1 AND state = 'open' ORDER BY number",
    )
    .bind(repo_id)
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

/// Records a new head commit only while the open pull request still has the
/// expected head. Base movement is independent, so one side cannot starve the other.
pub async fn move_open_pull_request_head(
    pool: &PgPool,
    pull_request_id: i64,
    old_head: &str,
    new_head: &str,
) -> Result<bool> {
    let changed = sqlx::query(
        "UPDATE pull_requests SET head_sha = $2, updated_at = GREATEST(clock_timestamp(), updated_at + INTERVAL '1 microsecond') WHERE id = $1 AND state = 'open' AND head_sha = $3",
    )
    .bind(pull_request_id)
    .bind(new_head)
    .bind(old_head)
    .execute(pool)
    .await?
    .rows_affected();
    Ok(changed == 1)
}

pub async fn move_open_pull_request_base(
    pool: &PgPool,
    pull_request_id: i64,
    old_base: &str,
    new_base: &str,
    base_branch: &str,
) -> Result<bool> {
    let changed = sqlx::query(
        "UPDATE pull_requests SET base_sha = $2, updated_at = GREATEST(clock_timestamp(), updated_at + INTERVAL '1 microsecond') WHERE id = $1 AND state = 'open' AND base_sha = $3 AND base_branch = $4",
    )
    .bind(pull_request_id)
    .bind(new_base)
    .bind(old_base)
    .bind(base_branch)
    .execute(pool)
    .await?
    .rows_affected();
    Ok(changed == 1)
}

/// Approves workflow execution for this exact head, while the actor can write.
pub async fn approve_pull_request_workflows(
    pool: &PgPool,
    pr_id: i64,
    actor_id: i64,
    head: &str,
) -> Result<bool> {
    let mut tx = pool.begin().await?;
    let repo: Option<i64> = sqlx::query_scalar("SELECT repo_id FROM pull_requests WHERE id = $1")
        .bind(pr_id)
        .fetch_optional(&mut *tx)
        .await?;
    let Some(repo) = repo else { return Ok(false) };
    if !crate::backend::db::postgres::discussions::lock_write_authority(&mut tx, actor_id, repo)
        .await?
    {
        return Ok(false);
    }
    let changed = sqlx::query("UPDATE pull_requests SET workflows_approved_sha = $2 WHERE id = $1 AND head_sha = $2 AND state = 'open' AND NOT is_draft")
        .bind(pr_id).bind(head).execute(&mut *tx).await?.rows_affected() == 1;
    tx.commit().await?;
    Ok(changed)
}

/// Takes an open draft out of draft. Writers may do it, and so may the author while
/// they can still take part in the repository.
pub async fn mark_pull_request_ready(
    pool: &PgPool,
    pr_id: i64,
    actor_id: i64,
) -> Result<ReadyOutcome> {
    let mut tx = pool.begin().await?;
    let scope: Option<(i64, i64)> =
        sqlx::query_as("SELECT repo_id, author_id FROM pull_requests WHERE id = $1")
            .bind(pr_id)
            .fetch_optional(&mut *tx)
            .await?;
    let Some((repo, author)) = scope else {
        return Ok(ReadyOutcome::Missing);
    };
    let writer =
        crate::backend::db::postgres::discussions::lock_write_authority(&mut tx, actor_id, repo)
            .await?;
    let author_may = !writer
        && author == actor_id
        && crate::backend::db::postgres::discussions::lock_contribution_authority(
            &mut tx, actor_id, repo,
        )
        .await?;
    if !writer && !author_may {
        return Ok(ReadyOutcome::Denied);
    }
    // Lock the row so the state read below is still true when it is changed.
    let row: Option<(String, bool)> = sqlx::query_as(
        "SELECT state, is_draft FROM pull_requests WHERE id = $1 AND repo_id = $2 FOR UPDATE",
    )
    .bind(pr_id)
    .bind(repo)
    .fetch_optional(&mut *tx)
    .await?;
    let Some((state, draft)) = row else {
        return Ok(ReadyOutcome::Missing);
    };
    if state != "open" {
        return Ok(ReadyOutcome::NotOpen);
    }
    if !draft {
        return Ok(ReadyOutcome::NotDraft);
    }
    let pr = sqlx::query_as::<_, PullRequest>(
        "UPDATE pull_requests SET is_draft = FALSE, updated_at = GREATEST(clock_timestamp(), updated_at + INTERVAL '1 microsecond') WHERE id = $1 RETURNING *",
    )
    .bind(pr_id)
    .fetch_one(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(ReadyOutcome::Ready(Box::new(pr)))
}

pub async fn create_pull_request(pool: &PgPool, input: NewPullRequest<'_>) -> Result<PullRequest> {
    let mut tx = pool.begin().await?;
    // Serialize duplicate detection while retaining the parent-to-child
    // organization/account/repository authority lock order.
    crate::backend::db::postgres::discussions::require_exclusive_contribution_authority(
        &mut tx,
        input.author_id,
        input.repo_id,
    )
    .await?;
    let head_id = input.head_repo_id.unwrap_or(input.repo_id);
    let head = sqlx::query_as::<_, crate::backend::db::models::Repository>(
        "SELECT * FROM repositories WHERE id = $1 FOR SHARE",
    )
    .bind(head_id)
    .fetch_optional(&mut *tx)
    .await?
    .ok_or(crate::backend::db::postgres::discussions::AUTHORITY_DENIED)?;
    let access =
        crate::backend::db::postgres::repositories::access::user_repo_access_in_transaction(
            &mut tx,
            input.author_id,
            head_id,
        )
        .await?;
    let personal_admin: bool = sqlx::query_scalar("SELECT is_admin FROM users WHERE id = $1")
        .bind(input.author_id)
        .fetch_one(&mut *tx)
        .await?;
    if !(head.id == input.repo_id || head.forked_from_id == Some(input.repo_id))
        || (head.is_private && !access.can_read() && !(head.owner_kind == "user" && personal_admin))
    {
        return Err(crate::backend::db::postgres::discussions::AUTHORITY_DENIED.into());
    }
    let duplicate: Option<i32> = sqlx::query_scalar(
        "SELECT number FROM pull_requests WHERE repo_id = $1 AND head_repo_id = $2 AND head_branch = $3 AND base_branch = $4 AND state = 'open' LIMIT 1",
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
    let pr = sqlx::query_as::<_, PullRequest>(
        r#"
        INSERT INTO pull_requests (repo_id, number, author_id, title, body, head_branch, head_sha, base_branch, base_sha, is_draft, head_repo_id)
        VALUES ($1, get_next_issue_number($1), $2, $3, $4, $5, $6, $7, $8, $9, $10)
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
    .bind(input.head_repo_id.unwrap_or(input.repo_id))
    .fetch_one(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(pr)
}

pub async fn get_pull_request(
    pool: &PgPool,
    repo_id: i64,
    number: i32,
) -> Result<Option<PullRequest>> {
    let pr = sqlx::query_as::<_, PullRequest>(
        "SELECT * FROM pull_requests WHERE repo_id = $1 AND number = $2",
    )
    .bind(repo_id)
    .bind(number)
    .fetch_optional(pool)
    .await?;

    Ok(pr)
}

pub async fn get_pull_request_by_id(pool: &PgPool, id: i64) -> Result<Option<PullRequest>> {
    let pr = sqlx::query_as::<_, PullRequest>("SELECT * FROM pull_requests WHERE id = $1")
        .bind(id)
        .fetch_optional(pool)
        .await?;

    Ok(pr)
}

pub async fn list_repo_pull_requests(
    pool: &PgPool,
    repo_id: i64,
    filter: &ListFilter<'_>,
    limit: i64,
) -> Result<Vec<PullRequestListEntry>> {
    let columns = crate::backend::db::postgres::users::accounts::AUTHOR_DISPLAY_COLUMNS;
    let (assignee_mode, assignee_id) = filter.assignee.binding();
    let (milestone_mode, milestone_id) = filter.milestone.binding();
    let labels = super::super::discussions::label_names_array(filter.labels);
    let order = filter
        .sort
        .order_by("pr_comments", "pull_request_id", SHOWN_PR_COMMENT);
    let prs = sqlx::query_as::<_, PullRequestListEntry>(&format!(
        r#"SELECT i.*, {columns}, h.id AS head_display_id, h.name AS head_display_name, CASE WHEN h.owner_kind='user' AND h.owner_org_id IS NULL THEN hu.username WHEN h.owner_kind='org' AND h.owner_user_id IS NULL THEN ho.name END AS head_display_owner FROM pull_requests i JOIN users u ON u.id=i.author_id LEFT JOIN repositories h ON h.id=i.head_repo_id LEFT JOIN users hu ON h.owner_kind='user' AND h.owner_user_id=hu.id LEFT JOIN organizations ho ON h.owner_kind='org' AND h.owner_org_id=ho.id
        WHERE i.repo_id = $1 AND ($2::TEXT IS NULL OR i.state = $2)
            AND ($3::TEXT IS NULL OR LOWER(i.title COLLATE "C") LIKE LOWER($3::TEXT COLLATE "C") ESCAPE '\' OR LOWER(i.body COLLATE "C") LIKE LOWER($3::TEXT COLLATE "C") ESCAPE '\')
            AND ($5::BIGINT IS NULL OR i.author_id = $5)
            AND ($6::BIGINT = 0 OR ($6::BIGINT = 1 AND NOT EXISTS (SELECT 1 FROM pr_assignees a WHERE a.pull_request_id = i.id))
                OR ($6::BIGINT = 2 AND EXISTS (SELECT 1 FROM pr_assignees a WHERE a.pull_request_id = i.id AND a.user_id = $7)))
            AND ($8::BIGINT = 0 OR ($8::BIGINT = 1 AND i.milestone_id IS NULL) OR ($8::BIGINT = 2 AND i.milestone_id = $9))
            AND ($10::TEXT[] IS NULL OR (SELECT COUNT(DISTINCT LOWER(l.name COLLATE "C")) FROM pr_labels pl JOIN labels l ON l.id=pl.label_id
                WHERE pl.pull_request_id = i.id AND LOWER(l.name COLLATE "C") = ANY(SELECT LOWER(n COLLATE "C") FROM unnest($10::TEXT[]) AS n))
                = (SELECT COUNT(DISTINCT LOWER(n COLLATE "C")) FROM unnest($10::TEXT[]) AS n))
        ORDER BY {order} LIMIT $4"#
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

    Ok(prs)
}

/// The condition that picks a viewer's pull requests. `$1` is the viewer.
fn viewer_pull_request_sql(relation: PullRequestRelation) -> &'static str {
    match relation {
        PullRequestRelation::Assigned => {
            "EXISTS (SELECT 1 FROM pr_assignees a WHERE a.pull_request_id = p.id AND a.user_id = $1)"
        }
        PullRequestRelation::Created => "p.author_id = $1",
        PullRequestRelation::ReviewRequested => {
            "EXISTS (SELECT 1 FROM pr_reviewers a WHERE a.pull_request_id = p.id AND a.user_id = $1)"
        }
        PullRequestRelation::Involved => {
            "(p.author_id = $1 \
             OR EXISTS (SELECT 1 FROM pr_assignees a WHERE a.pull_request_id = p.id AND a.user_id = $1) \
             OR EXISTS (SELECT 1 FROM pr_reviewers a WHERE a.pull_request_id = p.id AND a.user_id = $1))"
        }
    }
}

/// The pull requests in repositories `viewer_id` can read that the viewer was assigned, opened, or
/// was asked to review. Visibility is applied before the limit, as for issues. A merged pull
/// request is closed, so the closed list includes it.
pub async fn list_viewer_pull_requests(
    pool: &PgPool,
    viewer_id: i64,
    relation: PullRequestRelation,
    open: bool,
    limit: i64,
) -> Result<Vec<ViewerPullRequestEntry>> {
    anyhow::ensure!(limit >= 0, "Result limit must be nonnegative");
    let access = crate::backend::db::postgres::repositories::access::USER_REPO_ACCESS_SQL;
    let relation_sql = viewer_pull_request_sql(relation);
    let prs = sqlx::query_as::<_, ViewerPullRequestEntry>(&format!(
        r"SELECT p.*, r.name AS repo_name, COALESCE(ou.username, og.name) AS repo_owner
        FROM pull_requests p
        JOIN repositories r ON r.id = p.repo_id
        LEFT JOIN users ou ON ou.id = r.owner_user_id
        LEFT JOIN organizations og ON og.id = r.owner_org_id
        JOIN users u ON u.id = $1
        WHERE (p.state = 'open') = $2::BOOLEAN AND {relation_sql}
            AND NOT u.is_banned AND NOT u.must_change_password
            AND ((r.owner_kind = 'user' AND u.is_admin) OR ({access}) >= 1)
        ORDER BY p.created_at DESC, p.id DESC LIMIT $3"
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
    pool: &PgPool,
    viewer_id: i64,
    relation: PullRequestRelation,
) -> Result<StateCounts> {
    let access = crate::backend::db::postgres::repositories::access::USER_REPO_ACCESS_SQL;
    let relation_sql = viewer_pull_request_sql(relation);
    let rows = sqlx::query_as::<_, (String, i64)>(&format!(
        r"SELECT CASE WHEN p.state = 'open' THEN 'open' ELSE 'closed' END AS bucket, COUNT(*)
        FROM pull_requests p
        JOIN repositories r ON r.id = p.repo_id
        JOIN users u ON u.id = $1
        WHERE {relation_sql} AND NOT u.is_banned AND NOT u.must_change_password
            AND ((r.owner_kind = 'user' AND u.is_admin) OR ({access}) >= 1)
        GROUP BY bucket"
    ))
    .bind(viewer_id)
    .fetch_all(pool)
    .await?;
    Ok(StateCounts::from_rows(rows))
}

pub async fn count_repo_pull_requests(
    pool: &PgPool,
    repo_id: i64,
    search: Option<&str>,
) -> Result<StateCounts> {
    let rows = sqlx::query_as::<_, (String, i64)>(
        r#"SELECT i.state, COUNT(*) FROM pull_requests i WHERE i.repo_id = $1
            AND ($2::TEXT IS NULL OR LOWER(i.title COLLATE "C") LIKE LOWER($2::TEXT COLLATE "C") ESCAPE '\' OR LOWER(i.body COLLATE "C") LIKE LOWER($2::TEXT COLLATE "C") ESCAPE '\')
        GROUP BY i.state"#,
    )
    .bind(repo_id)
    .bind(search.map(contains_pattern))
    .fetch_all(pool)
    .await?;
    Ok(StateCounts::from_rows(rows))
}

pub async fn update_pull_request(
    pool: &PgPool,
    pr_id: i64,
    edit: crate::backend::db::discussions::DiscussionEdit<'_>,
    base: Option<NewBase<'_>>,
) -> Result<Option<PullRequest>> {
    if !edit.has_exact_revision() {
        return Ok(None);
    }
    let mut tx = pool.begin().await?;
    let repo: Option<i64> = sqlx::query_scalar("SELECT repo_id FROM pull_requests WHERE id = $1")
        .bind(pr_id)
        .fetch_optional(&mut *tx)
        .await?;
    let Some(repo) = repo else {
        return Ok(None);
    };
    // Reopening, or moving the base, checks for another open pull request on the same branches,
    // so it takes the repository exclusively, as creating one does.
    let final_open = edit.state.unwrap_or(edit.expected_state) == "open";
    let checks_duplicates = final_open && (edit.expected_state == "closed" || base.is_some());
    let authority = if checks_duplicates {
        crate::backend::db::postgres::discussions::lock_exclusive_write_authority(
            &mut tx,
            edit.actor_id,
            repo,
        )
        .await?
    } else {
        crate::backend::db::postgres::discussions::lock_write_authority(
            &mut tx,
            edit.actor_id,
            repo,
        )
        .await?
    };
    if !authority {
        return Ok(None);
    }
    // Lock the row so the checked update below sees the latest revision.
    sqlx::query("SELECT id FROM pull_requests WHERE id = $1 AND repo_id = $2 FOR UPDATE")
        .bind(pr_id)
        .bind(repo)
        .execute(&mut *tx)
        .await?;
    if checks_duplicates {
        let duplicate: Option<i32> = sqlx::query_scalar(
            "SELECT o.number FROM pull_requests p JOIN pull_requests o \
             ON o.repo_id = p.repo_id AND o.head_repo_id IS NOT DISTINCT FROM p.head_repo_id \
             AND o.head_branch = p.head_branch AND o.base_branch = COALESCE($2::TEXT, p.base_branch) \
             AND o.state = 'open' AND o.id <> p.id WHERE p.id = $1 LIMIT 1",
        )
        .bind(pr_id)
        .bind(base.as_ref().map(|base| base.branch))
        .fetch_optional(&mut *tx)
        .await?;
        if let Some(number) = duplicate {
            return Err(DuplicateOpenPullRequest { number }.into());
        }
    }
    let pr = sqlx::query_as::<_, PullRequest>(
        r#"UPDATE pull_requests SET
            title = COALESCE($2, title), body = COALESCE($3, body),
            state = COALESCE($4, state),
            base_branch = COALESCE($8, base_branch), base_sha = COALESCE($9, base_sha),
            closed_at = CASE WHEN $4 = 'open' THEN NULL WHEN $4 = 'closed' AND state <> 'closed' THEN clock_timestamp() ELSE closed_at END,
            updated_at = GREATEST(clock_timestamp(), updated_at + INTERVAL '1 microsecond')
        WHERE id = $1 AND state = $5 AND updated_at = $6 AND repo_id = $7
            AND ($4::TEXT IS NULL OR (state <> 'merged' AND $4 IN ('open', 'closed')))
            AND ($8::TEXT IS NULL OR stack_id IS NULL)
        RETURNING *"#,
    ).bind(pr_id).bind(edit.title).bind(edit.body).bind(edit.state)
        .bind(edit.expected_state).bind(edit.expected_updated).bind(repo)
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
    pool: &PgPool,
    pulls: &[PullRequest],
    actor_id: i64,
    merge_sha: &str,
) -> Result<bool> {
    let mut ordered: Vec<&PullRequest> = pulls.iter().collect();
    ordered.sort_by_key(|pull| pull.id);
    let mut tx = pool.begin().await?;
    for pull in ordered {
        let changed = sqlx::query("UPDATE pull_requests SET state='merged',merged=TRUE,merged_by_id=$2,merge_commit_sha=$3,merged_at=NOW(),closed_at=NOW(),updated_at=NOW(),head_sha=$4,base_sha=$5 WHERE id=$1 AND state='open' AND NOT merged AND base_branch=$6")
            .bind(pull.id)
            .bind(actor_id)
            .bind(merge_sha)
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
async fn read_join(conn: &mut sqlx::PgConnection, pull_request_id: i64) -> Result<JoinCheck> {
    let Some(pull) = sqlx::query_as::<_, PullRequest>("SELECT * FROM pull_requests WHERE id = $1")
        .bind(pull_request_id)
        .fetch_optional(&mut *conn)
        .await?
    else {
        return Ok(JoinCheck::Done(StackJoin::Refused(StackRefusal::Missing)));
    };
    let parent = sqlx::query_as::<_, PullRequest>(
        "SELECT * FROM pull_requests WHERE repo_id = $1 AND head_repo_id = $1 AND head_branch = $2 AND state = 'open' AND id <> $3 ORDER BY number LIMIT 1",
    )
    .bind(pull.repo_id)
    .bind(pull.base_branch.as_str())
    .bind(pull.id)
    .fetch_optional(&mut *conn)
    .await?;
    let parent_stack =
        match parent.as_ref().and_then(|parent| parent.stack_id) {
            Some(stack) => sqlx::query_as::<_, PullRequest>(
                "SELECT * FROM pull_requests WHERE repo_id = $1 AND stack_id = $2 ORDER BY number",
            )
            .bind(pull.repo_id)
            .bind(stack)
            .fetch_all(&mut *conn)
            .await?,
            None => Vec::new(),
        };
    Ok(decide_join(&pull, parent.as_ref(), &parent_stack))
}

/// How many times a join decides again when the rows it read changed before it could write.
const JOIN_ATTEMPTS: usize = 3;

/// Asks a pull request to join the stack of the open pull request its base branch names, or to
/// start one with that pull request. The rules are checked and the join written in one
/// transaction, which holds the repository exclusively. When the parent it chose changed before
/// the write, the join decides again, a few times at most.
pub async fn join_pull_request_stack(
    pool: &PgPool,
    actor_id: i64,
    pull_request_id: i64,
) -> Result<StackJoin> {
    for _ in 0..JOIN_ATTEMPTS {
        if let Some(answer) = join_attempt(pool, actor_id, pull_request_id).await? {
            return Ok(answer);
        }
    }
    Ok(StackJoin::Refused(StackRefusal::Changed))
}

/// One decision and its write. `None` means the rows changed before the write: the decision chose
/// a parent other than the one it locked, or a row it writes was no longer open. Nothing is written
/// then, and the caller decides again.
async fn join_attempt(
    pool: &PgPool,
    actor_id: i64,
    pull_request_id: i64,
) -> Result<Option<StackJoin>> {
    let mut tx = pool.begin().await?;
    let repo: Option<i64> = sqlx::query_scalar("SELECT repo_id FROM pull_requests WHERE id = $1")
        .bind(pull_request_id)
        .fetch_optional(&mut *tx)
        .await?;
    let Some(repo) = repo else {
        return Ok(Some(StackJoin::Refused(StackRefusal::Missing)));
    };
    if !crate::backend::db::postgres::discussions::lock_exclusive_write_authority(
        &mut tx, actor_id, repo,
    )
    .await?
    {
        return Ok(Some(StackJoin::Refused(StackRefusal::Denied)));
    }
    // Lock the rows the decision reads before making it, in id order, so a merge that marks the
    // parent cannot commit between the decision and the write. The decision is then read again;
    // if it now chooses another parent, the next attempt locks that one, in order, from the start.
    let candidate = match read_join(&mut tx, pull_request_id).await? {
        JoinCheck::Ready(plan) => Some(plan.parent_id),
        JoinCheck::Done(_) => None,
    };
    let mut rows = vec![pull_request_id];
    rows.extend(candidate);
    rows.sort_unstable();
    sqlx::query("SELECT id FROM pull_requests WHERE id = ANY($1) ORDER BY id FOR UPDATE")
        .bind(&rows)
        .execute(&mut *tx)
        .await?;
    let plan = match read_join(&mut tx, pull_request_id).await? {
        JoinCheck::Ready(plan) if Some(plan.parent_id) == candidate => plan,
        JoinCheck::Ready(_) => return Ok(None),
        JoinCheck::Done(answer) => return Ok(Some(answer)),
    };
    if plan.parent_starts_stack {
        let started = sqlx::query(
            "UPDATE pull_requests SET stack_id = $2 WHERE id = $1 AND stack_id IS NULL AND state = 'open'",
        )
        .bind(plan.parent_id)
        .bind(plan.stack_id)
        .execute(&mut *tx)
        .await?
        .rows_affected();
        if started != 1 {
            return Ok(None);
        }
    }
    let joined = sqlx::query(
        "UPDATE pull_requests SET stack_id = $2 WHERE id = $1 AND stack_id IS NULL AND state = 'open'",
    )
    .bind(pull_request_id)
    .bind(plan.stack_id)
    .execute(&mut *tx)
    .await?
    .rows_affected();
    if joined != 1 {
        return Ok(None);
    }
    tx.commit().await?;
    Ok(Some(StackJoin::Joined(plan.stack_id)))
}

/// The number of the open pull request that this pull request could join right now, if any.
pub async fn join_pull_request_stack_candidate(
    pool: &PgPool,
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
    pool: &PgPool,
    actor_id: i64,
    repo_id: i64,
    stack_id: i64,
    from: &str,
    to: &str,
    to_sha: &str,
) -> Result<(Vec<PullRequest>, Vec<i32>)> {
    let mut tx = pool.begin().await?;
    if !crate::backend::db::postgres::discussions::lock_exclusive_write_authority(
        &mut tx, actor_id, repo_id,
    )
    .await?
    {
        return Ok((Vec::new(), Vec::new()));
    }
    let candidates: Vec<i32> = sqlx::query_scalar(
        "SELECT number FROM pull_requests WHERE repo_id = $1 AND stack_id = $2 AND head_repo_id = $1 AND state = 'open' AND base_branch = $3 ORDER BY number",
    )
    .bind(repo_id)
    .bind(stack_id)
    .bind(from)
    .fetch_all(&mut *tx)
    .await?;
    // A member whose head is `to` would be based on itself, and one whose move would duplicate
    // another open pull request stays on `from`.
    let moved = sqlx::query_as::<_, PullRequest>(
        "UPDATE pull_requests SET base_branch = $4, base_sha = $5, updated_at = GREATEST(clock_timestamp(), updated_at + INTERVAL '1 microsecond') \
         WHERE repo_id = $1 AND stack_id = $2 AND head_repo_id = $1 AND state = 'open' AND base_branch = $3 \
         AND head_branch <> $4 \
         AND NOT EXISTS (SELECT 1 FROM pull_requests o WHERE o.repo_id = pull_requests.repo_id \
             AND o.head_repo_id IS NOT DISTINCT FROM pull_requests.head_repo_id \
             AND o.head_branch = pull_requests.head_branch AND o.base_branch = $4 \
             AND o.state = 'open' AND o.id <> pull_requests.id) \
         RETURNING *",
    )
    .bind(repo_id)
    .bind(stack_id)
    .bind(from)
    .bind(to)
    .bind(to_sha)
    .fetch_all(&mut *tx)
    .await?;
    tx.commit().await?;
    let left: Vec<i32> = candidates
        .into_iter()
        .filter(|number| moved.iter().all(|pull| pull.number != *number))
        .collect();
    Ok((moved, left))
}

pub async fn unstack_pull_requests(
    pool: &PgPool,
    actor_id: i64,
    pull_request_id: i64,
) -> Result<bool> {
    let mut tx = pool.begin().await?;
    let repo: Option<i64> = sqlx::query_scalar("SELECT repo_id FROM pull_requests WHERE id = $1")
        .bind(pull_request_id)
        .fetch_optional(&mut *tx)
        .await?;
    let Some(repo) = repo else {
        return Ok(false);
    };
    if !crate::backend::db::postgres::discussions::lock_exclusive_write_authority(
        &mut tx, actor_id, repo,
    )
    .await?
    {
        return Ok(false);
    }
    // Read after taking the lock, so the stack is the one the pull request is in now.
    let stack: Option<Option<i64>> =
        sqlx::query_scalar("SELECT stack_id FROM pull_requests WHERE id = $1")
            .bind(pull_request_id)
            .fetch_optional(&mut *tx)
            .await?;
    if let Some(Some(stack_id)) = stack {
        // Lock the rows in id order, as marking a merge does, so that the two cannot wait on each
        // other while a merge has already moved the trunk.
        sqlx::query(
            "SELECT id FROM pull_requests WHERE repo_id = $1 AND stack_id = $2 AND state <> 'merged' ORDER BY id FOR UPDATE",
        )
        .bind(repo)
        .bind(stack_id)
        .execute(&mut *tx)
        .await?;
        sqlx::query(
            "UPDATE pull_requests SET stack_id = NULL WHERE repo_id = $1 AND stack_id = $2 AND state <> 'merged'",
        )
        .bind(repo)
        .bind(stack_id)
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    Ok(true)
}

pub async fn list_stack_members(
    pool: &PgPool,
    repo_id: i64,
    stack_id: i64,
) -> Result<Vec<PullRequest>> {
    let rows = sqlx::query_as::<_, PullRequest>(
        "SELECT * FROM pull_requests WHERE repo_id = $1 AND stack_id = $2 ORDER BY number",
    )
    .bind(repo_id)
    .bind(stack_id)
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

impl<'r> sqlx::FromRow<'r, sqlx::postgres::PgRow> for ViewerPullRequestEntry {
    fn from_row(row: &'r sqlx::postgres::PgRow) -> Result<Self, sqlx::Error> {
        use sqlx::Row;
        Ok(Self {
            pull_request: PullRequest::from_row(row)?,
            repo_owner: row.try_get("repo_owner")?,
            repo_name: row.try_get("repo_name")?,
        })
    }
}

impl<'r> sqlx::FromRow<'r, sqlx::postgres::PgRow> for PullRequestListEntry {
    fn from_row(row: &'r sqlx::postgres::PgRow) -> Result<Self, sqlx::Error> {
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
            author: crate::backend::db::postgres::users::accounts::author_display(row)?,
        })
    }
}
