use crate::backend::{
    api::{http::error::ApiError, http::session::get_current_user},
    app::AppState,
    db::files as files_db,
};
use axum::{
    extract::State,
    http::{HeaderMap, StatusCode},
    Json,
};
use serde::Serialize;
use std::sync::Arc;

#[derive(Serialize, Default)]
pub struct UserFileStatsResponse {
    pub total_files: i64,
    pub total_bytes: i64,
    pub by_reason: ByReasonStats,
    pub last_24h: Last24hStats,
}

#[derive(Serialize, Default)]
pub struct ByReasonStats {
    pub avatar: i64,
    pub org_avatar: i64,
    pub release_asset: i64,
    pub issue_attachment: i64,
    pub pr_attachment: i64,
    pub readme_image: i64,
    pub repo_asset: i64,
    pub other: i64,
}

#[derive(Serialize, Default)]
pub struct Last24hStats {
    pub uploads: i64,
    pub bytes: i64,
    pub downloads: i64,
}

pub async fn get_my_file_stats(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Result<Json<UserFileStatsResponse>, (StatusCode, Json<ApiError>)> {
    let user = get_current_user(&state.pool, &headers).await?;
    let stats = files_db::get_user_file_stats(&state.pool, user.id)
        .await
        .map_err(|_| {
            (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(ApiError::new(
                    "database_unavailable",
                    "File statistics are unavailable",
                )),
            )
        })?;

    match stats {
        Some(stats) => Ok(Json(UserFileStatsResponse {
            total_files: stats.total_files,
            total_bytes: stats.total_bytes,
            by_reason: ByReasonStats {
                avatar: stats.avatar_count,
                org_avatar: stats.org_avatar_count,
                release_asset: stats.release_asset_count,
                issue_attachment: stats.issue_attachment_count,
                pr_attachment: stats.pr_attachment_count,
                readme_image: stats.readme_image_count,
                repo_asset: stats.repo_asset_count,
                other: stats.other_count,
            },
            last_24h: Last24hStats {
                uploads: stats.uploads_24h,
                bytes: stats.bytes_24h,
                downloads: stats.downloads_24h,
            },
        })),
        None => Ok(Json(UserFileStatsResponse::default())),
    }
}
