//! The wiki JSON API, read-only: a repository's pages and the current content of one page. A
//! repository whose wiki is off, or that the caller cannot read, answers 404.
use crate::backend::api::http::error::ApiError;
use crate::backend::api::http::session::get_optional_user;
use crate::backend::app::AppState;
use crate::backend::db;
use crate::backend::db::models::WikiPage;
use crate::backend::repositories::access::{self, AccessError};
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::Json;
use serde::Serialize;
use std::sync::Arc;

type Failure = (StatusCode, Json<ApiError>);

#[derive(Serialize)]
pub struct PageSummary {
    slug: String,
    title: String,
    updated_at: String,
}

#[derive(Serialize)]
pub struct PageResponse {
    slug: String,
    title: String,
    body: String,
    revision: i64,
    message: String,
    author: String,
    updated_at: String,
}

fn not_found() -> Failure {
    (
        StatusCode::NOT_FOUND,
        Json(ApiError::new("not_found", "Wiki page not found.")),
    )
}

fn unavailable() -> Failure {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        Json(ApiError::new(
            "unavailable",
            "Wiki storage is unavailable. Try again.",
        )),
    )
}

/// The repository's id when the caller may read its wiki.
async fn readable_wiki(
    state: &AppState,
    headers: &HeaderMap,
    owner: &str,
    name: &str,
) -> Result<i64, Failure> {
    let viewer = get_optional_user(&state.pool, headers)
        .await?
        .map(|user| user.id);
    let repository = db::repositories::records::get_repository(&state.pool, owner, name)
        .await
        .map_err(|_| unavailable())?
        .ok_or_else(not_found)?;
    let repository = access::check_read(&state.pool, repository.id, viewer)
        .await
        .map_err(|error| match error {
            AccessError::NotFound => not_found(),
            _ => unavailable(),
        })?;
    if !repository.wiki_enabled {
        return Err(not_found());
    }
    Ok(repository.id)
}

pub(super) async fn list(
    State(state): State<Arc<AppState>>,
    Path((owner, name)): Path<(String, String)>,
    headers: HeaderMap,
) -> Result<Json<Vec<PageSummary>>, Failure> {
    let repo_id = readable_wiki(&state, &headers, &owner, &name).await?;
    let pages = db::wiki::list_pages(&state.pool, repo_id, None)
        .await
        .map_err(|_| unavailable())?;
    Ok(Json(
        pages
            .into_iter()
            .map(|page| PageSummary {
                slug: page.slug,
                title: page.title,
                updated_at: page.updated_at.to_rfc3339(),
            })
            .collect(),
    ))
}

pub(super) async fn get_one(
    State(state): State<Arc<AppState>>,
    Path((owner, name, slug)): Path<(String, String, String)>,
    headers: HeaderMap,
) -> Result<Json<PageResponse>, Failure> {
    let repo_id = readable_wiki(&state, &headers, &owner, &name).await?;
    let page = db::wiki::get_page_by_slug(&state.pool, repo_id, &slug)
        .await
        .map_err(|_| unavailable())?
        .ok_or_else(not_found)?;
    Ok(Json(page_response(page)))
}

fn page_response(page: WikiPage) -> PageResponse {
    PageResponse {
        slug: page.slug,
        title: page.head.title,
        body: page.head.body,
        revision: page.head.number,
        message: page.head.message,
        author: page.head.author,
        updated_at: page.head.saved_at.to_rfc3339(),
    }
}
