//! The signed-in user's own issues and pull requests, across the repositories they can read.
//! The queries apply the repository visibility rule, so a row the viewer cannot read is never
//! returned. Each list is capped at `LIST_LIMIT`; the counts are not.

use anyhow::Result;
use codeyard_database::DbPool;
use codeyard_db_repos::discussions::StateCounts;
use codeyard_db_repos::issues::IssueRelation;
use codeyard_db_repos::issues::ViewerIssueEntry;
use codeyard_db_repos::pull_requests::records::PullRequestRelation;
use codeyard_db_repos::pull_requests::records::ViewerPullRequestEntry;

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
    let rows =
        codeyard_db_repos::issues::list_viewer_issues(pool, viewer_id, relation, open, LIST_LIMIT)
            .await?;
    let counts = codeyard_db_repos::issues::count_viewer_issues(pool, viewer_id, relation).await?;
    Ok(IssueList { rows, counts })
}

pub async fn list_pull_requests(
    pool: &DbPool,
    viewer_id: i64,
    relation: PullRequestRelation,
    open: bool,
) -> Result<PullRequestList> {
    let rows = codeyard_db_repos::pull_requests::records::list_viewer_pull_requests(
        pool, viewer_id, relation, open, LIST_LIMIT,
    )
    .await?;
    let counts = codeyard_db_repos::pull_requests::records::count_viewer_pull_requests(
        pool, viewer_id, relation,
    )
    .await?;
    Ok(PullRequestList { rows, counts })
}
