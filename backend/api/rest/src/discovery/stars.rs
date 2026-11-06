use crate::discovery::page_number;
use crate::discovery::repository_cards;
use crate::discovery::unavailable;
use crate::discovery::DiscoveryReadError;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::http::StatusCode;
use axum::http::Uri;
use axum::response::Response;
use codeyard_api_http::context::PageContext;
use codeyard_app::app::AppState;
use codeyard_db_repos::repositories::engagement::StarSort;
use codeyard_webui::app::Page as AppPage;
use codeyard_webui::pages::discovery as view;
use codeyard_webui::pages::discovery::RepositoryCard;
use std::collections::HashMap;
use std::sync::Arc;

/// Starred repositories per page, on /stars and on a profile's Stars tab.
pub(crate) const STARS_PAGE_SIZE: usize = 50;

/// One page of starred repositories as cards. `has_older` says whether a page after it has more.
pub(crate) struct StarredPage {
    pub repositories: Vec<RepositoryCard>,
    pub has_older: bool,
}

/// One page (1-based, at least 1) of the repositories `starred_by` starred whose name or
/// description contains `text` and that `viewer` can read.
async fn local_stars(
    ctx: &PageContext,
    target: i64,
    text: &str,
    sort: StarSort,
    limit: usize,
    offset: usize,
) -> Result<Vec<serde_json::Value>, DiscoveryReadError> {
    let viewer = ctx.user.as_ref().map(|viewer| viewer.id);
    let repos = codeyard_db_repos::repositories::engagement::list_starred_repositories(
        &ctx.state.pool, target, viewer, text, sort, limit as i64, offset as i64,
    ).await.map_err(|_| DiscoveryReadError::Unavailable)?;
    let mut times = HashMap::with_capacity(repos.len());
    for repo in &repos {
        if let Some(at) = codeyard_db_repos::repositories::engagement::starred_at(&ctx.state.pool, target, repo.id)
            .await.map_err(|_| DiscoveryReadError::Unavailable)? { times.insert(repo.id, at); }
    }
    let cards = repository_cards(&ctx.state, viewer, repos).await?;
    Ok(cards.into_iter().filter_map(|repo| times.remove(&repo.id).map(|at| serde_json::json!({"repo":repo,"starred_at":at}))).collect())
}

async fn remote_stars(
    ctx: &PageContext,
    target: &codeyard_db_core::models::User,
    text: &str,
    sort: StarSort,
    wanted: usize,
) -> Result<Option<codeyard_db_core::cells::pointers::PageResult>, DiscoveryReadError> {
    let actor = ctx.user.as_ref().map(|user| ctx.database_actor(user.id));
    codeyard_app::api_shared::github::pointers::stars(
        &ctx.state.pool, target, actor.as_ref(), ctx.session_id.as_deref(),
        codeyard_db_core::cells::pointers::List::BrowserStars,
        vec![("username".into(),target.username.clone()),("q".into(),text.into()),("sort".into(),match sort { StarSort::Stars=>"stars",StarSort::Name=>"name",_=>"created" }.into())], wanted,
    ).await.map_err(|_| DiscoveryReadError::Unavailable)
}

pub(crate) async fn starred_count(
    ctx: &PageContext,
    target: &codeyard_db_core::models::User,
) -> Result<i64, DiscoveryReadError> {
    let local = codeyard_db_repos::repositories::engagement::count_starred_repositories(
        &ctx.state.pool, target.id, ctx.user.as_ref().map(|user| user.id), "",
    ).await.map_err(|_| DiscoveryReadError::Unavailable)?;
    match remote_stars(ctx, target, "", StarSort::Recent, 1).await? {
        None => Ok(local),
        Some(remote) => remote.total.and_then(|count| local.checked_add(count)).ok_or(DiscoveryReadError::Unavailable),
    }
}

