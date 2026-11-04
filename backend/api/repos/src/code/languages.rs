use crate::access::readable_repository;
use crate::chrome::repository_default_branch;
use crate::responses::repo_store_error;
use axum::extract::Path;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::http::StatusCode;
use axum::Json;
use codeyard_api_http::error::ApiError;
use codeyard_app::app::AppState;
use codeyard_app::repositories::store::repo_store;
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
    let repository = readable_repository(&state.pool, &owner, &repo, &headers).await?;
    let default_branch = repository_default_branch(&state, &repository).await?;
    let languages = codeyard_app::repositories::languages::compute_and_store(
        &state.pool,
        &repo_store(&repository, state.cluster.as_ref()),
        &repository,
        &default_branch,
    )
    .await
    .map_err(|error| repo_store_error(error, StatusCode::INTERNAL_SERVER_ERROR))?;
    let current = readable_repository(&state.pool, &owner, &repo, &headers).await?;
    if current.id != repository.id {
        return Err((
            StatusCode::NOT_FOUND,
            Json(ApiError::new("not_found", "Repository not found")),
        ));
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
