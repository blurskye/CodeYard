use crate::backend::api::discovery::stars::{star_order_and_text, star_sort, starred_page};
use crate::backend::api::http::context::PageContext;
use crate::backend::api::http::error::ApiError;
use crate::backend::api::user::profile::profile_model;
use crate::backend::api::user::responses::PublicUserResponse as UserResponse;
use crate::backend::api::users::activity::ActivityQuery;
use crate::backend::app::AppState;
use crate::backend::db;
use crate::webui::pages::{account, users};
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode, Uri};
use axum::response::Response;
use axum::Json;
use std::sync::Arc;

pub async fn get_user(
    State(state): State<Arc<AppState>>,
    Path(username): Path<String>,
) -> Result<Json<UserResponse>, (StatusCode, Json<ApiError>)> {
    let user = db::users::accounts::get_user_by_username(&state.pool, &username)
        .await
        .map_err(|_| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ApiError::new(
                    "database_error",
                    "Account storage is temporarily unavailable",
                )),
            )
        })?
        .ok_or_else(|| {
            (
                StatusCode::NOT_FOUND,
                Json(ApiError::new("not_found", "User not found")),
            )
        })?;

    Ok(Json(UserResponse::from(user)))
}

/// The Stars tab: how many of the user's stars the viewer can read, and the open page of them when
/// that tab is the open one. The list is read only for the open tab.
async fn profile_stars(
    ctx: &PageContext,
    target: i64,
    page: i64,
) -> Result<users::ProfileStars, Response> {
    let viewer = ctx.user.as_ref().map(|viewer| viewer.id);
    let unavailable = || {
        ctx.error(
            StatusCode::SERVICE_UNAVAILABLE,
            "Unable to load this user's stars.",
        )
    };
    let count = db::repositories::engagement::count_starred_repositories(
        &ctx.state.pool,
        target,
        viewer,
        "",
    )
    .await
    .map_err(|_| unavailable())?;
    if ctx.query.get("tab").map(String::as_str) != Some("stars") {
        return Ok(users::ProfileStars {
            count,
            ..users::ProfileStars::default()
        });
    }
    let (sort, text) = star_order_and_text(&ctx.query)
        .map_err(|message| ctx.error(StatusCode::BAD_REQUEST, message))?;
    let page = usize::try_from(page.max(1)).unwrap_or(usize::MAX);
    let listed = starred_page(&ctx.state, target, viewer, &text, star_sort(&sort), page)
        .await
        .map_err(|_| unavailable())?;
    Ok(users::ProfileStars {
        count,
        open: true,
        page,
        has_older: listed.has_older,
        repositories: listed.repositories,
        sort,
        query: text,
    })
}

