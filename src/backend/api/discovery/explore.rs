use super::{
    database_error, missing_owner, owner_name, page_number, repository_cards, unavailable,
};
use crate::backend::db::repositories::records::{RepositoryFilter, RepositoryKind, RepositorySort};
use crate::backend::{api::http::context::PageContext, app::AppState, db};
use crate::webui::app::Page as AppPage;
use crate::webui::pages::discovery::{self as view, ExploreModel};
use axum::{
    extract::State,
    http::{HeaderMap, StatusCode, Uri},
    response::Response,
};
use std::{collections::HashMap, sync::Arc};

use crate::backend::api::http::error::ApiError;
use crate::backend::api::repos::responses::RepoResponse;

use axum::{extract::Query, Json};
use serde::Deserialize;

#[derive(Deserialize)]
pub struct ExploreQuery {
    pub sort: Option<String>,
    pub limit: Option<u32>,
}

pub async fn explore_repos(
    State(state): State<Arc<AppState>>,
    Query(query): Query<ExploreQuery>,
) -> Result<Json<Vec<RepoResponse>>, (StatusCode, Json<ApiError>)> {
    let limit = query.limit.unwrap_or(50).clamp(1, 100) as i64;

    let repos = db::repositories::records::list_public_repositories(
        &state.pool,
        repository_sort(query.sort.as_deref()),
        limit,
    )
    .await
    .map_err(database_error)?;

    let ids = repos.iter().map(|repo| repo.id).collect::<Vec<_>>();
    let owners = db::repositories::records::repository_owner_names(&state.pool, &ids)
        .await
        .map_err(database_error)?;
    let mut topics = db::repositories::topics::repository_topics(&state.pool, &ids)
        .await
        .map_err(database_error)?;
    let mut responses = Vec::with_capacity(repos.len());
    for repo in repos {
        let owner = owner_name(&repo, &owners).map_err(|_| missing_owner())?;
        responses.push(RepoResponse::new(
            &repo,
            &owner,
            topics.remove(&repo.id).unwrap_or_default(),
            None,
        ));
    }

    Ok(Json(responses))
}

/// Any value but stars, forks or updated is newest first.
fn repository_sort(sort: Option<&str>) -> RepositorySort {
    match sort {
        Some("stars") => RepositorySort::Stars,
        Some("forks") => RepositorySort::Forks,
        Some("updated") => RepositorySort::Updated,
        _ => RepositorySort::Recent,
    }
}

/// Repositories per page of Explore.
const EXPLORE_PAGE_SIZE: usize = 50;

#[derive(Debug, PartialEq)]
struct ExploreFilters {
    sort: String,
    page: usize,
    query: String,
    language: String,
    topic: String,
    kind: String,
}

fn repository_kind(kind: &str) -> RepositoryKind {
    match kind {
        "sources" => RepositoryKind::Sources,
        "forks" => RepositoryKind::Forks,
        "archived" => RepositoryKind::Archived,
        _ => RepositoryKind::All,
    }
}

fn explore_filters(query: &HashMap<String, String>) -> Result<ExploreFilters, &'static str> {
    let sort = query.get("sort").map(String::as_str).unwrap_or("recent");
    if !matches!(sort, "recent" | "updated" | "stars" | "forks") {
        return Err("Unknown repository sort. Choose recent, updated, stars or forks.");
    }
    let kind = query.get("type").map(String::as_str).unwrap_or("all");
    if !matches!(kind, "all" | "sources" | "forks" | "archived") {
        return Err("Unknown repository type. Choose all, sources, forks or archived.");
    }
    let language = query
        .get("language")
        .map(|text| text.trim())
        .unwrap_or_default();
    if language.len() > 100 {
        return Err("Language must be at most 100 bytes.");
    }
    // Topics are stored lowercase, so a typed name matches however it was capitalized.
    let topic = query
        .get("topic")
        .map(|text| text.trim().to_ascii_lowercase())
        .unwrap_or_default();
    let page = page_number(query)?;
    let text = query.get("q").map(|text| text.trim()).unwrap_or_default();
    if text.len() > 256 {
        return Err("Filter text must be at most 256 bytes.");
    }
    Ok(ExploreFilters {
        sort: sort.into(),
        page,
        query: text.to_owned(),
        language: language.to_owned(),
        topic,
        kind: kind.to_owned(),
    })
}

pub(super) async fn explore_page(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    uri: Uri,
) -> Response {
    let ctx = match PageContext::load(state, headers, &uri).await {
        Ok(ctx) => ctx,
        Err(response) => return response,
    };
    let filters = match explore_filters(&ctx.query) {
        Ok(filters) => filters,
        Err(message) => return ctx.error(StatusCode::BAD_REQUEST, message),
    };
    // One row past the page shows whether an older page exists.
    let offset = (filters.page as i64 - 1) * EXPLORE_PAGE_SIZE as i64;
    let mut repos = match db::repositories::records::search_public_repositories(
        &ctx.state.pool,
        &filters.query,
        RepositoryFilter {
            language: &filters.language,
            topic: &filters.topic,
            kind: repository_kind(&filters.kind),
        },
        repository_sort(Some(filters.sort.as_str())),
        EXPLORE_PAGE_SIZE as i64 + 1,
        offset,
    )
    .await
    {
        Ok(repos) => repos,
        Err(_) => return unavailable(&ctx, "explore_repositories"),
    };
    let has_older = repos.len() > EXPLORE_PAGE_SIZE;
    repos.truncate(EXPLORE_PAGE_SIZE);
    let repositories = match repository_cards(&ctx.state, repos).await {
        Ok(cards) => cards,
        Err(_) => return unavailable(&ctx, "explore_owners"),
    };
    let languages = db::repositories::records::public_languages(&ctx.state.pool, 100)
        .await
        .unwrap_or_default();
    ctx.page_model(
        "Explore repositories",
        AppPage::Discovery(view::Page::Explore(ExploreModel {
            sort: filters.sort,
            query: filters.query,
            language: filters.language,
            topic: filters.topic,
            kind: filters.kind,
            languages,
            page: filters.page,
            has_older,
            repositories,
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
    fn explore_filters_start_on_the_first_page_and_only_supported_sorts_are_accepted() {
        assert_eq!(
            explore_filters(&HashMap::new()).unwrap(),
            ExploreFilters {
                sort: "recent".into(),
                page: 1,
                query: String::new(),
                language: String::new(),
                topic: String::new(),
                kind: "all".into(),
            }
        );
        assert_eq!(
            explore_filters(&query(&[("topic", " Rust-Lang ")]))
                .unwrap()
                .topic,
            "rust-lang"
        );
        assert_eq!(
            explore_filters(&query(&[("sort", "stars"), ("page", "3"), ("q", "rust")]))
                .unwrap()
                .page,
            3
        );
        for value in ["0", "-1", "abc", "4294967296", "9999999999999999999999999"] {
            assert!(
                explore_filters(&query(&[("page", value)])).is_err(),
                "{value}"
            );
        }
        assert_eq!(
            explore_filters(&query(&[("q", "  Rust  ")])).unwrap().query,
            "Rust"
        );
        assert!(explore_filters(&query(&[("sort", "oldest")])).is_err());
        assert!(explore_filters(&query(&[("type", "mirrors")])).is_err());
        let narrowed =
            explore_filters(&query(&[("type", "forks"), ("language", " Rust ")])).unwrap();
        assert_eq!(
            (narrowed.kind.as_str(), narrowed.language.as_str()),
            ("forks", "Rust")
        );
        assert!(explore_filters(&query(&[("q", &"x".repeat(257))])).is_err());
    }
}
