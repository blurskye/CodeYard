//! A user's public activity, filtered to the repositories the viewer may read.
use crate::backend::db::{self, activity::{ActivityRow, ActivityViewer}, DbPool};
use anyhow::Result;

pub const PAGE_SIZE: usize = 30;
/// Later pages are not served, so one request reads a bounded number of rows.
pub const MAX_PAGE: i64 = 100;
/// Most rows one request reads while it looks for items the viewer can read. A page the scan does
/// not fill is served short, with no next page, so a long private history is not read in full.
pub const MAX_SCAN_ROWS: i64 = 1_000;

pub struct ActivityPage {
    pub items: Vec<ActivityRow>,
    pub next_page: Option<i64>,
}

/// One page of `user_id`'s activity, newest first. Rows in repositories the viewer cannot
/// read are skipped and do not count toward the page size. Out-of-range pages are clamped, and
/// the scan stops after `MAX_SCAN_ROWS` rows.
pub async fn profile_activity(
    pool: &DbPool,
    viewer: Option<ActivityViewer<'_>>,
    user_id: i64,
    page: i64,
) -> Result<ActivityPage> {
    let page = page.clamp(1, MAX_PAGE);
    // Visible rows before this page, and one row beyond it to learn whether there is a next page.
    let skip = (page - 1) as usize * PAGE_SIZE;
    let wanted = PAGE_SIZE + 1;
    // One read of the newest rows; the batch keeps only those the viewer can read.
    let batch =
        db::activity::list_user_activity(pool, user_id, viewer, MAX_SCAN_ROWS, 0).await?;
    let mut items: Vec<ActivityRow> = batch.rows.into_iter().skip(skip).take(wanted).collect();
    let next_page = (items.len() == wanted).then_some(page + 1);
    items.truncate(PAGE_SIZE);
    Ok(ActivityPage { items, next_page })
}
