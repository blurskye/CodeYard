//! Paging for lists: the `page` and `per_page` of JSON lists and the page numbers of HTML lists.
use crate::backend::api::http::context::PageContext;
use crate::backend::api::http::error::ApiError;
use crate::backend::api::repos::chrome::ApiResult;
use crate::backend::repositories::store::MAX_COMMIT_PAGE_SIZE;
use axum::http::StatusCode;
use axum::response::Response;
use axum::Json;

#[expect(
    clippy::result_large_err,
    reason = "HTML adapters preserve complete Axum responses under the PageContext contract."
)]
pub(crate) fn number(
    ctx: &PageContext,
    key: &str,
    default: usize,
    allowed: std::ops::RangeInclusive<usize>,
) -> Result<usize, Response> {
    match ctx.query.get(key) {
        None => Ok(default),
        Some(value) => value
            .parse::<usize>()
            .ok()
            .filter(|value| allowed.contains(value))
            .ok_or_else(|| {
                ctx.error(
                    StatusCode::BAD_REQUEST,
                    "Invalid pagination or preview offset.",
                )
            }),
    }
}

/// Items per page for HTML lists and the default `per_page` for JSON lists.
pub(crate) const DEFAULT_PER_PAGE: usize = 30;
pub(crate) const MAX_PAGE: usize = 1_000;

#[derive(serde::Deserialize)]
pub(crate) struct PageQuery {
    pub(crate) page: Option<u32>,
    pub(crate) per_page: Option<u32>,
}

fn page_error() -> (StatusCode, Json<ApiError>) {
    (
        StatusCode::BAD_REQUEST,
        Json(ApiError::new(
            "validation_error",
            "Page must be between 1 and 1000",
        )),
    )
}

fn page_offset(page: usize, limit: usize) -> Option<usize> {
    page.checked_sub(1)?.checked_mul(limit)
}

/// SQL offset and limit for a 1-based page. The limit is clamped to the store's
/// page size.
pub(crate) fn page_window(page: usize, per_page: usize) -> Option<(usize, usize)> {
    if !(1..=MAX_PAGE).contains(&page) {
        return None;
    }
    let limit = per_page.clamp(1, MAX_COMMIT_PAGE_SIZE);
    Some((page_offset(page, limit)?, limit))
}

/// Whether a page after `page` exists. `fetched` is the row count read for `page`
/// with one extra row, so a full extra row means more rows follow. The cap is the
/// last page a URL can name, so it never offers a next page.
pub(crate) fn has_next_page(page: usize, fetched: usize, per_page: usize, max_page: usize) -> bool {
    fetched > per_page && page < max_page
}

/// JSON list paging: `page` is 1 through 1,000, `per_page` defaults to 30 and is
/// clamped to 1 through 100. Returns the offset and limit.
pub(crate) fn json_page(page: Option<u32>, per_page: Option<u32>) -> ApiResult<(usize, usize)> {
    let page = usize::try_from(page.unwrap_or(1)).map_err(|_| page_error())?;
    let per_page =
        usize::try_from(per_page.unwrap_or(DEFAULT_PER_PAGE as u32)).map_err(|_| page_error())?;
    page_window(page, per_page).ok_or_else(page_error)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pages_use_an_offset_and_clamp_page_size() {
        assert_eq!(page_window(1, 30), Some((0, 30)));
        assert_eq!(page_window(2, 30), Some((30, 30)));
        assert_eq!(
            page_window(2, MAX_COMMIT_PAGE_SIZE),
            Some((MAX_COMMIT_PAGE_SIZE, MAX_COMMIT_PAGE_SIZE))
        );
        assert_eq!(
            page_window(2, MAX_COMMIT_PAGE_SIZE + 1),
            Some((MAX_COMMIT_PAGE_SIZE, MAX_COMMIT_PAGE_SIZE))
        );
        assert_eq!(page_window(1, 0), Some((0, 1)));
    }

    #[test]
    fn pages_reject_out_of_range_and_overflowing_offsets() {
        assert_eq!(page_window(0, 30), None);
        assert_eq!(page_window(MAX_PAGE, 30), Some((29_970, 30)));
        assert_eq!(page_window(MAX_PAGE + 1, 30), None);
        assert_eq!(page_offset(usize::MAX, MAX_COMMIT_PAGE_SIZE), None);
    }

    #[test]
    fn next_page_needs_an_extra_row_and_stops_at_the_cap() {
        assert!(has_next_page(1, 31, 30, MAX_PAGE));
        assert!(!has_next_page(1, 30, 30, MAX_PAGE));
        assert!(has_next_page(MAX_PAGE - 1, 31, 30, MAX_PAGE));
        assert!(!has_next_page(MAX_PAGE, 31, 30, MAX_PAGE));
    }
}
