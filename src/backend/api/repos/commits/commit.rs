use crate::backend::api::context::PageContext;
use crate::backend::api::error::ApiError;
use crate::backend::api::repos::access::check_repo_read_access;
use crate::backend::api::repos::chrome::{commit, hex_revision, store_error, Loaded};
use crate::backend::api::repos::responses::{AuthorInfo, CommitResponse};
use crate::backend::app::AppState;
use crate::backend::db;
use crate::backend::repositories::store::{repo_store, DiffInfo};
use crate::webui::pages::repositories as view;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::Response;
use axum::Json;
use serde::Serialize;
use std::sync::Arc;
#[derive(Serialize)]
pub struct CommitDetailResponse {
    pub commit: CommitResponse,
    pub diff: DiffInfo,
}

pub async fn get_commit(
    State(state): State<Arc<AppState>>,
    Path((owner, repo, sha)): Path<(String, String, String)>,
    headers: HeaderMap,
) -> Result<Json<CommitDetailResponse>, (StatusCode, Json<ApiError>)> {
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

    let store = repo_store(&repository, state.cluster.as_ref());
    let commit_info = store.commit(&sha).await.map_err(|e| {
        crate::backend::api::repos::responses::repo_store_error(
            e,
            StatusCode::INTERNAL_SERVER_ERROR,
        )
    })?;
    let diff = store.commit_diff(&commit_info.sha).await.map_err(|e| {
        crate::backend::api::repos::responses::repo_store_error(
            e,
            StatusCode::INTERNAL_SERVER_ERROR,
        )
    })?;

    let author = db::users::accounts::get_user_by_email(&state.pool, &commit_info.author_email)
        .await
        .ok()
        .flatten()
        .map(|user| AuthorInfo {
            id: Some(user.id),
            username: Some(user.username),
            avatar_url: user
                .avatar_file_id
                .map(|id| format!("/api/files/{}/download", id)),
        });

    Ok(Json(CommitDetailResponse {
        commit: CommitResponse::from_commit(commit_info, author),
        diff,
    }))
}

async fn commit_page(ctx: &PageContext, loaded: &Loaded, sha: &str) -> Result<Response, Response> {
    if !hex_revision(sha) {
        return Err(ctx.error(StatusCode::BAD_REQUEST, "Invalid commit SHA."));
    }
    let store = repo_store(&loaded.repository, ctx.state.cluster.as_ref());
    let info = store
        .commit(sha)
        .await
        .map_err(|error| store_error(ctx, Some(&loaded.chrome), error))?;
    let diff = store
        .commit_diff(&info.sha)
        .await
        .map_err(|error| store_error(ctx, Some(&loaded.chrome), error))?;
    let info = commit(info);
    let files: Vec<_> = diff
        .files
        .into_iter()
        .map(|file| view::DiffFile {
            path: file.filename,
            status: file.status,
            additions: file.additions,
            deletions: file.deletions,
            patch: file.patch,
        })
        .collect();
    Ok(ctx.page_model(
        "Commit",
        crate::webui::app::Page::Repositories(view::Page::CommitDetail {
            repo: loaded.chrome.clone(),
            commit: info,
            files,
        }),
    ))
}

pub(super) async fn page(
    axum::extract::State(state): axum::extract::State<
        std::sync::Arc<crate::backend::app::AppState>,
    >,
    headers: axum::http::HeaderMap,
    axum::extract::OriginalUri(uri): axum::extract::OriginalUri,
    axum::extract::Path(params): axum::extract::Path<std::collections::HashMap<String, String>>,
) -> axum::response::Response {
    let ctx = match crate::backend::api::context::PageContext::load(state, headers, &uri).await {
        Ok(ctx) => ctx,
        Err(response) => return response,
    };
    let loaded =
        match crate::backend::api::repos::chrome::load(&ctx, &params["owner"], &params["repo"])
            .await
        {
            Ok(loaded) => loaded,
            Err(response) => return response,
        };
    match commit_page(&ctx, &loaded, &params["sha"]).await {
        Ok(response) | Err(response) => response,
    }
}
