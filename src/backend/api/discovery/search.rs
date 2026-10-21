use super::{
    content_search, database_error, missing_owner, owner_name, page_number, repository_cards,
    unavailable, DiscoveryReadError,
};
use crate::backend::{
    api::http::context::PageContext,
    api::http::session::get_optional_user,
    app::AppState,
    db,
    db::repositories::{discovery::ItemSort, records::RepositorySort},
};
use crate::webui::app::Page as AppPage;
use crate::webui::components::body_preview;
use crate::webui::pages::discovery::{
    self as view, IssueCard, SearchCounts, SearchModel, UserCard, SEARCH_COUNT_CAP,
};
use axum::{
    extract::State,
    http::{HeaderMap, StatusCode, Uri},
    response::Response,
};
use std::{collections::HashMap, sync::Arc};

use crate::backend::api::http::error::ApiError;
use crate::backend::api::repos::responses::RepoResponse;
use crate::backend::api::user::responses::PublicUserResponse as UserResponse;

use axum::{extract::Query, Json};
use serde::{Deserialize, Serialize};

/// Results per page of every search tab: repositories, issues, pull requests, people, code and
/// commits.
pub(super) const RESULTS_PER_PAGE: usize = 30;

static SEARCHES: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(4);

async fn admit_search() -> Result<tokio::sync::SemaphorePermit<'static>, DiscoveryReadError> {
    crate::backend::api::http::middleware::wait_for_static_slot(&SEARCHES)
        .await
        .ok_or(DiscoveryReadError::Unavailable)
}

fn badge_count(
    result: anyhow::Result<i64>,
    tab: &str,
    unavailable: &mut Vec<String>,
) -> Result<usize, DiscoveryReadError> {
    match result {
        Ok(count) => Ok(count_as_usize(count)),
        Err(error) => {
            let timed_out = error
                .downcast_ref::<sqlx::Error>()
                .and_then(sqlx::Error::as_database_error)
                .is_some_and(|error| matches!(error.code().as_deref(), Some("57014" | "9")));
            if !timed_out {
                return Err(DiscoveryReadError::Unavailable);
            }
            unavailable.push(tab.to_owned());
            Ok(0)
        }
    }
}

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

