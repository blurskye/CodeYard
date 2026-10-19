//! A user's public activity, filtered to the repositories the viewer may read.
use crate::backend::{
    db::{self, activity::ActivityRow, models::User, DbPool},
    repositories::access,
};
use anyhow::Result;
use std::collections::HashMap;

pub const PAGE_SIZE: usize = 30;
/// Later pages are not served, so one request reads a bounded number of rows.
pub const MAX_PAGE: i64 = 100;
const SCAN_ROWS: i64 = 100;
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
    viewer: Option<&User>,
    user_id: i64,
    page: i64,
) -> Result<ActivityPage> {
    let page = page.clamp(1, MAX_PAGE);
    // Visible rows before this page, and one row beyond it to learn whether there is a next page.
    let skip = (page - 1) as usize * PAGE_SIZE;
    let wanted = PAGE_SIZE + 1;
    let mut readable: HashMap<i64, bool> = HashMap::new();
    let mut visible_before = 0usize;
    let mut items = Vec::with_capacity(wanted);
    let mut offset = 0i64;
    'scan: while offset < MAX_SCAN_ROWS {
        let rows = db::activity::list_user_activity(pool, user_id, SCAN_ROWS, offset).await?;
        let exhausted = (rows.len() as i64) < SCAN_ROWS;
        for row in rows {
            if !can_read_repository(pool, viewer, row.repo_id, &mut readable).await? {
                continue;
            }
            if visible_before >= skip {
                items.push(row);
                if items.len() == wanted {
                    break 'scan;
                }
            }
            visible_before += 1;
        }
        if exhausted {
            break;
        }
        offset += SCAN_ROWS;
    }
    let next_page = (items.len() == wanted).then_some(page + 1);
    items.truncate(PAGE_SIZE);
    Ok(ActivityPage { items, next_page })
}

async fn can_read_repository(
    pool: &DbPool,
    viewer: Option<&User>,
    repo_id: i64,
    cache: &mut HashMap<i64, bool>,
) -> Result<bool> {
    if let Some(allowed) = cache.get(&repo_id) {
        return Ok(*allowed);
    }
    let allowed = match db::repositories::records::get_repository_by_id(pool, repo_id).await? {
        Some(repository) => access::can_read(pool, viewer, &repository).await?,
        None => false,
    };
    cache.insert(repo_id, allowed);
    Ok(allowed)
}
