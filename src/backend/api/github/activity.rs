//! Stars use the same access checks and notices as the repository page.
mod events;
pub(super) mod feeds;
mod notifications;
mod subscriptions;
use super::{auth, pagination::Pagination, repository, Error, Result};
use crate::backend::webhooks::payloads;
use crate::backend::{
    app::AppState,
    db,
    repositories::{
        access::AccessError,
        engagement::{self, Engagement, EngagementError},
    },
};
use axum::{
    extract::{OriginalUri, Path, Query, State},
    http::{header, HeaderMap, StatusCode, Uri},
    response::Response,
    routing::{delete, get, put, MethodRouter},
    Json,
};
use serde::Deserialize;
use serde_json::{json, Value};
use std::sync::Arc;

fn engagement_error(error: EngagementError) -> Error {
    match error {
        EngagementError::Access(AccessError::NotFound) => Error::missing(),
        EngagementError::Access(
            AccessError::PasswordChangeRequired
            | AccessError::WriteDenied
            | AccessError::InvalidCollaboratorOwner,
        ) => Error::new(StatusCode::FORBIDDEN, "Repository access denied"),
        _ => Error::unavailable(()),
    }
}

async fn starred(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, name)): Path<(String, String)>,
) -> Result<StatusCode> {
    let actor = auth::authenticate(&state, &headers, true)
        .await?
        .ok_or_else(auth::required)?
        .user;
    let (repo, _) = repository(&state, &headers, &owner, &name, false).await?;
    if db::repositories::engagement::is_repo_starred(&state.pool, actor.id, repo.id)
        .await
        .map_err(Error::unavailable)?
    {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(Error::missing())
    }
}

async fn change_star(
    state: Arc<AppState>,
    headers: HeaderMap,
    owner: String,
    name: String,
    enabled: bool,
) -> Result<StatusCode> {
    let (repo, auth) = repository(&state, &headers, &owner, &name, true).await?;
    let actor = auth.ok_or_else(auth::required)?.user;
    engagement::change(
        &state.pool,
        &state.notification_broadcaster,
        actor.id,
        repo.id,
        Engagement::Star,
        enabled,
    )
    .await
    .map_err(engagement_error)?;
    Ok(StatusCode::NO_CONTENT)
}

async fn star(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, name)): Path<(String, String)>,
) -> Result<StatusCode> {
    change_star(state, headers, owner, name, true).await
}

async fn unstar(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, name)): Path<(String, String)>,
) -> Result<StatusCode> {
    change_star(state, headers, owner, name, false).await
}

async fn star_count(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, name)): Path<(String, String)>,
) -> Result<Json<Value>> {
    let (repo, _) = repository(&state, &headers, &owner, &name, false).await?;
    Ok(Json(json!({"count": repo.stars_count})))
}

fn timestamps(headers: &HeaderMap) -> bool {
    headers
        .get(header::ACCEPT)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.contains(".star+json"))
}

#[derive(Deserialize)]
struct StarList {
    page: Option<usize>,
    per_page: Option<usize>,
    sort: Option<String>,
    direction: Option<String>,
}

async fn list_stars(
    state: &AppState,
    headers: &HeaderMap,
    username: Option<&str>,
    query: StarList,
    uri: &Uri,
) -> Result<Response> {
    let auth = auth::authenticate(state, headers, username.is_none()).await?;
    let user = match username {
        Some(username) => db::users::accounts::get_user_by_username(&state.pool, username)
            .await
            .map_err(Error::unavailable)?
            .ok_or_else(Error::missing)?,
        None => auth.as_ref().ok_or_else(auth::required)?.user.clone(),
    };
    let sort = match (
        query.sort.as_deref().unwrap_or("created"),
        query.direction.as_deref().unwrap_or("desc"),
    ) {
        ("created", "desc") => db::repositories::engagement::StarSort::Recent,
        ("created", "asc") => db::repositories::engagement::StarSort::Oldest,
        ("updated", "desc") => db::repositories::engagement::StarSort::Updated,
        ("updated", "asc") => db::repositories::engagement::StarSort::UpdatedOldest,
        _ => return Err(Error::invalid("Invalid sort or direction")),
    };
    let page = Pagination {
        page: query.page,
        per_page: query.per_page,
    };
    let (offset, limit) = page.window()?;
    let rows = db::repositories::engagement::list_starred_repositories(
        &state.pool,
        user.id,
        auth.as_ref().map(|auth| auth.user.id),
        "",
        sort,
        (limit + 1) as i64,
        offset as i64,
    )
    .await
    .map_err(Error::unavailable)?;
    let has_next = rows.len() > limit;
    let readable = auth::readable_repositories(state, auth.as_ref(), &rows.iter().map(|repo| repo.id).collect::<Vec<_>>(), "metadata").await?;
    let mut items = Vec::new();
    for repo in rows.iter().take(limit).filter(|repo| readable.contains(&repo.id)) {
        let value = super::repos::document(state, repo, auth.as_ref()).await?;
        items.push(if timestamps(headers) {
            let Some(at) = db::repositories::engagement::starred_at(&state.pool, user.id, repo.id)
                .await
                .map_err(Error::unavailable)? else { continue; };
            json!({"starred_at": at, "repo": value})
        } else {
            value
        });
    }
    page.response(uri, items, has_next)
}

