use super::{database_error, missing_owner, owner_name, repository_cards, unavailable};
use crate::backend::db::repositories::records::RepositorySort;
use crate::backend::{api::context::PageContext, app::AppState, db};
use crate::webui::app::Page as AppPage;
use crate::webui::pages::discovery::{self as view, ExploreModel};
use axum::{
    extract::State,
    http::{HeaderMap, StatusCode, Uri},
    response::Response,
};
use std::{collections::HashMap, sync::Arc};

use crate::backend::api::error::ApiError;
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
    let mut responses = Vec::with_capacity(repos.len());
    for repo in repos {
        let owner = owner_name(&repo, &owners).map_err(|_| missing_owner())?;
        responses.push(RepoResponse {
            id: repo.id,
            owner_id: repo.owner_id(),
            owner: owner.clone(),
            name: repo.name.clone(),
            full_name: format!("{}/{}", owner, repo.name),
            description: repo.description,
            is_private: repo.is_private,
            is_archived: repo.is_archived,
            allow_merge_commit: repo.allow_merge_commit,
            allow_squash_merge: repo.allow_squash_merge,
            allow_rebase_merge: repo.allow_rebase_merge,
            default_branch: repo.default_branch,
            stars_count: repo.stars_count,
            forks_count: repo.forks_count,
            watchers_count: repo.watchers_count,
            forked_from: None,
            created_at: repo.created_at.to_rfc3339(),
            updated_at: repo.updated_at.to_rfc3339(),
        });
    }

    Ok(Json(responses))
}

/// Only "stars" changes the order; any other value is newest first.
fn repository_sort(sort: Option<&str>) -> RepositorySort {
    match sort {
        Some("stars") => RepositorySort::Stars,
        _ => RepositorySort::Recent,
    }
}

#[derive(Debug, PartialEq)]
struct ExploreFilters {
    sort: String,
    limit: usize,
    query: String,
}

fn explore_filters(query: &HashMap<String, String>) -> Result<ExploreFilters, &'static str> {
    let sort = query.get("sort").map(String::as_str).unwrap_or("recent");
    if !matches!(sort, "recent" | "stars") {
        return Err("Unknown repository sort. Choose recent or stars.");
    }
    let limit = match query.get("limit") {
        None => 50,
        Some(raw) => raw
            .parse::<usize>()
            .ok()
            .filter(|limit| (1..=100).contains(limit))
            .ok_or("List size must be between 1 and 100.")?,
    };
    let text = query.get("q").map(|text| text.trim()).unwrap_or_default();
    if text.len() > 256 {
        return Err("Filter text must be at most 256 bytes.");
    }
    Ok(ExploreFilters {
        sort: sort.into(),
        limit,
        query: text.to_owned(),
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
    let repos = match db::repositories::records::search_public_repositories(
        &ctx.state.pool,
        &filters.query,
        repository_sort(Some(filters.sort.as_str())),
        filters.limit as i64,
    )
    .await
    {
        Ok(repos) => repos,
        Err(_) => return unavailable(&ctx, "explore_repositories"),
    };
    let repositories = match repository_cards(&ctx.state, repos).await {
        Ok(cards) => cards,
        Err(_) => return unavailable(&ctx, "explore_owners"),
    };
    let loaded_count = repositories.len();
    ctx.page_model(
        "Explore repositories",
        AppPage::Discovery(view::Page::Explore(ExploreModel {
            sort: filters.sort,
            limit: filters.limit,
            query: filters.query,
            loaded_count,
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
    fn explore_filters_are_bounded_and_only_supported_sorts_are_accepted() {
        assert_eq!(
            explore_filters(&HashMap::new()).unwrap(),
            ExploreFilters {
                sort: "recent".into(),
                limit: 50,
                query: String::new()
            }
        );
        assert_eq!(
            explore_filters(&query(&[
                ("sort", "stars"),
                ("limit", "100"),
                ("q", "rust")
            ]))
            .unwrap()
            .limit,
            100
        );
        for value in ["0", "101", "-1", "abc", "9999999999999999999999999"] {
            assert!(explore_filters(&query(&[("limit", value)])).is_err());
        }
        assert_eq!(
            explore_filters(&query(&[("q", "  Rust  ")])).unwrap().query,
            "Rust"
        );
        assert!(explore_filters(&query(&[("sort", "updated")])).is_err());
        assert!(explore_filters(&query(&[("q", &"x".repeat(257))])).is_err());
    }
}
