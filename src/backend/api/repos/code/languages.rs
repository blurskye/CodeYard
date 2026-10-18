use crate::backend::api::http::error::ApiError;
use crate::backend::api::repos::access::check_repo_read_access;
use crate::backend::api::repos::chrome::repository_default_branch;
use crate::backend::api::repos::responses::repo_store_error;
use crate::backend::app::AppState;
use crate::backend::db;
use crate::backend::repositories::languages as repo_languages;
use crate::backend::repositories::store::repo_store;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::Json;
use serde::Serialize;
use std::sync::Arc;
#[derive(Serialize)]
pub struct LanguageStatResponse {
    pub language: String,
    pub bytes: i64,
    pub percentage: f32,
    pub color: String,
}

pub async fn get_repo_languages(
    State(state): State<Arc<AppState>>,
    Path((owner, repo)): Path<(String, String)>,
    headers: HeaderMap,
) -> Result<Json<Vec<LanguageStatResponse>>, (StatusCode, Json<ApiError>)> {
    let repository = db::repositories::records::get_repository(&state.pool, &owner, &repo)
        .await
        .map_err(|_| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ApiError::new(
                    "database_error",
                    "Repository operation is unavailable",
                )),
            )
        })?
        .ok_or_else(|| {
            (
                StatusCode::NOT_FOUND,
                Json(ApiError::new("not_found", "Repository not found")),
            )
        })?;

    check_repo_read_access(&state.pool, &repository, &headers).await?;
    let mut languages = repo_languages::stored(&state.pool, repository.id)
        .await
        .map_err(|_| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ApiError::new(
                    "database_error",
                    "Repository operation is unavailable",
                )),
            )
        })?;
    // Nothing is stored until the default branch first moves, so the first reader computes it.
    if languages.is_empty() {
        let default_branch = repository_default_branch(&state, &repository).await?;
        languages = repo_languages::compute_and_store(
            &state.pool,
            &repo_store(&repository, state.cluster.as_ref()),
            repository.id,
            &default_branch,
        )
        .await
        .map_err(|error| repo_store_error(error, StatusCode::INTERNAL_SERVER_ERROR))?;
    }

    Ok(Json(
        languages
            .into_iter()
            .map(|language| LanguageStatResponse {
                language: language.language,
                bytes: language.bytes as i64,
                percentage: language.percentage,
                color: language.color,
            })
            .collect(),
    ))
}