/// `page` is the page of the open tab: activity on the Repositories tab, stars on the Stars tab.
pub(crate) async fn profile_response(
    ctx: &PageContext,
    username: &str,
    status: StatusCode,
    message: Option<&str>,
    page: i64,
) -> Response {
    // The Stars tab shows the repository count and its own list. Only the Repositories tab reads the
    // repositories and the activity feed.
    let stars_tab = ctx.query.get("tab").map(String::as_str) == Some("stars");
    let viewer = ctx.user.as_ref().map(|viewer| viewer.id);
    let repository_page = ctx
        .query
        .get("repo_page")
        .and_then(|page| page.parse::<i64>().ok())
        .unwrap_or(1)
        .clamp(1, 10_000);
    let listing = if stars_tab {
        crate::backend::identity::profiles::repository_count_on_profile(
            &ctx.state.pool,
            username,
            viewer,
        )
        .await
        .map(|(target, count)| (target, count, Vec::new()))
    } else {
        crate::backend::identity::profiles::repositories_on_profile(
            &ctx.state.pool,
            username,
            viewer,
            repository_page,
        )
        .await
    };
    let (target, repository_count, repositories) = match listing {
        Ok(value) => value,
        Err(crate::backend::identity::profiles::ProfileReadError::Missing) => {
            return ctx.error(
                StatusCode::NOT_FOUND,
                "No page, user or organization has this name.",
            )
        }
        Err(crate::backend::identity::profiles::ProfileReadError::UserUnavailable) => {
            return ctx.error(
                StatusCode::SERVICE_UNAVAILABLE,
                "Unable to load this profile.",
            )
        }
        Err(crate::backend::identity::profiles::ProfileReadError::RepositoriesUnavailable) => {
            return ctx.error(
                StatusCode::SERVICE_UNAVAILABLE,
                "Unable to load this user's repositories.",
            )
        }
    };
    let own = ctx
        .user
        .as_ref()
        .is_some_and(|viewer| viewer.id == target.id);
    let repositories = repositories.into_iter().map(profile_repository).collect();
    let following = if let Some(viewer) = ctx.user.as_ref().filter(|viewer| viewer.id != target.id)
    {
        match db::following::is_user_following(&ctx.state.pool, viewer.id, target.id).await {
            Ok(value) => value,
            Err(_) => {
                return ctx.error(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "Unable to load follow status.",
                )
            }
        }
    } else {
        false
    };
    let followers = match db::following::count_user_followers(&ctx.state.pool, target.id).await {
        Ok(value) => value,
        Err(_) => {
            return ctx.error(
                StatusCode::SERVICE_UNAVAILABLE,
                "Unable to load follower count.",
            )
        }
    };
    let follows = match db::following::count_user_following(&ctx.state.pool, target.id).await {
        Ok(value) => value,
        Err(_) => {
            return ctx.error(
                StatusCode::SERVICE_UNAVAILABLE,
                "Unable to load following count.",
            )
        }
    };
    let activity = if stars_tab {
        users::ProfileActivity::default()
    } else {
        match crate::backend::identity::activity::profile_activity(
            &ctx.state.pool,
            ctx.user.as_ref(),
            target.id,
            page,
        )
        .await
        {
            Ok(page) => crate::backend::api::users::activity::profile_activity_view(page),
            Err(_) => {
                return ctx.error(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "Unable to load this user's activity.",
                )
            }
        }
    };
    let stars = match profile_stars(ctx, target.id, page).await {
        Ok(stars) => stars,
        Err(response) => return response,
    };
    let title = format!("{}'s profile", target.username);
    ctx.page_model_status(
        status,
        &title,
        crate::webui::app::Page::Identity(account::Page::Profile {
            profile: profile_model(&target),
            repositories,
            repository_count,
            repository_next: (!stars_tab && (repository_page as usize) * 30 < repository_count)
                .then_some(repository_page as usize + 1),
            csrf_token: ctx.csrf_token.clone(),
            own,
            authenticated: ctx.user.is_some(),
            following,
            followers,
            follows,
            message: message.map(str::to_owned),
            activity,
            stars,
        }),
    )
}

/// One repository row on a public page. Users and organizations list the same fields.
pub(crate) fn profile_repository(repo: db::models::Repository) -> users::Repository {
    users::Repository {
        name: repo.name,
        description: repo.description.unwrap_or_default(),
        private: repo.is_private,
        archived: repo.is_archived,
        stars: i64::from(repo.stars_count),
        forks: i64::from(repo.forks_count),
        updated: repo.updated_at.format("%b %e, %Y").to_string(),
    }
}

pub(crate) async fn profile_get(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    uri: Uri,
    Path(username): Path<String>,
    Query(query): Query<ActivityQuery>,
) -> Response {
    let ctx = match PageContext::load(state, headers, &uri).await {
        Ok(ctx) => ctx,
        Err(response) => return response,
    };
    // Users and organizations share one name namespace, so a name that is no user may be an organization.
    match db::users::accounts::get_user_by_username(&ctx.state.pool, &username).await {
        Ok(None) => {
            crate::backend::api::organizations::profile::public_profile_response(&ctx, &username)
                .await
        }
        _ => {
            profile_response(
                &ctx,
                &username,
                StatusCode::OK,
                None,
                query.page.unwrap_or(1),
            )
            .await
        }
    }
}