pub(crate) async fn starred_page(
    ctx: &PageContext,
    target: &codeyard_db_core::models::User,
    text: &str,
    sort: StarSort,
    page: usize,
) -> Result<StarredPage, DiscoveryReadError> {
    let offset = page.checked_sub(1).and_then(|n| n.checked_mul(STARS_PAGE_SIZE)).ok_or(DiscoveryReadError::Unavailable)?;
    let wanted = offset.checked_add(STARS_PAGE_SIZE + 1).ok_or(DiscoveryReadError::Unavailable)?;
    let remote = remote_stars(ctx, target, text, sort, wanted).await?;
    let mut rows = local_stars(ctx, target.id, text, sort,
        if remote.is_some() { wanted } else { STARS_PAGE_SIZE + 1 },
        if remote.is_some() { 0 } else { offset },
    ).await?;
    let has_older = if let Some(remote) = remote {
        rows.extend(remote.rows);
        // Cards use their existing field names; retain each star's timestamp for ordering.
        for row in &mut rows {
            let card = &mut row["repo"];
            card["stargazers_count"] = card["stars"].clone();
            card["full_name"] = card["name"].clone();
        }
        let (page, more) = codeyard_app::api_shared::github::pointers::page(rows,
            match sort { StarSort::Stars=>"stars",StarSort::Name=>"full_name",_=>"starred" },
            sort != StarSort::Name, offset, STARS_PAGE_SIZE,
        ).map_err(|_| DiscoveryReadError::Unavailable)?;
        rows = page;
        more
    } else {
        let more = rows.len() > STARS_PAGE_SIZE;
        rows.truncate(STARS_PAGE_SIZE);
        more
    };
    let repositories = rows.into_iter().map(|row| serde_json::from_value(row["repo"].clone()))
        .collect::<Result<Vec<RepositoryCard>,_>>().map_err(|_| DiscoveryReadError::Unavailable)?;
    Ok(StarredPage { repositories, has_older })
}

/// Resource-cell projection for a home user's star list. Stub identities remain internal.
pub(super) async fn star_rows(
    State(state): State<Arc<AppState>>,
    axum::extract::Path(username): axum::extract::Path<String>,
    headers: HeaderMap,
    uri: Uri,
) -> Response {
    use axum::response::IntoResponse;
    let ctx = match PageContext::load(state, headers, &uri).await { Ok(ctx)=>ctx,Err(response)=>return response };
    let page = match page_number(&ctx.query) { Ok(page)=>page,Err(message)=>return ctx.error(StatusCode::BAD_REQUEST,message) };
    let size = match ctx.query.get("per_page").map_or(Some(100),|value| value.parse::<usize>().ok()).filter(|size| (1..=100).contains(size)) {
        Some(size)=>size,None=>return ctx.error(StatusCode::BAD_REQUEST,"Invalid star page size"),
    };
    let Some(offset) = (page-1).checked_mul(size).filter(|offset| *offset <= 10000) else { return ctx.error(StatusCode::BAD_REQUEST,"Star page exceeds its limit"); };
    let sort = match ctx.query.get("sort").map(String::as_str).unwrap_or("created") {
        "created"=>StarSort::Recent,"stars"=>StarSort::Stars,"name"=>StarSort::Name,
        _=>return ctx.error(StatusCode::BAD_REQUEST,"Invalid star sort"),
    };
    let text = ctx.query.get("q").map(String::as_str).unwrap_or("");
    if text.len()>256 || text.chars().any(char::is_control) { return ctx.error(StatusCode::BAD_REQUEST,"Invalid star filter"); }
    let target = match codeyard_db_identity::users::accounts::get_user_by_username(&ctx.state.pool,&username).await {
        Ok(Some(target))=>target,Ok(None)=>return ctx.error(StatusCode::NOT_FOUND,"User not found"),Err(_)=>return unavailable(&ctx,"star_target"),
    };
    let rows = match local_stars(&ctx,target.id,text,sort,size,offset).await { Ok(rows)=>rows,Err(_)=>return unavailable(&ctx,"star_rows") };
    let total = if page==1 {
        match codeyard_db_repos::repositories::engagement::count_starred_repositories(&ctx.state.pool,target.id,ctx.user.as_ref().map(|user|user.id),text).await {
            Ok(total)=>Some(total),Err(_)=>return unavailable(&ctx,"star_count"),
        }
    } else { None };
    axum::Json(serde_json::json!({"rows":rows,"total":total})).into_response()
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
        &ctx,
        user,
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
