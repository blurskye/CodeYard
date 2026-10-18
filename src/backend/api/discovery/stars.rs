use super::{page_number, repository_cards, unavailable, DiscoveryReadError};
use crate::backend::{
    api::http::context::PageContext,
    app::AppState,
    db::{self, repositories::engagement::StarSort},
};
use crate::webui::app::Page as AppPage;
use crate::webui::pages::discovery::{self as view, RepositoryCard};
use axum::{
    extract::State,
    http::{HeaderMap, StatusCode, Uri},
    response::Response,
};
use std::{collections::HashMap, sync::Arc};

/// Starred repositories per page, on /stars and on a profile's Stars tab.
pub(crate) const STARS_PAGE_SIZE: usize = 50;

/// One page of starred repositories as cards. `has_older` says whether a page after it has more.
pub(crate) struct StarredPage {
    pub repositories: Vec<RepositoryCard>,
    pub has_older: bool,
}

/// One page (1-based, at least 1) of the repositories `starred_by` starred whose name or
/// description contains `text` and that `viewer` can read.
pub(crate) async fn starred_page(
    state: &AppState,
    starred_by: i64,
    viewer: Option<i64>,
    text: &str,
    sort: StarSort,
    page: usize,
) -> Result<StarredPage, DiscoveryReadError> {
    let offset = i64::try_from((page - 1).saturating_mul(STARS_PAGE_SIZE)).unwrap_or(i64::MAX);
    // One row past the page shows whether an older page exists.
    let mut repos = db::repositories::engagement::list_starred_repositories(
        &state.pool,
        starred_by,
        viewer,
        text,
        sort,
        STARS_PAGE_SIZE as i64 + 1,
        offset,
    )
    .await
    .map_err(|_| DiscoveryReadError::Unavailable)?;
    let has_older = repos.len() > STARS_PAGE_SIZE;
    repos.truncate(STARS_PAGE_SIZE);
    Ok(StarredPage {
        repositories: repository_cards(state, repos).await?,
        has_older,
    })
}

/// The filters of /stars: the order, the page, and the text the name or description must contain.
#[derive(Debug, PartialEq)]
pub(crate) struct StarFilters {
    pub(crate) sort: String,
    pub(crate) page: usize,
    pub(crate) query: String,
}

/// The order and text a Stars list is narrowed to, as its address names them.
pub(crate) fn star_order_and_text(
    query: &HashMap<String, String>,
) -> Result<(String, String), &'static str> {
    let sort = query.get("sort").map(String::as_str).unwrap_or("recent");
    if !matches!(sort, "recent" | "stars" | "name") {
        return Err("Unknown star sort. Choose recent, stars or name.");
    }
    let text = query.get("q").map(|text| text.trim()).unwrap_or_default();
    if text.len() > 256 {
        return Err("Filter text must be at most 256 bytes.");
    }
    Ok((sort.into(), text.to_owned()))
}

fn star_filters(query: &HashMap<String, String>) -> Result<StarFilters, &'static str> {
    let (sort, text) = star_order_and_text(query)?;
    Ok(StarFilters {
        sort,
        page: page_number(query)?,
        query: text,
    })
}

pub(crate) fn star_sort(sort: &str) -> StarSort {
    match sort {
        "stars" => StarSort::Stars,
        "name" => StarSort::Name,
        _ => StarSort::Recent,
    }
}

pub(super) async fn stars_page(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    uri: Uri,
) -> Response {
    let ctx = match PageContext::load(state, headers, &uri).await {
        Ok(ctx) => ctx,
        Err(response) => return response,
    };
    let user = match ctx.require_user() {
        Ok(user) => user,
        Err(response) => return response,
    };
    let filters = match star_filters(&ctx.query) {
        Ok(filters) => filters,
        Err(message) => return ctx.error(StatusCode::BAD_REQUEST, message),
    };
    let page = match starred_page(
        &ctx.state,
        user.id,
        Some(user.id),
        &filters.query,
        star_sort(&filters.sort),
        filters.page,
    )
    .await
    {
        Ok(page) => page,
        Err(_) => return unavailable(&ctx, "stars"),
    };
    ctx.page_model(
        "Your stars",
        AppPage::Discovery(view::Page::Stars(view::StarsModel {
            sort: filters.sort,
            query: filters.query,
            page: filters.page,
            has_older: page.has_older,
            repositories: page.repositories,
            csrf_token: ctx.csrf_token.clone(),
        })),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn query(values: &[(&str, &str)]) -> HashMap<String, String> {
        values
            .iter()
            .map(|(key, value)| ((*key).into(), (*value).into()))
            .collect()
    }

    #[test]
    fn star_filters_start_with_the_latest_star_on_the_first_page() {
        assert_eq!(
            star_filters(&HashMap::new()).unwrap(),
            StarFilters {
                sort: "recent".into(),
                page: 1,
                query: String::new(),
            }
        );
    }

    #[test]
    fn star_filters_accept_three_sorts_and_refuse_others() {
        for sort in ["recent", "stars", "name"] {
            assert_eq!(star_filters(&query(&[("sort", sort)])).unwrap().sort, sort);
        }
        assert!(star_filters(&query(&[("sort", "updated")])).is_err());
        assert_eq!(star_sort("stars"), StarSort::Stars);
        assert_eq!(star_sort("name"), StarSort::Name);
        assert_eq!(star_sort("recent"), StarSort::Recent);
    }

    #[test]
    fn star_filters_trim_the_text_bound_its_length_and_validate_the_page() {
        assert_eq!(
            star_filters(&query(&[("q", "  rust  "), ("page", "2")])).unwrap(),
            StarFilters {
                sort: "recent".into(),
                page: 2,
                query: "rust".into(),
            }
        );
        assert!(star_filters(&query(&[("q", &"x".repeat(257))])).is_err());
        assert!(star_filters(&query(&[("page", "0")])).is_err());
    }
}
