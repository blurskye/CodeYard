//! Public profile activity: pushes, issues, pull requests and comments, newest first.
use crate::backend::api::{http::error::ApiError, http::session::get_optional_user};
use crate::backend::app::AppState;
use crate::backend::db::{self, activity::ActivityRow};
use crate::backend::identity::activity::{self, ActivityPage};
use crate::webui::pages::users::{ProfileActivity, ProfileActivityItem};
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::Json;
use serde::Deserialize;
use std::sync::Arc;

#[derive(Deserialize)]
pub(crate) struct ActivityQuery {
    pub page: Option<i64>,
}

pub(crate) async fn list_user_activity(
    State(state): State<Arc<AppState>>,
    Path(username): Path<String>,
    Query(query): Query<ActivityQuery>,
    headers: HeaderMap,
) -> Result<Json<ProfileActivity>, (StatusCode, Json<ApiError>)> {
    let viewer = get_optional_user(&state.pool, &headers).await?;
    let user = db::users::accounts::get_user_by_username(&state.pool, &username)
        .await
        .map_err(|_| activity_unavailable())?
        .ok_or_else(|| {
            (
                StatusCode::NOT_FOUND,
                Json(ApiError::new("not_found", "User not found")),
            )
        })?;
    let page = activity::profile_activity(
        &state.pool,
        viewer.as_ref(),
        user.id,
        query.page.unwrap_or(1),
    )
    .await
    .map_err(|_| activity_unavailable())?;
    Ok(Json(profile_activity_view(page)))
}

fn activity_unavailable() -> (StatusCode, Json<ApiError>) {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(ApiError::new(
            "database_error",
            "Activity storage is temporarily unavailable",
        )),
    )
}

/// The presentation of one page, shared by the profile page and the JSON endpoint.
pub(crate) fn profile_activity_view(page: ActivityPage) -> ProfileActivity {
    ProfileActivity {
        items: page.items.into_iter().map(activity_item).collect(),
        next_page: page.next_page,
    }
}

fn activity_item(row: ActivityRow) -> ProfileActivityItem {
    let repository = format!("{}/{}", row.owner_name, row.repo_name);
    let repo_path = format!("/{repository}");
    let number = row.number.unwrap_or_default();
    let url = match row.kind.as_str() {
        "push" => format!(
            "{repo_path}/commits?ref={}",
            crate::webui::encode_component(row.branch.as_deref().unwrap_or_default())
        ),
        "issue_opened" | "issue_comment" => format!("{repo_path}/issues/{number}"),
        "pull_request_opened" | "pull_request_merged" | "pull_request_comment" => {
            format!("{repo_path}/pulls/{number}")
        }
        _ => repo_path,
    };
    ProfileActivityItem {
        kind: row.kind,
        created_at: row.created_at.to_rfc3339(),
        repository,
        url,
        number: row.number,
        title: row.title,
        branch: row.branch,
        commits: row.commit_count,
    }
}
