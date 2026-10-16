use super::{
    database_error, missing_owner, owner_name, repository_cards, unavailable, DiscoveryReadError,
};
use crate::backend::{api::context::PageContext, app::AppState, db};
use crate::webui::app::Page as AppPage;
use crate::webui::pages::discovery::{self as view, IssueCard, SearchModel, UserCard};
use axum::{
    extract::State,
    http::{HeaderMap, StatusCode, Uri},
    response::Response,
};
use std::{collections::HashMap, sync::Arc};

use crate::backend::api::error::ApiError;
use crate::backend::api::repos::responses::RepoResponse;
use crate::backend::api::user::responses::PublicUserResponse as UserResponse;

use axum::{extract::Query, Json};
use serde::{Deserialize, Serialize};

#[derive(Deserialize)]
pub struct SearchQuery {
    pub q: String,
    #[serde(rename = "type")]
    pub search_type: Option<String>,
}

#[derive(Serialize)]
pub struct SearchIssueResponse {
    pub id: i64,
    pub repo_id: i64,
    pub repo_name: Option<String>,
    pub repo_owner: Option<String>,
    pub number: i32,
    pub title: String,
    pub state: String,
    pub author: Option<UserResponse>,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Serialize)]
pub struct SearchResponse {
    pub repos: Vec<RepoResponse>,
    pub issues: Vec<SearchIssueResponse>,
    pub users: Vec<UserResponse>,
    pub repos_count: usize,
    pub issues_count: usize,
    pub users_count: usize,
}

pub async fn search(
    State(state): State<Arc<AppState>>,
    Query(query): Query<SearchQuery>,
) -> Result<Json<SearchResponse>, (StatusCode, Json<ApiError>)> {
    let q = query.q.trim();
    if q.len() > 256 {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(ApiError::new(
                "validation_error",
                "Search text must be at most 256 bytes.",
            )),
        ));
    }

    if q.is_empty() {
        return Ok(Json(SearchResponse {
            repos: vec![],
            issues: vec![],
            users: vec![],
            repos_count: 0,
            issues_count: 0,
            users_count: 0,
        }));
    }

    let limit = 50i64;
    let search_type = query.search_type.as_deref();

    let (repos, issues, users) = match search_type {
        Some("repos") => {
            let repos = db::repositories::discovery::search_repositories(&state.pool, q, limit)
                .await
                .map_err(database_error)?;
            (repos, vec![], vec![])
        }
        Some("issues") => {
            let issues = db::repositories::discovery::search_issues(&state.pool, q, limit)
                .await
                .map_err(database_error)?;
            (vec![], issues, vec![])
        }
        Some("users") => {
            let users = db::repositories::discovery::search_users(&state.pool, q, limit)
                .await
                .map_err(database_error)?;
            (vec![], vec![], users)
        }
        _ => {
            let repos = db::repositories::discovery::search_repositories(&state.pool, q, limit)
                .await
                .map_err(database_error)?;
            let issues = db::repositories::discovery::search_issues(&state.pool, q, limit)
                .await
                .map_err(database_error)?;
            let users = db::repositories::discovery::search_users(&state.pool, q, limit)
                .await
                .map_err(database_error)?;
            (repos, issues, users)
        }
    };

    let ids = issues.iter().map(|issue| issue.repo_id).collect::<Vec<_>>();
    let issue_repos = db::repositories::records::get_repositories_by_ids(&state.pool, &ids)
        .await
        .map_err(database_error)?
        .into_iter()
        .map(|repo| (repo.id, repo))
        .collect::<HashMap<_, _>>();
    let ids = repos
        .iter()
        .chain(issue_repos.values().filter(|repo| !repo.is_private))
        .map(|repo| repo.id)
        .collect::<Vec<_>>();
    let owners = db::repositories::records::repository_owner_names(&state.pool, &ids)
        .await
        .map_err(database_error)?;
    let mut repo_responses = Vec::with_capacity(repos.len());
    for repo in repos {
        let owner = owner_name(&repo, &owners).map_err(|_| missing_owner())?;
        repo_responses.push(RepoResponse {
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

    let author_ids = issues
        .iter()
        .filter(|issue| {
            issue_repos
                .get(&issue.repo_id)
                .is_some_and(|repo| !repo.is_private)
        })
        .map(|issue| issue.author_id)
        .collect::<Vec<_>>();
    let authors = db::users::accounts::get_users_by_ids(&state.pool, &author_ids)
        .await
        .map_err(database_error)?
        .into_iter()
        .map(|user| (user.id, UserResponse::from(user)))
        .collect::<HashMap<_, _>>();
    let mut issue_responses = Vec::with_capacity(issues.len());
    for issue in issues {
        let Some(repo) = issue_repos.get(&issue.repo_id) else {
            continue;
        };
        if repo.is_private {
            continue;
        }
        let repo_owner = owner_name(repo, &owners).map_err(|_| missing_owner())?;
        let author = authors.get(&issue.author_id).cloned();
        issue_responses.push(SearchIssueResponse {
            id: issue.id,
            repo_id: issue.repo_id,
            repo_name: Some(repo.name.clone()),
            repo_owner: Some(repo_owner),
            number: issue.number,
            title: issue.title,
            state: issue.state,
            author,
            created_at: issue.created_at.to_rfc3339(),
            updated_at: issue.updated_at.to_rfc3339(),
        });
    }

    let user_responses: Vec<UserResponse> = users.into_iter().map(UserResponse::from).collect();

    Ok(Json(SearchResponse {
        repos_count: repo_responses.len(),
        issues_count: issue_responses.len(),
        users_count: user_responses.len(),
        repos: repo_responses,
        issues: issue_responses,
        users: user_responses,
    }))
}

/// The search text and the tab named by `type`. `None` means the URL names no tab.
fn search_filters(
    query: &HashMap<String, String>,
) -> Result<(String, Option<String>), &'static str> {
    let text = query.get("q").cloned().unwrap_or_default();
    if text.len() > 256 {
        return Err("Search text must be at most 256 bytes.");
    }
    let tab = match query.get("type").map(String::as_str) {
        None => None,
        Some(tab @ ("repos" | "issues" | "users")) => Some(tab.to_owned()),
        Some(_) => return Err("Unknown search type. Choose repos, issues or users."),
    };
    Ok((text, tab))
}

async fn load_search(
    state: &AppState,
    text: String,
    tab: Option<String>,
) -> Result<SearchModel, DiscoveryReadError> {
    let mut model = SearchModel {
        query: text,
        tab: tab.clone().unwrap_or_else(|| "repos".into()),
        ..Default::default()
    };
    let query = model.query.trim();
    if query.is_empty() {
        return Ok(model);
    }
    // Each category has its existing independent bound. No count is advertised as a total.
    let repos = db::repositories::discovery::search_repositories(&state.pool, query, 50)
        .await
        .map_err(|_| DiscoveryReadError::Unavailable)?;
    let issues = db::repositories::discovery::search_issues(&state.pool, query, 50)
        .await
        .map_err(|_| DiscoveryReadError::Unavailable)?;
    let users = db::repositories::discovery::search_users(&state.pool, query, 50)
        .await
        .map_err(|_| DiscoveryReadError::Unavailable)?;
    model.repositories = repository_cards(state, repos).await?;
    let ids = issues.iter().map(|issue| issue.repo_id).collect::<Vec<_>>();
    let issue_repos = db::repositories::records::get_repositories_by_ids(&state.pool, &ids)
        .await
        .map_err(|_| DiscoveryReadError::Unavailable)?
        .into_iter()
        .map(|repo| (repo.id, repo))
        .collect::<HashMap<_, _>>();
    let ids = issue_repos
        .values()
        .filter(|repo| !repo.is_private)
        .map(|repo| repo.id)
        .collect::<Vec<_>>();
    let owners = db::repositories::records::repository_owner_names(&state.pool, &ids)
        .await
        .map_err(|_| DiscoveryReadError::Unavailable)?;
    for issue in issues {
        let repo = issue_repos
            .get(&issue.repo_id)
            .ok_or(DiscoveryReadError::MissingRepository)?;
        // Visibility can change between search and projection. Do not return a now-private result.
        if repo.is_private {
            continue;
        }
        let owner = owner_name(repo, &owners)?;
        model.issues.push(IssueCard {
            owner,
            repo: repo.name.clone(),
            number: issue.number,
            title: issue.title,
            state: issue.state,
            updated_at: issue.updated_at.format("%Y-%m-%d").to_string(),
        });
    }
    model.users = users
        .into_iter()
        .map(|user| UserCard {
            username: user.username,
            avatar_url: user
                .avatar_file_id
                .map(|id| format!("/api/files/{id}/download")),
            display_name: user.display_name,
            bio: user.bio,
        })
        .collect();
    if tab.is_none() {
        model.tab = first_search_tab_with_results(&model).into();
    }
    Ok(model)
}

/// Without an explicit `type`, a search opens the first tab that has results, so a search that
/// only matches people does not open on an empty repository list.
fn first_search_tab_with_results(model: &SearchModel) -> &'static str {
    if !model.repositories.is_empty() {
        "repos"
    } else if !model.issues.is_empty() {
        "issues"
    } else if !model.users.is_empty() {
        "users"
    } else {
        "repos"
    }
}

