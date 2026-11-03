use crate::backend::api::discovery::stars::{star_order_and_text, star_sort, starred_page};
use crate::backend::api::http::context::PageContext;
use crate::backend::api::user::profile::load_profile;
use crate::backend::api::users::activity::ActivityQuery;
use crate::backend::app::AppState;
use crate::backend::db;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode, Uri};
use axum::response::Response;
use codeyard_webui::pages::{account, users};
use std::sync::Arc;

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
    let page = usize::try_from(page.clamp(1, 10_000)).unwrap_or(1);
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
    let viewer = ctx.session_id.as_deref();
    let repositories_page = ctx
        .query
        .get("repo_page")
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(1)
        .clamp(1, 10_000);
    let repository_query = ctx.query.get("q").map(String::as_str).unwrap_or("");
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
            repository_query,
            repositories_page,
        )
        .await
        .map(|(target, repositories, count)| (target, count, repositories))
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
    let repositories = match profile_repositories(&ctx.state.pool, repositories).await {
        Ok(repositories) => repositories,
        Err(_) => {
            return ctx.error(
                StatusCode::SERVICE_UNAVAILABLE,
                "Repository counts are temporarily unavailable.",
            )
        }
    };
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
            ctx.user
                .as_ref()
                .zip(ctx.session_id.as_deref())
                .map(|(user, session)| db::activity::ActivityViewer {
                    user: user.id,
                    session,
                }),
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
    let repository_href = |page: usize| {
        format!(
            "{}?repo_page={page}&q={}",
            codeyard_webui::profile_path(&target.username),
            codeyard_webui::encode_component(repository_query)
        )
    };
    let repositories_newer =
        (!stars_tab && repositories_page > 1).then(|| repository_href(repositories_page - 1));
    let repositories_older = (!stars_tab
        && repositories_page.saturating_mul(30) < repository_count)
        .then(|| repository_href(repositories_page.saturating_add(1)));
    let mut profile = match load_profile(&ctx.state.pool, &target).await {
        Ok(profile) => profile,
        Err(_) => {
            return ctx.error(
                StatusCode::SERVICE_UNAVAILABLE,
                "Unable to load this user's profile.",
            )
        }
    };
    profile.repository_query = repository_query.to_owned();
    let title = format!("{}'s profile", target.username);
    ctx.page_model_status(
        status,
        &title,
        codeyard_webui::app::Page::Identity(account::Page::Profile {
            profile,
            repositories,
            repository_count,
            repositories_newer,
            repositories_older,
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

pub(crate) async fn profile_repositories(
    pool: &db::DbPool,
    repos: Vec<db::models::Repository>,
) -> anyhow::Result<Vec<users::Repository>> {
    let ids = repos.iter().map(|repo| repo.id).collect::<Vec<_>>();
    let mut counts = db::repositories::discovery::public_fork_counts(pool, &ids).await?;
    Ok(repos
        .into_iter()
        .map(|repo| {
            let forks = counts.remove(&repo.id).unwrap_or_default();
            profile_repository(repo, forks)
        })
        .collect())
}

/// One repository row on a public page. Users and organizations list the same fields.
fn profile_repository(repo: db::models::Repository, forks: i32) -> users::Repository {
    users::Repository {
        name: repo.name,
        description: repo.description.unwrap_or_default(),
        private: repo.is_private,
        archived: repo.is_archived,
        stars: i64::from(repo.stars_count),
        forks: i64::from(forks),
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
