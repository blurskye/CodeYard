//! Discovery routes and safe repository-card projection.
mod content_search;
pub(crate) mod explore;
mod home;
pub(crate) mod search;
pub(crate) mod stars;

use crate::backend::api::http::error::ApiError;
use crate::backend::{
    api::{http::context::PageContext, http::csrf, http::middleware},
    app::AppState,
    db,
};
use crate::webui::{components::languages::LanguageShare, pages::discovery::RepositoryCard};
use axum::{
    extract::DefaultBodyLimit, http::StatusCode, middleware::from_fn, response::Response,
    routing::get, Json, Router,
};
use std::{
    collections::{BTreeMap, HashMap},
    sync::Arc,
};

pub fn router() -> Router<Arc<AppState>> {
    let api = Router::new()
        .route("/api/explore", get(explore::explore_repos))
        .route("/api/search", get(search::search))
        .route_layer(from_fn(middleware::require_node_access))
        .route_layer(from_fn(csrf::cookie_api_submission))
        .layer(from_fn(middleware::private_api_response));
    let pages = Router::new()
        .route("/", get(home::home))
        .route("/explore", get(explore::explore_page))
        .route("/search", get(search::search_page))
        .route("/stars", get(stars::stars_page))
        .layer(DefaultBodyLimit::max(16 * 1024));
    api.merge(pages)
}

#[derive(Debug)]
pub(crate) enum DiscoveryReadError {
    Unavailable,
    InvalidOwner,
    MissingOwner,
    MissingRepository,
}

fn database_error(_: anyhow::Error) -> (StatusCode, Json<ApiError>) {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(ApiError::new(
            "database_error",
            "Discovery storage is temporarily unavailable",
        )),
    )
}

fn missing_owner() -> (StatusCode, Json<ApiError>) {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        Json(ApiError::new(
            "unavailable",
            "Repository owner is unavailable",
        )),
    )
}

fn unavailable(ctx: &PageContext, operation: &'static str) -> Response {
    // Neither query values nor underlying DB/filesystem errors belong in logs/HTML.
    tracing::warn!(operation, "Discovery page data unavailable");
    ctx.error(
        StatusCode::SERVICE_UNAVAILABLE,
        "This page's data is temporarily unavailable. Please try again.",
    )
}

/// The 1-based `page` of a list; a list whose address names no page starts on page 1.
fn page_number(query: &HashMap<String, String>) -> Result<usize, &'static str> {
    match query.get("page") {
        None => Ok(1),
        Some(raw) => raw
            .parse::<u32>()
            .ok()
            .filter(|page| *page >= 1)
            .map(|page| page as usize)
            .ok_or("Page must be a positive number."),
    }
}

fn owner_name(
    repo: &db::models::Repository,
    owners: &BTreeMap<i64, String>,
) -> Result<String, DiscoveryReadError> {
    // Numeric user/org IDs are separate namespaces, even in malformed records.
    match (
        repo.owner_kind.as_str(),
        repo.owner_user_id,
        repo.owner_org_id,
    ) {
        ("user", Some(_), None) | ("org", None, Some(_)) => {}
        _ => return Err(DiscoveryReadError::InvalidOwner),
    }
    owners
        .get(&repo.id)
        .cloned()
        .ok_or(DiscoveryReadError::MissingOwner)
}

/// Most topic chips a repository card shows.
const CARD_TOPICS: usize = 5;

fn repository_card(
    repo: db::models::Repository,
    owner: String,
    topics: Vec<String>,
    primary_language: Option<LanguageShare>,
) -> RepositoryCard {
    RepositoryCard {
        id: repo.id,
        owner,
        name: repo.name,
        description: repo.description,
        topics,
        is_private: repo.is_private,
        stars: repo.stars_count,
        forks: repo.forks_count,
        default_branch: repo.default_branch,
        primary_language,
        updated_at: repo.updated_at.format("%Y-%m-%d").to_string(),
    }
}

/// The cards of a list, with the owner, topics and primary language of each repository.
pub(crate) async fn repository_cards(
    state: &AppState,
    repos: Vec<db::models::Repository>,
) -> Result<Vec<RepositoryCard>, DiscoveryReadError> {
    let ids = repos.iter().map(|repo| repo.id).collect::<Vec<_>>();
    let owners = db::repositories::records::repository_owner_names(&state.pool, &ids)
        .await
        .map_err(|_| DiscoveryReadError::Unavailable)?;
    let topics = db::repositories::topics::repository_topics(&state.pool, &ids)
        .await
        .map_err(|_| DiscoveryReadError::Unavailable)?;
    let languages = db::repository_stats::get_primary_repo_languages(&state.pool, &ids)
        .await
        .map_err(|_| DiscoveryReadError::Unavailable)?
        .into_iter()
        .map(|row| {
            (
                row.repo_id,
                LanguageShare {
                    name: row.language,
                    percentage: row.percentage,
                    color: row.color.unwrap_or_default(),
                },
            )
        })
        .collect::<HashMap<_, _>>();
    card_list(repos, &owners, topics, languages)
}

