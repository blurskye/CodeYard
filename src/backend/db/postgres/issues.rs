pub(crate) mod management;
use crate::backend::db::assignees::AssignedItem;
use crate::backend::db::discussions::{
    contains_pattern, DiscussionWriteRefusal, ListFilter, StateCounts,
};
use crate::backend::db::issues::IssueCommentEntry;
use crate::backend::db::issues::IssueListEntry;
use crate::backend::db::issues::IssueMetadata;
use crate::backend::db::issues::{IssueRelation, ViewerIssueEntry};
use crate::backend::db::labels::LabelTarget;
use crate::backend::db::relationships::{Kind, Mutation};
use anyhow::Result;
use codeyard_db_core::models::Issue;
use codeyard_db_core::models::IssueComment;

pub async fn issue_repositories(pool: &PgPool, ids: &[i64]) -> Result<Vec<(i64, i64)>> {
    anyhow::ensure!(ids.len() <= 101, "Issue page exceeds its limit");
    Ok(
        sqlx::query_as("SELECT id,repo_id FROM issues WHERE id=ANY($1)")
            .bind(ids)
            .fetch_all(pool)
            .await?,
    )
}
use sqlx::{PgPool, Postgres, Transaction};

pub async fn create_issue_with_metadata(
    pool: &PgPool,
    repo_id: i64,
    author_id: impl Into<crate::backend::db::credentials::Actor>,
    title: &str,
    body: &str,
    metadata: IssueMetadata<'_>,
) -> Result<Issue> {
    let authority = author_id.into();
    let author_id = authority.user_id;
    let assignees = metadata.unique_assignees()?;
    let metadata = IssueMetadata {
        assignees: &assignees,
        ..metadata
    };
    super::retry_on_conflict(|| async {
        let mut tx = pool.begin().await?;
        // A milestone write locks the repository row exclusively, so that lock comes first
        // rather than as an upgrade of the shared lock the other checks take.
        if metadata.milestone.is_some() {
            anyhow::ensure!(
                super::discussions::lock_exclusive_write_authority(&mut tx, author_id, repo_id)
                    .await?,
                DiscussionWriteRefusal::Denied
            );
        }
        super::discussions::require_contribution_authority(&mut tx, author_id, repo_id).await?;
        if !metadata.labels.is_empty() || !metadata.assignees.is_empty() {
            anyhow::ensure!(
                super::discussions::lock_triage_authority(&mut tx, author_id, repo_id).await?,
                DiscussionWriteRefusal::Denied
            );
        }
        if metadata.issue_type.is_some() {
            anyhow::ensure!(
                super::discussions::lock_write_authority(&mut tx, author_id, repo_id).await?,
                DiscussionWriteRefusal::Denied
            );
            // Type writes take the attribute guard before any issue row.
            super::issue_attributes::lock_metadata(&mut tx).await?;
        }
        anyhow::ensure!(
            super::credentials::repository(&mut tx, &authority, repo_id, "issues", true).await?,
            DiscussionWriteRefusal::Denied
        );
        let issue = insert_in_transaction(&mut tx, repo_id, author_id, title, body).await?;
        if !metadata.labels.is_empty() {
            // The label write records each "labeled" event with its color.
            super::labels::write_item_labels_transaction(
                &mut tx,
                Some(author_id),
                repo_id,
                LabelTarget::Issue,
                issue.id,
                metadata.labels,
                false,
            )
            .await?;
        }
        for &user in metadata.assignees {
            super::assignees::assign_in_transaction(
                &mut tx,
                author_id,
                repo_id,
                AssignedItem::Issue(issue.id),
                user,
            )
            .await?;
        }
        if let Some(number) = metadata.milestone {
            super::milestones::set_item_milestone_in_transaction(
                &mut tx,
                author_id,
                repo_id,
                "issues",
                issue.id,
                Some(number),
            )
            .await?;
            let title: String =
                sqlx::query_scalar("SELECT title FROM milestones WHERE repo_id=$1 AND number=$2")
                    .bind(repo_id)
                    .bind(number)
                    .fetch_one(&mut *tx)
                    .await?;
            super::issue_attributes::event(
                &mut tx,
                (repo_id, issue.id, author_id),
                "milestoned",
                &title,
                "",
            )
            .await?;
        }
        if let Some(kind) = metadata.issue_type {
            super::issue_attributes::type_in_transaction(
                &mut tx,
                repo_id,
                author_id,
                issue.id,
                Some(kind),
            )
            .await?;
        }
        let mut tx = match metadata.parent {
            Some(parent) => {
                super::relationships::change_in_transaction(
                    tx,
                    author_id,
                    repo_id,
                    issue.id,
                    Mutation::Add {
                        kind: Kind::Parent,
                        other: parent,
                        replace: false,
                    },
                    true,
                    authority.token_id(),
                )
                .await?
            }
            None => tx,
        };
        if let (Some(parent), Some(expected)) = (metadata.parent, metadata.parent_repository) {
            let current: i64 = sqlx::query_scalar("SELECT repo_id FROM issues WHERE id=$1")
                .bind(parent)
                .fetch_one(&mut *tx)
                .await?;
            anyhow::ensure!(
                current == expected,
                crate::backend::db::relationships::Refusal::Missing
            );
        }
        let issue = sqlx::query_as("SELECT * FROM issues WHERE id=$1")
            .bind(issue.id)
            .fetch_one(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(issue)
    })
    .await
}
pub(crate) async fn insert_in_transaction(
    tx: &mut Transaction<'_, Postgres>,
    repo_id: i64,
    author_id: i64,
    title: &str,
    body: &str,
) -> Result<Issue> {
    let issue = sqlx::query_as::<_, Issue>(
        r#"
        INSERT INTO issues (repo_id, number, author_id, title, body)
        VALUES ($1, get_next_issue_number($1), $2, $3, $4)
        RETURNING *
        "#,
    )
    .bind(repo_id)
    .bind(author_id)
    .bind(title)
    .bind(body)
    .fetch_one(&mut **tx)
    .await?;
    Ok(issue)
}

pub async fn get_issue(pool: &PgPool, repo_id: i64, number: i32) -> Result<Option<Issue>> {
    let issue =
        sqlx::query_as::<_, Issue>("SELECT * FROM issues WHERE repo_id = $1 AND number = $2")
            .bind(repo_id)
            .bind(number)
            .fetch_optional(pool)
            .await?;

    Ok(issue)
}

pub async fn issue_ids_by_numbers(
    pool: &PgPool,
    repo: i64,
    viewer: Option<i64>,
    numbers: &[i32],
) -> Result<Vec<(i64, i32)>> {
    anyhow::ensure!(numbers.len() <= 500, "Too many issue numbers");
    Ok(sqlx::query_as(&format!("SELECT i.id,i.number FROM issues i JOIN repositories r ON r.id=i.repo_id LEFT JOIN users u ON u.id=$3 WHERE i.repo_id=$1 AND i.number=ANY($2) AND {} ORDER BY i.number,i.id", super::repositories::access::readable_condition())).bind(repo).bind(numbers).bind(viewer).fetch_all(pool).await?)
}

pub async fn get_issues_by_ids(pool: &PgPool, repo_id: i64, ids: &[i64]) -> Result<Vec<Issue>> {
    anyhow::ensure!(ids.len() <= 500, "Issue batch exceeds its limit");
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    Ok(sqlx::query_as(
        "SELECT * FROM issues WHERE repo_id = $1 AND id IN (SELECT unnest($2::BIGINT[]))",
    )
    .bind(repo_id)
    .bind(ids)
    .fetch_all(pool)
    .await?)
}

pub async fn get_issue_by_id(pool: &PgPool, id: i64) -> Result<Option<Issue>> {
    let issue = sqlx::query_as::<_, Issue>("SELECT * FROM issues WHERE id = $1")
        .bind(id)
        .fetch_optional(pool)
        .await?;

    Ok(issue)
}

pub async fn list_repo_issues(
    pool: &PgPool,
    repo_id: i64,
    filter: &ListFilter<'_>,
    limit: i64,
) -> Result<Vec<IssueListEntry>> {
    let columns = crate::backend::db::postgres::users::accounts::AUTHOR_DISPLAY_COLUMNS;
    let (assignee_mode, assignee_id) = filter.assignee.binding();
    let (milestone_mode, milestone_id) = filter.milestone.binding();
    let labels = super::discussions::label_names_array(filter.labels);
    let order = filter.sort.order_by("issue_comments", "issue_id", "");
    let issues = sqlx::query_as::<_, IssueListEntry>(&format!(
        r#"SELECT i.*, {columns} FROM issues i JOIN users u ON u.id=i.author_id
        WHERE i.repo_id = $1 AND ($2::TEXT IS NULL OR i.state = $2)
            AND ($3::TEXT IS NULL OR i.title ILIKE $3::TEXT ESCAPE '\' OR i.body ILIKE $3::TEXT ESCAPE '\')
            AND ($5::BIGINT IS NULL OR i.author_id = $5)
            AND ($6::BIGINT = 0 OR ($6::BIGINT = 1 AND NOT EXISTS (SELECT 1 FROM issue_assignees a WHERE a.issue_id = i.id))
                OR ($6::BIGINT = 2 AND EXISTS (SELECT 1 FROM issue_assignees a WHERE a.issue_id = i.id AND a.user_id = $7)))
            AND ($8::BIGINT = 0 OR ($8::BIGINT = 1 AND i.milestone_id IS NULL) OR ($8::BIGINT = 2 AND i.milestone_id = $9))
            AND ($10::TEXT[] IS NULL OR (SELECT COUNT(DISTINCT LOWER(l.name COLLATE "C")) FROM issue_labels il JOIN labels l ON l.id=il.label_id
                WHERE il.issue_id = i.id AND LOWER(l.name COLLATE "C") = ANY(SELECT LOWER(n COLLATE "C") FROM unnest($10::TEXT[]) AS n))
                = (SELECT COUNT(DISTINCT LOWER(n COLLATE "C")) FROM unnest($10::TEXT[]) AS n))
        ORDER BY {order}, i.id DESC LIMIT $4 OFFSET $11"#
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
    .bind(filter.offset)
    .fetch_all(pool)
    .await?;

    Ok(issues)
}

/// Counts by state for the items the filter picks; its state, sort and offset are ignored.
/// Issues and pull requests share one ordered page in GitHub's issue collection.
pub async fn list_repository_items(
    pool: &PgPool,
    repo_id: i64,
    filter: &ListFilter<'_>,
    options: &crate::backend::db::issues::RepositoryItemOptions<'_>,
    limit: i64,
) -> Result<Vec<(i64, bool)>> {
    let (mut assignee_mode, assignee_id) = filter.assignee.binding();
    if options.any_assignee {
        assignee_mode = 3;
    }
    let (mut milestone_mode, milestone_id) = filter.milestone.binding();
    if options.any_milestone {
        milestone_mode = 3;
    }
    let labels = if filter.labels.is_empty() {
        None
    } else {
        Some(serde_json::to_string(filter.labels)?)
    };
    let order = crate::backend::db::issues::repository_item_order(filter.sort);
    let (mut type_mode, type_id) = options.issue_type.binding();
    if options.any_type {
        type_mode = 3;
    }
    let mentioned = options.mentioned.map(|v| {
        format!(
            "(?i)(^|[^A-Za-z0-9_])@{}([^A-Za-z0-9_-]|$)",
            crate::backend::db::discussion_search::escape(v)
        )
    });
    Ok(sqlx::query_as(&format!(r"WITH items AS (
        SELECT id, number, author_id, state, milestone_id, created_at, updated_at, body, false AS pull_request,
            (SELECT COUNT(*) FROM issue_comments c WHERE c.issue_id=issues.id) AS comments FROM issues WHERE repo_id=$1
        UNION ALL
        SELECT id, number, author_id, CASE WHEN state='open' THEN 'open' ELSE 'closed' END, milestone_id, created_at, updated_at, body, true,
            (SELECT COUNT(*) FROM pr_comments c WHERE c.pull_request_id=pull_requests.id AND c.path IS NULL AND c.review_id IS NULL) FROM pull_requests WHERE repo_id=$1
        ), assignments AS (
            SELECT issue_id AS item_id, user_id, false AS pull_request FROM issue_assignees
            UNION ALL SELECT pull_request_id, user_id, true FROM pr_assignees
        ), item_labels AS (
            SELECT issue_id AS item_id, label_id, false AS pull_request FROM issue_labels
            UNION ALL SELECT pull_request_id, label_id, true FROM pr_labels
        ), item_types AS (
            SELECT s.issue_id,s.attribute_id FROM issue_type_assignments s JOIN issue_attributes t ON t.id=s.attribute_id JOIN repositories r ON r.id=$1 WHERE t.owner_user_id=r.owner_user_id OR t.owner_org_id=r.owner_org_id
        ), discussion_comments AS (
            SELECT issue_id AS item_id,body,false AS pull_request FROM issue_comments
            UNION ALL SELECT c.pull_request_id,c.body,true FROM pr_comments c LEFT JOIN pr_reviews v ON v.id=c.review_id WHERE c.review_id IS NULL OR v.state<>'pending'
            UNION ALL SELECT pull_request_id,body,true FROM pr_reviews WHERE state<>'pending'
        ) SELECT i.id,i.pull_request FROM items i
        WHERE ($2::TEXT IS NULL OR i.state=$2) AND ($3::BIGINT IS NULL OR i.author_id=$3)
        AND ($4=0 OR ($4=1 AND NOT EXISTS (SELECT 1 FROM assignments a WHERE a.item_id=i.id AND a.pull_request=i.pull_request))
            OR ($4=2 AND EXISTS (SELECT 1 FROM assignments a WHERE a.item_id=i.id AND a.pull_request=i.pull_request AND a.user_id=$5))
            OR ($4=3 AND EXISTS (SELECT 1 FROM assignments a WHERE a.item_id=i.id AND a.pull_request=i.pull_request)))
        AND ($6=0 OR ($6=1 AND i.milestone_id IS NULL) OR ($6=2 AND i.milestone_id=$7) OR ($6=3 AND i.milestone_id IS NOT NULL))
        AND ($8::TEXT IS NULL OR (SELECT COUNT(DISTINCT lower(l.name)) FROM item_labels il JOIN labels l ON l.id=il.label_id
            WHERE il.item_id=i.id AND il.pull_request=i.pull_request AND lower(l.name) IN (SELECT lower(value) FROM jsonb_array_elements_text($8::jsonb)))
            = (SELECT COUNT(DISTINCT lower(value)) FROM jsonb_array_elements_text($8::jsonb)))
        AND ($9::TIMESTAMPTZ IS NULL OR i.updated_at>$9)
        AND ($12=0 OR (NOT i.pull_request AND (($12=1 AND NOT EXISTS(SELECT 1 FROM item_types t WHERE t.issue_id=i.id)) OR ($12=2 AND EXISTS(SELECT 1 FROM item_types t WHERE t.issue_id=i.id AND t.attribute_id=$13)) OR ($12=3 AND EXISTS(SELECT 1 FROM item_types t WHERE t.issue_id=i.id)))))
        AND ($14::TEXT IS NULL OR i.body ~ $14 OR EXISTS(SELECT 1 FROM discussion_comments c WHERE c.item_id=i.id AND c.pull_request=i.pull_request AND c.body ~ $14))
        ORDER BY {order}, i.number DESC LIMIT $10 OFFSET $11"))
        .bind(repo_id).bind(filter.state).bind(filter.author_id).bind(assignee_mode).bind(assignee_id).bind(milestone_mode).bind(milestone_id).bind(labels)
        .bind(options.since).bind(limit).bind(filter.offset).bind(type_mode).bind(type_id).bind(mentioned).fetch_all(pool).await?)
}

pub async fn count_repo_issues(
    pool: &PgPool,
    repo_id: i64,
    filter: &ListFilter<'_>,
) -> Result<StateCounts> {
    let (assignee_mode, assignee_id) = filter.assignee.binding();
    let (milestone_mode, milestone_id) = filter.milestone.binding();
    let labels = super::discussions::label_names_array(filter.labels);
    let rows = sqlx::query_as::<_, (String, i64)>(
        r#"SELECT i.state, COUNT(*) FROM issues i WHERE i.repo_id = $1
            AND ($2::TEXT IS NULL OR i.title ILIKE $2::TEXT ESCAPE '\' OR i.body ILIKE $2::TEXT ESCAPE '\')
            AND ($3::BIGINT IS NULL OR i.author_id = $3)
            AND ($4::BIGINT = 0 OR ($4::BIGINT = 1 AND NOT EXISTS (SELECT 1 FROM issue_assignees a WHERE a.issue_id = i.id))
                OR ($4::BIGINT = 2 AND EXISTS (SELECT 1 FROM issue_assignees a WHERE a.issue_id = i.id AND a.user_id = $5)))
            AND ($6::BIGINT = 0 OR ($6::BIGINT = 1 AND i.milestone_id IS NULL) OR ($6::BIGINT = 2 AND i.milestone_id = $7))
            AND ($8::TEXT[] IS NULL OR (SELECT COUNT(DISTINCT LOWER(l.name COLLATE "C")) FROM issue_labels il JOIN labels l ON l.id=il.label_id
                WHERE il.issue_id = i.id AND LOWER(l.name COLLATE "C") = ANY(SELECT LOWER(n COLLATE "C") FROM unnest($8::TEXT[]) AS n))
                = (SELECT COUNT(DISTINCT LOWER(n COLLATE "C")) FROM unnest($8::TEXT[]) AS n))
        GROUP BY i.state"#,
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

pub async fn update_issue(
    pool: &PgPool,
    issue_id: i64,
    edit: crate::backend::db::discussions::DiscussionEdit<'_>,
) -> Result<Option<Issue>> {
    update_issue_with_reason(pool, issue_id, edit, None).await
}

pub async fn update_issue_with_reason(
    pool: &PgPool,
    issue_id: i64,
    edit: crate::backend::db::discussions::DiscussionEdit<'_>,
    state_reason: Option<&str>,
) -> Result<Option<Issue>> {
    if !edit.has_exact_revision() {
        return Ok(None);
    }
    let mut tx = pool.begin().await?;
    let repo: Option<i64> = sqlx::query_scalar("SELECT repo_id FROM issues WHERE id = $1")
        .bind(issue_id)
        .fetch_optional(&mut *tx)
        .await?;
    let Some(repo) = repo else {
        return Ok(None);
    };
    if !super::discussions::lock_write_authority(&mut tx, edit.actor_id, repo).await? {
        // The issue's author may still edit and close it with read access.
        let author: Option<i64> = sqlx::query_scalar("SELECT author_id FROM issues WHERE id = $1")
            .bind(issue_id)
            .fetch_optional(&mut *tx)
            .await?;
        if author != Some(edit.actor_id)
            || !super::discussions::lock_contribution_authority(&mut tx, edit.actor_id, repo)
                .await?
        {
            return Ok(None);
        }
    }
    // The rename and state change are recorded inside this transaction, so the history
    // cannot miss a committed edit or list concurrent transitions out of order.
    let issue = update_issue_in_transaction(&mut tx, repo, issue_id, edit, state_reason).await?;
    tx.commit().await?;
    Ok(issue)
}
/// Caller retains live issue-metadata authority and the issue revision through commit.
pub(crate) async fn update_issue_in_transaction(
    tx: &mut Transaction<'_, Postgres>,
    repo: i64,
    issue_id: i64,
    edit: crate::backend::db::discussions::DiscussionEdit<'_>,
    state_reason: Option<&str>,
) -> Result<Option<Issue>> {
    anyhow::ensure!(
        crate::backend::db::issues::valid_state_reason(
            edit.state.unwrap_or(edit.expected_state),
            state_reason
        ),
        "Invalid state reason"
    );
    let old: Option<Issue> =
        sqlx::query_as("SELECT * FROM issues WHERE id=$1 AND repo_id=$2 FOR UPDATE")
            .bind(issue_id)
            .bind(repo)
            .fetch_optional(&mut **tx)
            .await?;
    let issue = sqlx::query_as::<_, Issue>(
        r#"UPDATE issues SET
            title = COALESCE($2, title), body = COALESCE($3, body),
            state = COALESCE($4, state),
            state_reason = CASE WHEN $4='closed' AND $9::TEXT IS NOT NULL THEN $9 WHEN $4 IS NULL OR $4=state THEN state_reason WHEN $4='open' THEN 'reopened' WHEN $4='closed' THEN COALESCE($9,'completed') ELSE state_reason END,
            closed_at = CASE WHEN $4 = 'open' THEN NULL WHEN $4 = 'closed' AND state <> 'closed' THEN clock_timestamp() ELSE closed_at END,
            closed_by_id = CASE WHEN $4 = 'open' THEN NULL WHEN $4 = 'closed' AND state <> 'closed' THEN $7 ELSE closed_by_id END,
            updated_at = GREATEST(clock_timestamp(), updated_at + INTERVAL '1 microsecond')
        WHERE id = $1 AND state = $5 AND updated_at = $6 AND repo_id = $8
            AND ($4::TEXT IS NULL OR $4 IN ('open', 'closed'))
        RETURNING *"#,
    ).bind(issue_id).bind(edit.title).bind(edit.body).bind(edit.state)
        .bind(edit.expected_state).bind(edit.expected_updated).bind(edit.actor_id).bind(repo).bind(state_reason)
        .fetch_optional(&mut **tx).await?;
    if let (Some(old), Some(issue)) = (old, issue.as_ref()) {
        for (kind, detail, previous) in crate::backend::db::timeline::edit_events(
            &old.title,
            &old.state,
            &issue.title,
            &issue.state,
        ) {
            super::timeline::append(
                tx,
                repo,
                "issue",
                issue_id,
                edit.actor_id,
                kind,
                detail,
                previous,
            )
            .await?;
        }
    }
    Ok(issue)
}

pub async fn add_issue_comment(
    pool: &PgPool,
    repo_id: i64,
    issue_id: i64,
    author_id: i64,
    body: &str,
    credential: &crate::backend::db::users::accounts::AccountCredential<'_>,
) -> Result<IssueComment> {
    let mut tx = pool.begin().await?;
    super::discussions::lock_issue_contribution(&mut tx, author_id, repo_id, issue_id).await?;
    anyhow::ensure!(
        super::users::transitions::credential_live(&mut tx, author_id, credential, true).await?,
        crate::backend::db::users::accounts::AccountRestricted
    );
    let comment = sqlx::query_as::<_, IssueComment>(
        r#"
        INSERT INTO issue_comments (issue_id, author_id, body)
        VALUES ($1, $2, $3)
        RETURNING *
        "#,
    )
    .bind(issue_id)
    .bind(author_id)
    .bind(body)
    .fetch_one(&mut *tx)
    .await?;

    sqlx::query("UPDATE issues SET updated_at=GREATEST(clock_timestamp(),updated_at+INTERVAL '1 microsecond') WHERE id=$1")
        .bind(issue_id).execute(&mut *tx).await?;
    tx.commit().await?;
    Ok(comment)
}

pub async fn list_issue_comments(
    pool: &PgPool,
    issue_id: i64,
    limit: i64,
    offset: i64,
) -> Result<Vec<IssueCommentEntry>> {
    list_issue_comments_ordered(pool, issue_id, limit, offset, false, false).await
}

pub async fn list_issue_comments_ordered(
    pool: &PgPool,
    issue_id: i64,
    limit: i64,
    offset: i64,
    updated: bool,
    descending: bool,
) -> Result<Vec<IssueCommentEntry>> {
    let order = if updated { "updated_at" } else { "created_at" };
    let direction = if descending { "DESC" } else { "ASC" };
    let columns = crate::backend::db::postgres::users::accounts::AUTHOR_DISPLAY_COLUMNS;
    let comments = sqlx::query_as::<_, IssueCommentEntry>(
        &format!("SELECT i.*, {columns} FROM issue_comments i JOIN users u ON u.id=i.author_id WHERE i.issue_id = $1 ORDER BY i.{order} {direction}, i.id {direction} LIMIT $2 OFFSET $3"),
    )
    .bind(issue_id)
    .bind(limit)
    .bind(offset)
    .fetch_all(pool)
    .await?;

    Ok(comments)
}

/// One-based position in the same (created_at, id) order as the discussion list.
pub async fn issue_comment_position(pool: &PgPool, comment_id: i64) -> Result<i64> {
    Ok(sqlx::query_scalar(
        "SELECT COUNT(*) FROM issue_comments i JOIN issue_comments target ON target.issue_id=i.issue_id WHERE target.id=$1 AND (i.created_at, i.id) <= (target.created_at, target.id)",
    )
    .bind(comment_id)
    .fetch_one(pool)
    .await?)
}

/// Comment counts for one page of issues. Callers pass at most a page of ids.
pub async fn count_issue_comments(pool: &PgPool, issue_ids: &[i64]) -> Result<Vec<(i64, i64)>> {
    debug_assert!(issue_ids.len() <= 500);
    if issue_ids.is_empty() {
        return Ok(Vec::new());
    }
    let mut query = sqlx::QueryBuilder::<sqlx::Postgres>::new(
        "SELECT issue_id, COUNT(*) FROM issue_comments WHERE issue_id IN (",
    );
    let mut ids = query.separated(",");
    for id in issue_ids {
        ids.push_bind(id);
    }
    query.push(") GROUP BY issue_id");
    Ok(query.build_query_as().fetch_all(pool).await?)
}

/// The condition that picks a viewer's issues. `$1` is the viewer.
fn viewer_issue_sql(relation: IssueRelation) -> &'static str {
    match relation {
        IssueRelation::Assigned => {
            "EXISTS (SELECT 1 FROM issue_assignees a WHERE a.issue_id = i.id AND a.user_id = $1)"
        }
        IssueRelation::Created => "i.author_id = $1",
    }
}

/// The issues in repositories `viewer_id` can read that the viewer was assigned or opened. The
/// visibility rule is the access projection the repository pages use, applied before the limit so
/// hidden rows do not take up a page.
pub async fn list_viewer_issues(
    pool: &PgPool,
    viewer_id: i64,
    relation: IssueRelation,
    open: bool,
    limit: i64,
) -> Result<Vec<ViewerIssueEntry>> {
    anyhow::ensure!(
        (0..=500).contains(&limit),
        "Result limit must be between 0 and 500"
    );
    let access = crate::backend::db::postgres::repositories::access::USER_REPO_ACCESS_SQL;
    let relation_sql = viewer_issue_sql(relation);
    // One snapshot for the rows, their comment counts and their labels: an issue transferred
    // between the reads cannot bring its new repository's labels or comments to this list.
    let mut tx = pool.begin().await?;
    sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ READ ONLY")
        .execute(&mut *tx)
        .await?;
    let mut issues = sqlx::query_as::<_, ViewerIssueEntry>(&format!(
        r"SELECT i.id, i.number, i.title, i.state, i.created_at,
            (SELECT COUNT(*) FROM issue_comments c WHERE c.issue_id = i.id) AS comments,
            r.name AS repo_name, COALESCE(ou.username, og.name) AS repo_owner
        FROM issues i
        JOIN repositories r ON r.id = i.repo_id
        LEFT JOIN users ou ON ou.id = r.owner_user_id
        LEFT JOIN organizations og ON og.id = r.owner_org_id
        JOIN users u ON u.id = $1
        WHERE (i.state = 'open') = $2::BOOLEAN AND {relation_sql}
            AND NOT u.is_banned AND NOT u.must_change_password
            AND ((r.owner_kind = 'user' AND u.is_admin) OR ({access}) >= 1)
        ORDER BY i.created_at DESC, i.id DESC LIMIT $3"
    ))
    .bind(viewer_id)
    .bind(open)
    .bind(limit)
    .fetch_all(&mut *tx)
    .await?;
    let ids = issues.iter().map(|issue| issue.id).collect::<Vec<_>>();
    if !ids.is_empty() {
        let mut labels =
            std::collections::BTreeMap::<i64, Vec<codeyard_db_core::models::Label>>::new();
        for row in crate::backend::db::postgres::labels::get_items_labels(
            &mut tx,
            LabelTarget::Issue,
            None,
            &ids,
        )
        .await?
        {
            labels.entry(row.item_id).or_default().push(row.label);
        }
        for issue in &mut issues {
            issue.labels = labels.remove(&issue.id).unwrap_or_default();
        }
    }
    tx.commit().await?;
    Ok(issues)
}

/// Open and closed counts of the viewer's issues, with the same visibility as the list.
pub async fn count_viewer_issues(
    pool: &PgPool,
    viewer_id: i64,
    relation: IssueRelation,
) -> Result<StateCounts> {
    let access = crate::backend::db::postgres::repositories::access::USER_REPO_ACCESS_SQL;
    let relation_sql = viewer_issue_sql(relation);
    let rows = sqlx::query_as::<_, (String, i64)>(&format!(
        r"SELECT CASE WHEN i.state = 'open' THEN 'open' ELSE 'closed' END AS bucket, COUNT(*)
        FROM issues i
        JOIN repositories r ON r.id = i.repo_id
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

impl<'r> sqlx::FromRow<'r, sqlx::postgres::PgRow> for IssueListEntry {
    fn from_row(row: &'r sqlx::postgres::PgRow) -> Result<Self, sqlx::Error> {
        Ok(Self {
            issue: codeyard_db_core::models::Issue::from_row(row)?,
            author: crate::backend::db::postgres::users::accounts::author_display(row)?,
        })
    }
}

impl<'r> sqlx::FromRow<'r, sqlx::postgres::PgRow> for IssueCommentEntry {
    fn from_row(row: &'r sqlx::postgres::PgRow) -> Result<Self, sqlx::Error> {
        Ok(Self {
            comment: codeyard_db_core::models::IssueComment::from_row(row)?,
            author: crate::backend::db::postgres::users::accounts::author_display(row)?,
        })
    }
}

/// The author or a live repository writer may edit. Only writers may change locked threads.
#[expect(
    clippy::too_many_arguments,
    reason = "Each argument is one ID, body or check of the edit."
)]
pub async fn edit_issue_comment(
    pool: &PgPool,
    actor_id: i64,
    repo_id: i64,
    issue_id: i64,
    comment_id: i64,
    body: &str,
    expected_body: &str,
    credential: &crate::backend::db::users::accounts::AccountCredential<'_>,
) -> Result<Option<IssueComment>> {
    let mut tx = pool.begin().await?;
    // Repository authority precedes the issue and comment rows.
    if !super::discussions::lock_contribution_authority(&mut tx, actor_id, repo_id).await? {
        return Ok(None);
    }
    anyhow::ensure!(
        super::users::transitions::credential_live(&mut tx, actor_id, credential, true).await?,
        crate::backend::db::users::accounts::AccountRestricted
    );
    let writer = super::discussions::lock_write_authority(&mut tx, actor_id, repo_id).await?;
    let author: Option<i64> = sqlx::query_scalar(
        "SELECT c.author_id FROM issue_comments c JOIN issues i ON i.id = c.issue_id
         WHERE c.id = $1 AND c.issue_id = $2 AND i.repo_id = $3 AND (NOT i.locked OR $4) FOR UPDATE OF i, c",
    )
    .bind(comment_id)
    .bind(issue_id)
    .bind(repo_id)
    .bind(writer)
    .fetch_optional(&mut *tx)
    .await?;
    if author.is_none() || (author != Some(actor_id) && !writer) {
        return Ok(None);
    }
    let comment = sqlx::query_as::<_, IssueComment>(
        "UPDATE issue_comments SET body = $2, updated_at = GREATEST(clock_timestamp(), updated_at + INTERVAL '1 microsecond')
         WHERE id = $1 AND body = $3 RETURNING *",
    )
    .bind(comment_id)
    .bind(body)
    .bind(expected_body)
    .fetch_optional(&mut *tx)
    .await?;
    if comment.is_some() {
        super::issue_attributes::touch(&mut tx, issue_id).await?;
    }
    tx.commit().await?;
    Ok(comment)
}

/// Deletes a comment with the notifications that link to it. The author may delete their own
/// comment; a repository writer may delete anyone's. Returns the deleted comment, or None when
/// the delete is refused.
pub async fn delete_issue_comment(
    pool: &PgPool,
    actor_id: i64,
    repo_id: i64,
    issue_id: i64,
    comment_id: i64,
    credential: &crate::backend::db::users::accounts::AccountCredential<'_>,
) -> Result<Option<IssueComment>> {
    let mut tx = pool.begin().await?;
    if !super::discussions::lock_contribution_authority(&mut tx, actor_id, repo_id).await? {
        return Ok(None);
    }
    anyhow::ensure!(
        super::users::transitions::credential_live(&mut tx, actor_id, credential, true).await?,
        crate::backend::db::users::accounts::AccountRestricted
    );
    let writer = super::discussions::lock_write_authority(&mut tx, actor_id, repo_id).await?;
    let author: Option<i64> = sqlx::query_scalar(
        "SELECT c.author_id FROM issue_comments c JOIN issues i ON i.id = c.issue_id
         WHERE c.id = $1 AND c.issue_id = $2 AND i.repo_id = $3 AND (NOT i.locked OR $4) FOR UPDATE OF i, c",
    )
    .bind(comment_id)
    .bind(issue_id)
    .bind(repo_id)
    .bind(writer)
    .fetch_optional(&mut *tx)
    .await?;
    let Some(author) = author else {
        return Ok(None);
    };
    if author != actor_id && !writer {
        return Ok(None);
    }
    sqlx::query(
        "DELETE FROM notifications WHERE repo_id = $1 AND kind IN ('issue_comment', 'mention')
         AND metadata->>'comment_id' = $2::text",
    )
    .bind(repo_id)
    .bind(comment_id)
    .execute(&mut *tx)
    .await?;
    sqlx::query("DELETE FROM reactions WHERE subject_kind = 'issue_comment' AND subject_id = $1")
        .bind(comment_id)
        .execute(&mut *tx)
        .await?;
    let removed =
        sqlx::query_as::<_, IssueComment>("DELETE FROM issue_comments WHERE id = $1 RETURNING *")
            .bind(comment_id)
            .fetch_optional(&mut *tx)
            .await?;
    if removed.is_some() {
        super::issue_attributes::touch(&mut tx, issue_id).await?;
    }
    tx.commit().await?;
    Ok(removed)
}

pub async fn list_repository_comments(
    pool: &PgPool,
    repo_id: i64,
    filter: &crate::backend::db::issues::CommentFilter,
) -> Result<Vec<crate::backend::db::issues::ConversationComment>> {
    anyhow::ensure!(
        filter.offset >= 0 && (1..=101).contains(&filter.limit),
        "Invalid conversation comment page"
    );
    let order = crate::backend::db::issues::repository_item_order(filter.sort);
    Ok(sqlx::query_as(&format!(r"WITH comments AS (
        SELECT c.id,false AS pull_request,i.id AS item_id,i.number,c.author_id,c.created_at,c.updated_at FROM issue_comments c JOIN issues i ON i.id=c.issue_id WHERE i.repo_id=$1
        UNION ALL SELECT c.id,true,p.id,p.number,c.author_id,c.created_at,c.updated_at FROM pr_comments c JOIN pull_requests p ON p.id=c.pull_request_id WHERE p.repo_id=$1 AND c.path IS NULL AND c.review_id IS NULL
        ), page AS (SELECT i.* FROM comments i WHERE ($2::INTEGER IS NULL OR i.number=$2) AND ($3::TIMESTAMPTZ IS NULL OR i.updated_at>$3) ORDER BY {order},i.id,i.pull_request LIMIT $4 OFFSET $5)
        SELECT i.*,CASE WHEN i.pull_request THEN pc.body ELSE ic.body END AS body FROM page i LEFT JOIN issue_comments ic ON NOT i.pull_request AND ic.id=i.id LEFT JOIN pr_comments pc ON i.pull_request AND pc.id=i.id ORDER BY {order},i.id,i.pull_request"))
        .bind(repo_id).bind(filter.number).bind(filter.since).bind(filter.limit).bind(filter.offset).fetch_all(pool).await?)
}

pub async fn get_issue_comment(pool: &PgPool, comment_id: i64) -> Result<Option<IssueComment>> {
    Ok(
        sqlx::query_as::<_, IssueComment>("SELECT * FROM issue_comments WHERE id = $1")
            .bind(comment_id)
            .fetch_optional(pool)
            .await?,
    )
}
