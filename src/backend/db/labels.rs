//! Shared named operation surface; native transactions stay inside db.
use crate::backend::db::models::Label;
use crate::backend::db::DbPool;
use anyhow::Result;

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum LabelWriteRefusal {
    #[error("Label mutation denied")]
    Denied,
    #[error("Invalid label scope or batch")]
    InvalidLabels,
}

pub const MAX_ISSUE_LABELS: usize = 100;

/// The rows a label can be applied to: an issue or a pull request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LabelTarget {
    Issue,
    PullRequest,
}

impl LabelTarget {
    /// The item table, the label link table and the link column that names the item.
    pub(crate) fn tables(self) -> (&'static str, &'static str, &'static str) {
        match self {
            Self::Issue => ("issues", "issue_labels", "issue_id"),
            Self::PullRequest => ("pull_requests", "pr_labels", "pull_request_id"),
        }
    }
}

pub(crate) fn bounded_label_ids(label_ids: &[i64]) -> Result<Vec<i64>> {
    anyhow::ensure!(
        label_ids.len() <= MAX_ISSUE_LABELS,
        LabelWriteRefusal::InvalidLabels
    );
    let mut ids = label_ids.to_vec();
    ids.sort_unstable();
    ids.dedup();
    Ok(ids)
}

native_operation!(crate::backend::db::postgres::labels::create_label,
    crate::backend::db::sqlite::labels::create_label,
    fn create_label(
    pool: &DbPool,
    actor_id: i64,
    repo_id: i64,
    name: &str,
    color: &str,
    description: Option<&str>,
) -> Result<Label>);

native_operation!(crate::backend::db::postgres::labels::list_repo_labels,
    crate::backend::db::sqlite::labels::list_repo_labels,
    fn list_repo_labels(pool: &DbPool, repo_id: i64) -> Result<Vec<Label>>);

native_operation!(crate::backend::db::postgres::labels::list_label_usage,
    crate::backend::db::sqlite::labels::list_label_usage,
    fn list_label_usage(pool: &DbPool, repo_id: i64) -> Result<Vec<(i64, i64)>>);

native_operation!(crate::backend::db::postgres::labels::update_label,
    crate::backend::db::sqlite::labels::update_label,
    fn update_label(
    pool: &DbPool,
    actor_id: i64,
    repo_id: i64,
    label_id: i64,
    name: Option<&str>,
    color: Option<&str>,
    description: Option<&str>,
) -> Result<Label>);

native_operation!(crate::backend::db::postgres::labels::delete_label,
    crate::backend::db::sqlite::labels::delete_label,
    fn delete_label(pool: &DbPool, actor_id: i64, repo_id: i64, label_id: i64) -> Result<bool>);

native_operation!(crate::backend::db::postgres::labels::add_issue_label,
    crate::backend::db::sqlite::labels::add_issue_label,
    fn add_issue_label(pool: &DbPool, actor_id: i64, repo_id: i64, issue_id: i64, label_id: i64) -> Result<()>);

native_operation!(crate::backend::db::postgres::labels::add_issue_labels,
    crate::backend::db::sqlite::labels::add_issue_labels,
    fn add_issue_labels(pool: &DbPool, actor_id: i64, repo_id: i64, issue_id: i64, label_ids: &[i64]) -> Result<Vec<Label>>);

native_operation!(crate::backend::db::postgres::labels::add_pull_request_labels,
    crate::backend::db::sqlite::labels::add_pull_request_labels,
    fn add_pull_request_labels(pool: &DbPool, actor_id: i64, repo_id: i64, pull_request_id: i64, label_ids: &[i64]) -> Result<Vec<Label>>);

native_operation!(crate::backend::db::postgres::labels::remove_issue_label,
    crate::backend::db::sqlite::labels::remove_issue_label,
    fn remove_issue_label(pool: &DbPool, actor_id: i64, repo_id: i64, issue_id: i64, label_id: i64) -> Result<bool>);

native_operation!(crate::backend::db::postgres::labels::remove_pull_request_label,
    crate::backend::db::sqlite::labels::remove_pull_request_label,
    fn remove_pull_request_label(pool: &DbPool, actor_id: i64, repo_id: i64, pull_request_id: i64, label_id: i64) -> Result<bool>);

native_operation!(crate::backend::db::postgres::labels::get_issue_labels,
    crate::backend::db::sqlite::labels::get_issue_labels,
    fn get_issue_labels(pool: &DbPool, issue_id: i64) -> Result<Vec<Label>>);

native_operation!(crate::backend::db::postgres::labels::get_pull_request_labels,
    crate::backend::db::sqlite::labels::get_pull_request_labels,
    fn get_pull_request_labels(pool: &DbPool, pull_request_id: i64) -> Result<Vec<Label>>);

/// Loads a list's label assignments in bounded queries, preserving label-name order.
pub async fn get_issues_labels(
    pool: &DbPool,
    issue_ids: &[i64],
) -> Result<std::collections::BTreeMap<i64, Vec<Label>>> {
    labels_of_items(pool, LabelTarget::Issue, issue_ids).await
}

/// The same as `get_issues_labels`, for the rows of a pull request list.
pub async fn get_pull_requests_labels(
    pool: &DbPool,
    pull_request_ids: &[i64],
) -> Result<std::collections::BTreeMap<i64, Vec<Label>>> {
    labels_of_items(pool, LabelTarget::PullRequest, pull_request_ids).await
}

async fn labels_of_items(
    pool: &DbPool,
    target: LabelTarget,
    item_ids: &[i64],
) -> Result<std::collections::BTreeMap<i64, Vec<Label>>> {
    let mut ids = item_ids.to_vec();
    ids.sort_unstable();
    ids.dedup();
    let mut labels = std::collections::BTreeMap::<i64, Vec<Label>>::new();
    for chunk in ids.chunks(500) {
        let rows = match pool {
            DbPool::Postgres(pool) => {
                crate::backend::db::postgres::labels::get_items_labels(pool, target, chunk).await?
            }
            DbPool::Sqlite(pool) => {
                crate::backend::db::sqlite::labels::get_items_labels(pool, target, chunk).await?
            }
        };
        for row in rows {
            labels.entry(row.item_id).or_default().push(row.label);
        }
    }
    Ok(labels)
}

#[derive(sqlx::FromRow)]
pub(crate) struct ItemLabel {
    pub item_id: i64,
    #[sqlx(flatten)]
    pub label: Label,
}

native_operation!(crate::backend::db::postgres::labels::set_issue_labels,
    crate::backend::db::sqlite::labels::set_issue_labels,
    fn set_issue_labels(pool: &DbPool, actor_id: i64, repo_id: i64, issue_id: i64, label_ids: &[i64]) -> Result<()>);