/// The JSON search returns the first page of each type it asks for, most relevant first.
pub async fn search(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Query(query): Query<SearchQuery>,
) -> Result<Json<SearchResponse>, (StatusCode, Json<ApiError>)> {
    let q = query.q.trim();
    if !bounded_search_text(q) {
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

    let _permit = admit_search().await.map_err(|_| {
        (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(ApiError::new(
                "unavailable",
                "Search is busy. Try again shortly.",
            )),
        )
    })?;
    let viewer = get_optional_user(&state.pool, &headers)
        .await?
        .map(|user| user.id);
    let limit = 50i64;
    let search_type = query.search_type.as_deref();

    // A missing or unknown type searches all three.
    let wants = |kind: &str| {
        search_type == Some(kind) || !matches!(search_type, Some("repos" | "issues" | "users"))
    };
    let repos = if wants("repos") {
        db::repositories::discovery::search_repositories(
            &state.pool,
            viewer,
            q,
            RepositorySort::Stars,
            0,
            limit,
        )
        .await
        .map_err(database_error)?
    } else {
        vec![]
    };
    let issues = if wants("issues") {
        db::repositories::discovery::search_issues(
            &state.pool,
            viewer,
            q,
            ItemSort::Updated,
            0,
            limit,
        )
        .await
        .map_err(database_error)?
    } else {
        vec![]
    };
    let users = if wants("users") {
        db::repositories::discovery::search_users(&state.pool, q, 0, limit)
            .await
            .map_err(database_error)?
    } else {
        vec![]
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
        .map(|repo| repo.id)
        .chain(issue_repos.keys().copied())
        .collect::<Vec<_>>();
    let owners = db::repositories::records::repository_owner_names(&state.pool, &ids)
        .await
        .map_err(database_error)?;
    let mut topics = db::repositories::topics::repository_topics(&state.pool, &ids)
        .await
        .map_err(database_error)?;
    let mut repo_responses = Vec::with_capacity(repos.len());
    for repo in repos {
        let owner = owner_name(&repo, &owners).map_err(|_| missing_owner())?;
        repo_responses.push(RepoResponse::new(
            &repo,
            &owner,
            topics.remove(&repo.id).unwrap_or_default(),
            None,
        ));
    }

    let author_ids = issues
        .iter()
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

/// What a search page asks for: the text, the result type named by `type` (none when the address
/// names no type), the sort and the 1-based page of results.
#[derive(Debug, PartialEq)]
struct SearchFilters {
    text: String,
    tab: Option<String>,
    sort: String,
    page: usize,
}

fn bounded_search_text(text: &str) -> bool {
    if text.len() <= 256 {
        return true;
    }
    // The phone picker appends one bounded repository scope to the user's search term.
    if text.len() > 256 + 1 + 5 + 512 {
        return false;
    }
    let mut scope_seen = false;
    let terms = text
        .split_whitespace()
        .filter(|word| {
            if !scope_seen
                && word
                    .strip_prefix("repo:")
                    .is_some_and(|scope| scope.len() <= 512 && scope.contains('/'))
            {
                scope_seen = true;
                false
            } else {
                true
            }
        })
        .collect::<Vec<_>>()
        .join(" ");
    terms.len() <= 256
}

fn search_filters(query: &HashMap<String, String>) -> Result<SearchFilters, &'static str> {
    let text = query.get("q").cloned().unwrap_or_default();
    if !bounded_search_text(&text) {
        return Err("Search text must be at most 256 bytes.");
    }
    let tab = match query.get("type").map(String::as_str) {
        None => None,
        Some(tab @ ("repos" | "code" | "commits" | "issues" | "pulls" | "users")) => {
            Some(tab.to_owned())
        }
        Some(_) => {
            return Err("Unknown search type. Choose repos, code, commits, issues, pulls or users.")
        }
    };
    let sort = query.get("sort").cloned().unwrap_or_default();
    if !matches!(
        sort.as_str(),
        "" | "stars" | "forks" | "updated" | "newest" | "oldest"
    ) {
        return Err("Unknown search sort. Choose stars, forks, updated, newest or oldest.");
    }
    let page = page_number(query)?;
    Ok(SearchFilters {
        text,
        tab,
        sort,
        page,
    })
}

/// The order a repository search asks for. Best match, which is most stars first, is the default.
fn repository_sort(sort: &str) -> RepositorySort {
    match sort {
        "forks" => RepositorySort::Forks,
        "updated" => RepositorySort::Updated,
        _ => RepositorySort::Stars,
    }
}

/// The order an issue or pull request search asks for. Recently updated is the default.
fn item_sort(sort: &str) -> ItemSort {
    match sort {
        "newest" => ItemSort::Newest,
        "oldest" => ItemSort::Oldest,
        _ => ItemSort::Updated,
    }
}

/// An issue or pull request the search found, with the parts its result card shows.
struct ItemRow {
    repo_id: i64,
    number: i32,
    title: String,
    state: String,
    author_id: i64,
    body: String,
    created: chrono::DateTime<chrono::Utc>,
    updated: chrono::DateTime<chrono::Utc>,
}

/// The issue or pull request cards for rows the search already filtered by what the viewer can
/// read.
async fn issue_cards(
    state: &AppState,
    rows: Vec<ItemRow>,
) -> Result<Vec<IssueCard>, DiscoveryReadError> {
    let ids = rows.iter().map(|row| row.repo_id).collect::<Vec<_>>();
    let repos = db::repositories::records::get_repositories_by_ids(&state.pool, &ids)
        .await
        .map_err(|_| DiscoveryReadError::Unavailable)?
        .into_iter()
        .map(|repo| (repo.id, repo))
        .collect::<HashMap<_, _>>();
    let ids = repos.keys().copied().collect::<Vec<_>>();
    let owners = db::repositories::records::repository_owner_names(&state.pool, &ids)
        .await
        .map_err(|_| DiscoveryReadError::Unavailable)?;
    let author_ids = rows.iter().map(|row| row.author_id).collect::<Vec<_>>();
    let authors = db::users::accounts::get_users_by_ids(&state.pool, &author_ids)
        .await
        .map_err(|_| DiscoveryReadError::Unavailable)?
        .into_iter()
        .map(|user| (user.id, user.username))
        .collect::<HashMap<_, _>>();
    let mut cards = Vec::with_capacity(rows.len());
    for row in rows {
        let repo = repos
            .get(&row.repo_id)
            .ok_or(DiscoveryReadError::MissingRepository)?;
        cards.push(IssueCard {
            owner: owner_name(repo, &owners)?,
            repo: repo.name.clone(),
            number: row.number,
            title: row.title,
            state: row.state,
            author: authors.get(&row.author_id).cloned().unwrap_or_default(),
            excerpt: body_preview(&row.body),
            created_at: row.created.format("%Y-%m-%dT%H:%M:%S").to_string(),
            updated_at: row.updated.format("%Y-%m-%d").to_string(),
        });
    }
    Ok(cards)
}

fn count_as_usize(count: i64) -> usize {
    usize::try_from(count).unwrap_or(usize::MAX)
}

/// One page of rows, read with one row past it. That row says whether an older page has rows.
fn take_page<T>(mut rows: Vec<T>) -> (Vec<T>, bool) {
    let has_older = rows.len() > RESULTS_PER_PAGE;
    rows.truncate(RESULTS_PER_PAGE);
    (rows, has_older)
}

async fn load_search(
    state: &AppState,
    viewer: Option<i64>,
    filters: SearchFilters,
) -> Result<SearchModel, DiscoveryReadError> {
    let _permit = admit_search().await?;
    let SearchFilters {
        text,
        tab,
        sort,
        page,
    } = filters;
    let mut model = SearchModel {
        query: text,
        sort: sort.clone(),
        page,
        ..Default::default()
    };
    let query = model.query.trim().to_owned();
    if query.is_empty() {
        model.tab = tab.unwrap_or_else(|| "repos".into());
        return Ok(model);
    }
    let unavailable = |_| DiscoveryReadError::Unavailable;
    // Explicit content searches do not scan unrelated tables for tab badges.
    let cap = SEARCH_COUNT_CAP as i64;
    let counts = if matches!(tab.as_deref(), Some("code" | "commits")) {
        SearchCounts::default()
    } else {
        SearchCounts {
            repositories: count_as_usize(
                db::repositories::discovery::count_repositories(&state.pool, viewer, &query, cap)
                    .await
                    .map_err(unavailable)?,
            ),
            issues: badge_count(
                db::repositories::discovery::count_issues(&state.pool, viewer, &query, cap).await,
                "issues",
                &mut model.counts_unavailable,
            )?,
            pulls: badge_count(
                db::repositories::discovery::count_pull_requests(&state.pool, viewer, &query, cap)
                    .await,
                "pulls",
                &mut model.counts_unavailable,
            )?,
            users: count_as_usize(
                db::repositories::discovery::count_users(&state.pool, &query, cap)
                    .await
                    .map_err(unavailable)?,
            ),
        }
    };
    model.tab = tab.unwrap_or_else(|| first_search_tab_with_results(&counts).into());
    let offset =
        i64::try_from(page.saturating_sub(1).saturating_mul(RESULTS_PER_PAGE)).unwrap_or(i64::MAX);
    let limit = RESULTS_PER_PAGE as i64 + 1;
    match model.tab.as_str() {
        "repos" => {
            let repos = db::repositories::discovery::search_repositories(
                &state.pool,
                viewer,
                &query,
                repository_sort(&sort),
                offset,
                limit,
            )
            .await
            .map_err(unavailable)?;
            let (repos, has_older) = take_page(repos);
            model.has_older = has_older;
            model.repositories = repository_cards(state, repos).await?;
        }
        "issues" => {
            let issues = db::repositories::discovery::search_issues(
                &state.pool,
                viewer,
                &query,
                item_sort(&sort),
                offset,
                limit,
            )
            .await
            .map_err(unavailable)?;
            let (issues, has_older) = take_page(issues);
            model.has_older = has_older;
            model.issues = issue_cards(
                state,
                issues
                    .into_iter()
                    .map(|issue| ItemRow {
                        repo_id: issue.repo_id,
                        number: issue.number,
                        title: issue.title,
                        state: issue.state,
                        author_id: issue.author_id,
                        body: issue.body,
                        created: issue.created_at,
                        updated: issue.updated_at,
                    })
                    .collect(),
            )
            .await?;
        }
        "pulls" => {
            let pulls = db::repositories::discovery::search_pull_requests(
                &state.pool,
                viewer,
                &query,
                item_sort(&sort),
                offset,
                limit,
            )
            .await
            .map_err(unavailable)?;
            let (pulls, has_older) = take_page(pulls);
            model.has_older = has_older;
            model.pulls = issue_cards(
                state,
                pulls
                    .into_iter()
                    .map(|pull| ItemRow {
                        repo_id: pull.repo_id,
                        number: pull.number,
                        title: pull.title,
                        state: if pull.merged {
                            "merged".into()
                        } else {
                            pull.state
                        },
                        author_id: pull.author_id,
                        body: pull.body,
                        created: pull.created_at,
                        updated: pull.updated_at,
                    })
                    .collect(),
            )
            .await?;
        }
        "users" => {
            let users =
                db::repositories::discovery::search_users(&state.pool, &query, offset, limit)
                    .await
                    .map_err(unavailable)?;
            let (users, has_older) = take_page(users);
            model.has_older = has_older;
            model.users = users
                .into_iter()
                .map(|user| UserCard {
                    username: user.username,
                    display_name: user.display_name,
                    bio: user.bio,
                })
                .collect();
        }
        // Code and commits: the tab is one of the two, since the filters accept no other.
        _ => {
            let content =
                content_search::search_content(state, viewer, &query, model.tab == "code", page)
                    .await?;
            model.highlight = content.needle;
            model.commits = content.commits;
            model.code = content.code;
            model.has_older = content.has_older;
            model.scan = content.scan;
            model.unsearched = content.unsearched;
            model.truncated = content.truncated;
        }
    }
    model.counts = counts;
    Ok(model)
}

/// Without an explicit `type`, a search opens the first tab that has results, so a search that
/// only matches people does not open on an empty repository list. Commits and code are searched
/// only when their tab is open.
fn first_search_tab_with_results(counts: &SearchCounts) -> &'static str {
    if counts.repositories > 0 {
        "repos"
    } else if counts.issues > 0 {
        "issues"
    } else if counts.pulls > 0 {
        "pulls"
    } else if counts.users > 0 {
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
    let filters = match search_filters(&ctx.query) {
        Ok(filters) => filters,
        Err(message) => return ctx.error(StatusCode::BAD_REQUEST, message),
    };
    let viewer = ctx.user.as_ref().map(|user| user.id);
    match load_search(&ctx.state, viewer, filters).await {
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
        let filters = search_filters(&query(&[
            ("q", "rust + & % _"),
            ("type", "pulls"),
            ("sort", "newest"),
        ]))
        .unwrap();
        assert_eq!(
            filters,
            SearchFilters {
                text: "rust + & % _".into(),
                tab: Some("pulls".into()),
                sort: "newest".into(),
                page: 1,
            }
        );
        assert_eq!(
            search_filters(&HashMap::new()).unwrap(),
            SearchFilters {
                text: String::new(),
                tab: None,
                sort: String::new(),
                page: 1,
            }
        );
        for tab in ["repos", "code", "commits", "issues", "pulls", "users"] {
            assert!(search_filters(&query(&[("type", tab)])).is_ok(), "{tab}");
        }
        assert!(search_filters(&query(&[("type", "all")])).is_err());
        assert!(search_filters(&query(&[("sort", "best")])).is_err());
        assert!(search_filters(&query(&[("q", &"x".repeat(257))])).is_err());
    }

    #[test]
    fn repository_scope_has_a_separate_bounded_allowance() {
        let scope = format!("repo:{}/r", "a".repeat(250));
        assert!(bounded_search_text(&format!("x {scope}")));
        assert!(bounded_search_text(&format!("{} {scope}", "x".repeat(256))));
        assert!(!bounded_search_text(&format!(
            "{} {scope}",
            "x".repeat(257)
        )));
        assert!(!bounded_search_text(&format!(
            "x repo:{}/r",
            "a".repeat(513)
        )));
    }

    #[test]
    fn search_pages_start_at_one_and_refuse_zero_or_non_numbers() {
        assert_eq!(
            search_filters(&query(&[("type", "code"), ("page", "3")]))
                .unwrap()
                .page,
            3
        );
        for value in ["0", "-1", "abc", "4294967296"] {
            assert!(
                search_filters(&query(&[("page", value)])).is_err(),
                "{value}"
            );
        }
    }

    #[test]
    fn each_tab_maps_its_own_sorts_and_falls_back_to_the_default() {
        assert_eq!(repository_sort("forks"), RepositorySort::Forks);
        assert_eq!(repository_sort("updated"), RepositorySort::Updated);
        assert_eq!(repository_sort("newest"), RepositorySort::Stars);
        assert_eq!(item_sort("oldest"), ItemSort::Oldest);
        assert_eq!(item_sort("newest"), ItemSort::Newest);
        assert_eq!(item_sort("stars"), ItemSort::Updated);
    }

    #[tokio::test(start_paused = true)]
    async fn search_admission_waits_briefly_then_fails() {
        let permits = SEARCHES.try_acquire_many(4).unwrap();
        assert!(admit_search().await.is_err());
        drop(permits);
        assert!(admit_search().await.is_ok());
    }

    #[test]
    fn search_opens_the_first_tab_with_results() {
        let mut counts = SearchCounts::default();
        assert_eq!(first_search_tab_with_results(&counts), "repos");
        counts.users = 1;
        assert_eq!(first_search_tab_with_results(&counts), "users");
        counts.pulls = 1;
        assert_eq!(first_search_tab_with_results(&counts), "pulls");
        counts.issues = 1;
        assert_eq!(first_search_tab_with_results(&counts), "issues");
        counts.repositories = 1;
        assert_eq!(first_search_tab_with_results(&counts), "repos");
    }
}
