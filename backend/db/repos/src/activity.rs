use anyhow::Result;
use chrono::DateTime;
use chrono::Utc;
use codeyard_db_core::native_operation;
use codeyard_db_core::DbPool;

pub const SCAN_ROWS: i64 = 100;

pub(crate) fn scan_window(limit: i64, offset: i64) -> (i64, i64, i64) {
    let limit = limit.clamp(0, SCAN_ROWS);
    let offset = offset.max(0);
    (limit, offset, offset.saturating_add(limit))
}

/// A row outside any source's newest offset-plus-limit prefix cannot enter the merged window.
/// Private rows need the viewer's live session `$6`; `session_live` is the engine's expiry test
/// for the session alias `s`.
pub(crate) fn activity_sql(readable: &str, session_live: &str) -> String {
    format!(
        r#"
        WITH pushes AS (
            SELECT 'push' AS kind, p.id AS item_id, p.repo_id, p.created_at,
                   CAST(NULL AS INTEGER) AS number, CAST(NULL AS TEXT) AS title, p.branch, p.commit_count
            FROM repository_pushes p WHERE p.actor_id=$1 AND p.commit_count>0
            ORDER BY p.created_at DESC,p.id DESC LIMIT $5
        ), created_repositories AS (
            SELECT 'repository_created', r.id, r.id, r.created_at,
                   CAST(NULL AS INTEGER), CAST(NULL AS TEXT), CAST(NULL AS TEXT), CAST(NULL AS INTEGER)
            FROM repositories r WHERE r.created_by_id=$1
            ORDER BY r.created_at DESC,r.id DESC LIMIT $5
        ), opened_issues AS (
            SELECT 'issue_opened', i.id, i.repo_id, i.created_at,
                   i.number,i.title,CAST(NULL AS TEXT),CAST(NULL AS INTEGER)
            FROM issues i WHERE i.author_id=$1
            ORDER BY i.created_at DESC,i.id DESC LIMIT $5
        ), opened_pulls AS (
            SELECT 'pull_request_opened', p.id, p.repo_id, p.created_at,
                   p.number,p.title,CAST(NULL AS TEXT),CAST(NULL AS INTEGER)
            FROM pull_requests p WHERE p.author_id=$1
            ORDER BY p.created_at DESC,p.id DESC LIMIT $5
        ), merged_pulls AS (
            SELECT 'pull_request_merged', p.id, p.repo_id, p.merged_at,
                   p.number,p.title,CAST(NULL AS TEXT),CAST(NULL AS INTEGER)
            FROM pull_requests p WHERE p.merged_by_id=$1 AND p.merged_at IS NOT NULL
            ORDER BY p.merged_at DESC,p.id DESC LIMIT $5
        ), issue_replies AS (
            SELECT 'issue_comment', c.id, i.repo_id, c.created_at,
                   i.number,i.title,CAST(NULL AS TEXT),CAST(NULL AS INTEGER)
            FROM issue_comments c JOIN issues i ON i.id=c.issue_id WHERE c.author_id=$1
            ORDER BY c.created_at DESC,c.id DESC LIMIT $5
        ), pull_replies AS (
            SELECT 'pull_request_comment', c.id, p.repo_id, c.created_at,
                   p.number,p.title,CAST(NULL AS TEXT),CAST(NULL AS INTEGER)
            FROM pr_comments c JOIN pull_requests p ON p.id=c.pull_request_id
            WHERE c.author_id=$1 AND NOT EXISTS(SELECT 1 FROM pr_reviews v WHERE v.id=c.review_id AND v.state='pending')
            ORDER BY c.created_at DESC,c.id DESC LIMIT $5
        ), activity AS (
            SELECT * FROM pushes UNION ALL SELECT * FROM created_repositories
            UNION ALL SELECT * FROM opened_issues UNION ALL SELECT * FROM opened_pulls
            UNION ALL SELECT * FROM merged_pulls UNION ALL SELECT * FROM issue_replies
            UNION ALL SELECT * FROM pull_replies
        ), scanned AS (
            SELECT * FROM activity ORDER BY created_at DESC,kind,item_id DESC LIMIT $2 OFFSET $3
        )
        SELECT a.kind,a.item_id,a.repo_id,a.created_at,a.number,a.title,a.branch,a.commit_count,
               COALESCE(owner_user.username,o.name) AS owner_name,r.name AS repo_name,
               COALESCE(({readable}),FALSE) AS readable
        FROM scanned a JOIN repositories r ON r.id=a.repo_id
        LEFT JOIN users owner_user ON r.owner_kind='user' AND r.owner_user_id=owner_user.id
        LEFT JOIN organizations o ON r.owner_kind='org' AND r.owner_org_id=o.id
        LEFT JOIN users u ON u.id=$4 AND NOT u.is_banned AND NOT u.must_change_password
            AND EXISTS(SELECT 1 FROM sessions s WHERE s.id=$6 AND s.user_id=u.id AND {session_live})
        ORDER BY a.created_at DESC,a.kind,a.item_id DESC
        "#
    )
}

/// One item of a user's activity. `kind` is one of `push`, `repository_created`,
/// `issue_opened`, `pull_request_opened`, `pull_request_merged`, `issue_comment` or
/// `pull_request_comment`. `number`, `title` and `branch` are set only for the kinds that
/// have them.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct ActivityRow {
    pub kind: String,
    pub item_id: i64,
    pub repo_id: i64,
    pub created_at: DateTime<Utc>,
    pub number: Option<i32>,
    pub title: Option<String>,
    pub branch: Option<String>,
    pub commit_count: Option<i32>,
    pub owner_name: String,
    pub repo_name: String,
}

/// Private profile activity is tied to the browser session that initiated the read.
#[derive(Clone, Copy)]
pub struct ActivityViewer<'a> {
    pub user: i64,
    pub session: &'a str,
}

pub struct ActivityBatch {
    pub rows: Vec<ActivityRow>,
    pub scanned: usize,
}

#[derive(sqlx::FromRow)]
pub(crate) struct ScannedActivity {
    #[sqlx(flatten)]
    pub activity: ActivityRow,
    pub readable: bool,
}

impl ActivityBatch {
    pub(crate) fn from_scan(rows: Vec<ScannedActivity>) -> Self {
        Self {
            scanned: rows.len(),
            rows: rows
                .into_iter()
                .filter(|row| row.readable)
                .map(|row| row.activity)
                .collect(),
        }
    }
}

native_operation!(crate::postgres::activity::record_repository_push,
    crate::sqlite::activity::record_repository_push,
    fn record_repository_push(
    pool: &DbPool,
    repo_id: i64,
    actor_id: i64,
    branch: &str,
    commit_count: i32,
) -> Result<()>);

native_operation!(crate::postgres::activity::list_user_activity,
    crate::sqlite::activity::list_user_activity,
    fn list_user_activity(pool: &DbPool, user_id: i64, viewer: Option<ActivityViewer<'_>>, limit: i64, offset: i64) -> Result<ActivityBatch>);