/// The cards of the dashboard lists. The dashboard shows no topics or languages, so they are not
/// read.
pub(crate) async fn dashboard_cards(
    state: &AppState,
    repos: Vec<db::models::Repository>,
) -> Result<Vec<RepositoryCard>, DiscoveryReadError> {
    let ids = repos.iter().map(|repo| repo.id).collect::<Vec<_>>();
    let owners = db::repositories::records::repository_owner_names(&state.pool, &ids)
        .await
        .map_err(|_| DiscoveryReadError::Unavailable)?;
    card_list(repos, &owners, BTreeMap::new(), HashMap::new())
}

fn card_list(
    repos: Vec<db::models::Repository>,
    owners: &BTreeMap<i64, String>,
    mut topics: BTreeMap<i64, Vec<String>>,
    mut languages: HashMap<i64, LanguageShare>,
) -> Result<Vec<RepositoryCard>, DiscoveryReadError> {
    let mut cards = Vec::with_capacity(repos.len());
    for repo in repos {
        let owner = owner_name(&repo, owners)?;
        let shown: Vec<String> = topics
            .remove(&repo.id)
            .unwrap_or_default()
            .into_iter()
            .take(CARD_TOPICS)
            .collect();
        let language = languages.remove(&repo.id);
        cards.push(repository_card(repo, owner, shown, language));
    }
    Ok(cards)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn repository(
        id: i64,
        kind: &str,
        user_id: Option<i64>,
        org_id: Option<i64>,
    ) -> db::models::Repository {
        let now = chrono::Utc::now();
        db::models::Repository {
            id,
            owner_kind: kind.into(),
            owner_user_id: user_id,
            owner_org_id: org_id,
            name: "fixture-repository".into(),
            description: Some("Fixture description".into()),
            website: None,
            is_private: false,
            is_archived: false,
            is_fork: false,
            forked_from_id: None,
            default_branch: "main".into(),
            allow_merge_commit: true,
            allow_squash_merge: true,
            allow_rebase_merge: true,
            disk_path: "/private/repository.git".into(),
            storage_node_id: None,
            size_bytes: 0,
            stars_count: 3,
            forks_count: 2,
            watchers_count: 1,
            open_issues_count: 0,
            created_at: now,
            updated_at: now,
        }
    }

    #[test]
    fn bulk_owner_projection_keeps_namespaces_separate_and_cards_safe() {
        let owners = BTreeMap::from([(10, "alice".into()), (20, "team".into())]);
        let user_repo = repository(10, "user", Some(7), None);
        let org_repo = repository(20, "org", None, Some(7));
        assert_eq!(owner_name(&user_repo, &owners).unwrap(), "alice");
        assert_eq!(owner_name(&org_repo, &owners).unwrap(), "team");
        let card = repository_card(org_repo, "team".into(), Vec::new(), None);
        assert_eq!(card.owner, "team");
        assert_eq!(card.name, "fixture-repository");
        assert_eq!(card.stars, 3);
        assert_eq!(card.forks, 2);
        assert_eq!(card.default_branch, "main");
        let json = serde_json::to_string(&card).unwrap();
        assert!(!json.contains("disk_path"));
        assert!(!json.contains("/private/repository.git"));
    }

    #[test]
    fn owner_projection_refuses_missing_or_malformed_owner_records() {
        let owners = BTreeMap::from([(10, "alice".into())]);
        assert!(matches!(
            owner_name(&repository(20, "org", None, Some(7)), &owners),
            Err(DiscoveryReadError::MissingOwner)
        ));
        for (kind, user_id, org_id) in [
            ("user", Some(7), Some(7)),
            ("org", Some(7), None),
            ("unknown", Some(7), None),
            ("user", None, None),
        ] {
            assert!(matches!(
                owner_name(&repository(10, kind, user_id, org_id), &owners),
                Err(DiscoveryReadError::InvalidOwner)
            ));
        }
    }

    #[test]
    fn discovery_database_errors_do_not_disclose_driver_details() {
        let (status, Json(body)) =
            database_error(anyhow::anyhow!("private SQL, password and storage path"));
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(body.error, "database_error");
        assert_eq!(body.message, "Discovery storage is temporarily unavailable");
    }
}