async fn my_stars(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Query(query): Query<StarList>,
    OriginalUri(uri): OriginalUri,
) -> Result<Response> {
    list_stars(&state, &headers, None, query, &uri).await
}

async fn user_stars(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(username): Path<String>,
    Query(query): Query<StarList>,
    OriginalUri(uri): OriginalUri,
) -> Result<Response> {
    list_stars(&state, &headers, Some(&username), query, &uri).await
}

async fn stargazers(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, name)): Path<(String, String)>,
    Query(page): Query<Pagination>,
    OriginalUri(uri): OriginalUri,
) -> Result<Response> {
    let (repo, _) = repository(&state, &headers, &owner, &name, false).await?;
    let (offset, limit) = page.window()?;
    let rows = db::repositories::engagement::list_repo_stargazers(
        &state.pool,
        repo.id,
        limit as i64,
        offset as i64,
    )
    .await
    .map_err(Error::unavailable)?;
    let has_next = rows.len() == limit
        && !db::repositories::engagement::list_repo_stargazers(
            &state.pool,
            repo.id,
            1,
            (offset + limit) as i64,
        )
        .await
        .map_err(Error::unavailable)?
        .is_empty();
    let mut items = Vec::new();
    for user in rows {
        let value = payloads::user(&user);
        items.push(if timestamps(&headers) {
            let Some(at) = db::repositories::engagement::starred_at(&state.pool, user.id, repo.id)
                .await
                .map_err(Error::unavailable)? else { continue; };
            json!({"starred_at": at, "user": value})
        } else {
            value
        });
    }
    page.response(&uri, items, has_next)
}

async fn star_history(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, name)): Path<(String, String)>,
    Query(query): Query<Pagination>,
    OriginalUri(uri): OriginalUri,
) -> Result<Response> {
    let (repo, _) = repository(&state, &headers, &owner, &name, false).await?;
    if query.page.is_some_and(|p| p == 0 || p > 100) || query.per_page == Some(0) {
        return Err(Error::invalid(
            "page must be 1 to 100 and per_page must be positive",
        ));
    }
    let page = Pagination {
        page: query.page,
        per_page: Some(super::pagination::page_size(query.per_page).min(30)),
    };
    let (offset, limit) = page.window()?;
    let rows = db::repositories::engagement::list_star_history(
        &state.pool,
        repo.id,
        (limit + 1) as i64,
        offset as i64,
    )
    .await
    .map_err(Error::unavailable)?;
    let has_next = rows.len() > limit;
    let items=rows.iter().take(limit).map(|week|{
        let days=[week.day0,week.day1,week.day2,week.day3,week.day4,week.day5,week.day6];
        json!({"week":week.week.and_time(chrono::NaiveTime::MIN).and_utc().timestamp(),"days":days,"total":days.iter().sum::<i64>()})
    }).collect();
    page.response(&uri, items, has_next)
}

pub(super) fn handlers() -> Vec<(&'static str, MethodRouter<Arc<AppState>>)> {
    let mut handlers = vec![
        (
            "activity/check-repo-is-starred-by-authenticated-user",
            get(starred),
        ),
        ("activity/star-repo-for-authenticated-user", put(star)),
        (
            "activity/unstar-repo-for-authenticated-user",
            delete(unstar),
        ),
        ("activity/get-stargazer-count-for-repo", get(star_count)),
        ("activity/get-stargazer-history-for-repo", get(star_history)),
        (
            "activity/list-repos-starred-by-authenticated-user",
            get(my_stars),
        ),
        ("activity/list-repos-starred-by-user", get(user_stars)),
        ("activity/list-stargazers-for-repo", get(stargazers)),
    ];
    handlers.extend(subscriptions::handlers());
    handlers.extend(notifications::handlers());
    handlers.extend(events::handlers());
    handlers.extend(feeds::handlers());
    handlers
}
