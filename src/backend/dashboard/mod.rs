//! The signed-in user's own issues and pull requests, across the repositories they can read.
//! The queries apply the repository visibility rule, so a row the viewer cannot read is never
//! returned. Each list is capped at `LIST_LIMIT`; the counts are not.
use crate::backend::db;
use crate::backend::db::discussions::StateCounts;
use crate::backend::db::issues::IssueRelation;
use crate::backend::db::issues::ViewerIssueEntry;
use crate::backend::db::pull_requests::records::PullRequestRelation;
use crate::backend::db::pull_requests::records::ViewerPullRequestEntry;
use anyhow::Result;
use codeyard_db_core::DbPool;

pub const LIST_LIMIT: i64 = 100;

pub struct IssueList {
    pub rows: Vec<ViewerIssueEntry>,
    pub counts: StateCounts,
}

pub struct PullRequestList {
    pub rows: Vec<ViewerPullRequestEntry>,
    pub counts: StateCounts,
}

pub async fn list_issues(
    pool: &DbPool,
    viewer_id: i64,
    relation: IssueRelation,
    open: bool,
) -> Result<IssueList> {
    // The rows carry their labels and comment counts, read in the rows' snapshot.
    let rows = db::issues::list_viewer_issues(pool, viewer_id, relation, open, LIST_LIMIT).await?;
    let counts = db::issues::count_viewer_issues(pool, viewer_id, relation).await?;
    Ok(IssueList { rows, counts })
}

pub async fn list_pull_requests(
    pool: &DbPool,
    viewer_id: i64,
    relation: PullRequestRelation,
    open: bool,
) -> Result<PullRequestList> {
    let rows = db::pull_requests::records::list_viewer_pull_requests(
        pool, viewer_id, relation, open, LIST_LIMIT,
    )
    .await?;
    let counts =
        db::pull_requests::records::count_viewer_pull_requests(pool, viewer_id, relation).await?;
    Ok(PullRequestList { rows, counts })
}
