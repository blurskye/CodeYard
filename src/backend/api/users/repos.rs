use crate::backend::api::http::error::ApiError;
use crate::backend::api::http::session::get_optional_user;
use crate::backend::api::repos::responses::{repository_topics, RepoResponse};
use crate::backend::app::AppState;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::Json;
use std::sync::Arc;

#[derive(serde::Deserialize)]
pub struct RepositoryQuery {
    page: Option<usize>,
    q: Option<String>,
}

pub async fn list_user_repos(
    State(state): State<Arc<AppState>>,
    Path(username): Path<String>,
    headers: HeaderMap,
    Query(query): Query<RepositoryQuery>,
) -> Result<(HeaderMap, Json<Vec<RepoResponse>>), (StatusCode, Json<ApiError>)> {
    let current_user = get_optional_user(&state.pool, &headers).await?;
    let page = query.page.unwrap_or(1).max(1);
    let text = query.q.as_deref().unwrap_or("");
    let (_, repos, count) = crate::backend::identity::profiles::repositories_on_profile(
        &state.pool,
        &username,
        current_user.map(|user| user.id),
        text,
        page,
    )
    .await
    .map_err(|error| match error {
        crate::backend::identity::profiles::ProfileReadError::Missing => (
            StatusCode::NOT_FOUND,
            Json(ApiError::new("not_found", "User not found")),
        ),
        _ => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ApiError::new(
                "database_error",
                "Account storage is temporarily unavailable",
            )),
        ),
    })?;
    let ids = repos.iter().map(|repo| repo.id).collect::<Vec<_>>();
    let mut topics = repository_topics(&state.pool, &ids).await?;
    let mut responses = Vec::new();
    for repository in repos {
        responses.push(RepoResponse::new(
            &repository,
            &username,
            topics.remove(&repository.id).unwrap_or_default(),
            None,
        ));
    }

    let mut headers = HeaderMap::new();
    headers.insert(
        "x-total-count",
        count.to_string().parse().expect("count header"),
    );
    let href = |page| {
        format!(
            "</api/users/{}/repos?page={page}&q={}>; rel=\"next\"",
            crate::webui::encode_component(&username),
            crate::webui::encode_component(text)
        )
    };
    if page.saturating_mul(30) < count {
        headers.insert(
            axum::http::header::LINK,
            href(page.saturating_add(1)).parse().expect("encoded link"),
        );
    }
    Ok((headers, Json(responses)))
}
