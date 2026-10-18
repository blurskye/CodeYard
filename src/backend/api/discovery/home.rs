use super::{dashboard_cards, unavailable};
use crate::backend::db::models::AccessLevel;
use crate::backend::db::repositories::records::RepositorySort;
use crate::backend::{api::http::context::PageContext, app::AppState, db};
use crate::webui::app::Page as AppPage;
use crate::webui::pages::discovery::{self as view, HomeModel, SharedRepositoryCard};
use axum::{
    extract::State,
    http::{HeaderMap, Uri},
    response::Response,
};
use std::sync::Arc;

pub(super) async fn home(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    uri: Uri,
) -> Response {
    let ctx = match PageContext::load(state, headers, &uri).await {
        Ok(ctx) => ctx,
        Err(response) => return response,
    };
    let Some(user) = &ctx.user else {
        return ctx.page_model(
            "Codeyard",
            AppPage::Discovery(view::Page::Home(HomeModel::default())),
        );
    };
    let username = user.username.clone();
    let find = ctx
        .query
        .get("q")
        .map(|text| text.trim().chars().take(100).collect::<String>())
        .unwrap_or_default();
    // This scope comes from the verified session, never from a query/form owner.
    let own =
        match db::repositories::records::list_user_repositories(&ctx.state.pool, user.id).await {
            Ok(repos) => repos,
            Err(_) => return unavailable(&ctx, "home_repositories"),
        };
    let shared =
        match db::repositories::records::list_shared_repositories(&ctx.state.pool, user.id).await {
            Ok(rows) => rows,
            Err(_) => return unavailable(&ctx, "home_shared_repositories"),
        };
    let public = match db::repositories::records::list_public_repositories(
        &ctx.state.pool,
        RepositorySort::Recent,
        100,
    )
    .await
    {
        Ok(repos) => repos,
        Err(_) => return unavailable(&ctx, "home_trending"),
    };
    // Trending is for other people's repositories, so the viewer's own are left out.
    let own_ids: std::collections::HashSet<i64> = own.iter().map(|repo| repo.id).collect();
    // The filter narrows every repository list on the dashboard, Trending included.
    let needle = find.to_lowercase();
    let matches = |repo: &db::models::Repository| {
        repo.name.to_lowercase().contains(&needle)
            || repo
                .description
                .as_deref()
                .is_some_and(|text| text.to_lowercase().contains(&needle))
    };
    let public: Vec<_> = public
        .into_iter()
        .filter(|repo| !own_ids.contains(&repo.id) && matches(repo))
        .collect();
    let own: Vec<_> = own.into_iter().filter(|repo| matches(repo)).collect();
    let repositories = match dashboard_cards(&ctx.state, own).await {
        Ok(cards) => cards,
        Err(_) => return unavailable(&ctx, "home_repository_owners"),
    };
    let (shared_repos, levels): (Vec<_>, Vec<_>) = shared
        .into_iter()
        .filter(|row| matches(&row.repository))
        .map(|row| (row.repository, row.access_level))
        .unzip();
    let shared = match dashboard_cards(&ctx.state, shared_repos).await {
        Ok(cards) => cards,
        Err(_) => return unavailable(&ctx, "home_shared_owners"),
    };
    let shared: Vec<SharedRepositoryCard> = shared
        .into_iter()
        .zip(levels)
        .map(|(repository, level)| SharedRepositoryCard {
            repository,
            role: role_name(level).to_owned(),
        })
        .collect();
    let mut trending = match dashboard_cards(&ctx.state, public).await {
        Ok(cards) => cards,
        Err(_) => return unavailable(&ctx, "home_trending_owners"),
    };
    trending.sort_by_key(|repo| std::cmp::Reverse(repo.stars));
    trending.truncate(5);
    let assigned_issues = match db::issues::count_viewer_issues(
        &ctx.state.pool,
        user.id,
        db::issues::IssueRelation::Assigned,
    )
    .await
    {
        Ok(counts) => counts.open,
        Err(_) => return unavailable(&ctx, "home_assigned_issues"),
    };
    let open_pull_requests = match db::pull_requests::records::count_viewer_pull_requests(
        &ctx.state.pool,
        user.id,
        db::pull_requests::records::PullRequestRelation::Involved,
    )
    .await
    {
        Ok(counts) => counts.open,
        Err(_) => return unavailable(&ctx, "home_pull_request_counts"),
    };
    ctx.page_model(
        "Dashboard",
        AppPage::Discovery(view::Page::Home(HomeModel {
            username: Some(username),
            avatar_url: user
                .avatar_file_id
                .map(|id| format!("/api/files/{id}/download")),
            assigned_issues,
            open_pull_requests,
            repositories,
            shared,
            trending,
            find,
        })),
    )
}

/// The name of a collaborator level, as the repository settings name it.
fn role_name(level: i16) -> &'static str {
    match AccessLevel::from(level) {
        AccessLevel::None => "None",
        AccessLevel::Read => "Read",
        AccessLevel::Triage => "Triage",
        AccessLevel::Write => "Write",
        AccessLevel::Maintain => "Maintain",
        AccessLevel::Admin => "Admin",
        AccessLevel::Owner => "Owner",
    }
}

pub(super) async fn demo(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    uri: Uri,
) -> Response {
    let ctx = match PageContext::load(state, headers, &uri).await {
        Ok(ctx) => ctx,
        Err(response) => return response,
    };
    ctx.page_model(
        "Blurshell Demo",
        AppPage::Discovery(view::Page::Demo { no_nav: false }),
    )
}

pub(super) async fn demo_no_nav(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    uri: Uri,
) -> Response {
    let ctx = match PageContext::load(state, headers, &uri).await {
        Ok(ctx) => ctx,
        Err(response) => return response,
    };
    ctx.page_model(
        "No Navbar Page",
        AppPage::Discovery(view::Page::Demo { no_nav: true }),
    )
}
