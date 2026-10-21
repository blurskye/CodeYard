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
    const PAGE_SIZE: i64 = 30;
    let page = ctx
        .query
        .get("page")
        .and_then(|page| page.parse::<usize>().ok())
        .unwrap_or(1)
        .max(1);
    let offset = i64::try_from(page.saturating_sub(1).saturating_mul(PAGE_SIZE as usize))
        .unwrap_or(i64::MAX);
    let (own, repository_count) = match db::repositories::records::list_user_repository_page(
        &ctx.state.pool,
        user.id,
        true,
        &find,
        PAGE_SIZE,
        offset,
    )
    .await
    {
        Ok(repos) => repos,
        Err(_) => return unavailable(&ctx, "home_repositories"),
    };
    let (shared, shared_count) = match db::repositories::records::list_shared_repository_page(
        &ctx.state.pool,
        user.id,
        &find,
        PAGE_SIZE,
        offset,
    )
    .await
    {
        Ok(rows) => rows,
        Err(_) => return unavailable(&ctx, "home_shared_repositories"),
    };
    let public = match db::repositories::records::list_public_repositories(
        &ctx.state.pool,
        RepositorySort::Stars,
        100,
    )
    .await
    {
        Ok(repos) => repos,
        Err(_) => return unavailable(&ctx, "home_trending"),
    };
    // Trending is for other people's repositories, so the viewer's own are left out.
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
        .filter(|repo| !repo.is_personal_owner(user.id) && matches(repo))
        .take(5)
        .collect();
    let repositories = match dashboard_cards(&ctx.state, own).await {
        Ok(cards) => cards,
        Err(_) => return unavailable(&ctx, "home_repository_owners"),
    };
    let (shared_repos, levels): (Vec<_>, Vec<_>) = shared
        .into_iter()
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
    let trending = match dashboard_cards(&ctx.state, public).await {
        Ok(cards) => cards,
        Err(_) => return unavailable(&ctx, "home_trending_owners"),
    };
    ctx.page_model(
        "Dashboard",
        AppPage::Discovery(view::Page::Home(HomeModel {
            username: Some(username),
            repositories,
            shared,
            trending,
            find,
            page,
            repository_count,
            shared_count,
            has_older: offset.saturating_add(PAGE_SIZE) < repository_count.max(shared_count),
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
