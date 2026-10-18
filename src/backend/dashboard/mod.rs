//! The signed-in user's own issues and pull requests, across the repositories they can read.
//! The queries apply the repository visibility rule, so a row the viewer cannot read is never
//! returned. Each list is capped at `LIST_LIMIT`; the counts are not.
use crate::backend::db::{
    self,
    discussions::StateCounts,
    issues::{IssueRelation, ViewerIssueEntry},
    models::Label,
    pull_requests::records::{PullRequestRelation, ViewerPullRequestEntry},
    DbPool,
};
use anyhow::Result;
use std::collections::BTreeMap;

pub const LIST_LIMIT: i64 = 100;

pub struct IssueRow {
    pub entry: ViewerIssueEntry,
    pub labels: Vec<Label>,
    pub comments: i64,
}

pub struct IssueList {
    pub rows: Vec<IssueRow>,
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
    let entries =
        db::issues::list_viewer_issues(pool, viewer_id, relation, open, LIST_LIMIT).await?;
    let ids: Vec<i64> = entries.iter().map(|entry| entry.issue.id).collect();
    let mut labels = db::labels::get_issues_labels(pool, &ids).await?;
    let comments: BTreeMap<i64, i64> = db::issues::count_issue_comments(pool, &ids)
        .await?
        .into_iter()
        .collect();
    let rows = entries
        .into_iter()
        .map(|entry| {
            let id = entry.issue.id;
            IssueRow {
                labels: labels.remove(&id).unwrap_or_default(),
                comments: comments.get(&id).copied().unwrap_or_default(),
                entry,
            }
        })
        .collect();
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