pub(super) async fn search_page(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    uri: Uri,
) -> Response {
    let ctx = match PageContext::load(state, headers, &uri).await {
        Ok(ctx) => ctx,
        Err(response) => return response,
    };
    let (text, tab) = match search_filters(&ctx.query) {
        Ok(filters) => filters,
        Err(message) => return ctx.error(StatusCode::BAD_REQUEST, message),
    };
    match load_search(&ctx.state, text, tab).await {
        Ok(model) => ctx.page_model("Search", AppPage::Discovery(view::Page::Search(model))),
        Err(_) => unavailable(&ctx, "search"),
    }
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
    fn search_uses_supported_tabs_and_preserves_the_query() {
        assert_eq!(
            search_filters(&query(&[("q", "rust + & % _"), ("type", "issues")])).unwrap(),
            ("rust + & % _".into(), Some("issues".into()))
        );
        assert_eq!(
            search_filters(&HashMap::new()).unwrap(),
            (String::new(), None)
        );
        assert!(search_filters(&query(&[("type", "all")])).is_err());
        assert!(search_filters(&query(&[("q", &"x".repeat(257))])).is_err());
    }

    #[test]
    fn search_opens_the_first_tab_with_results() {
        let mut model = SearchModel::default();
        assert_eq!(first_search_tab_with_results(&model), "repos");
        model.users.push(UserCard::default());
        assert_eq!(first_search_tab_with_results(&model), "users");
        model.issues.push(IssueCard::default());
        assert_eq!(first_search_tab_with_results(&model), "issues");
    }
}
