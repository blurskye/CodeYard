use crate::discovery::dashboard_cards;
use crate::discovery::unavailable;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::http::Uri;
use axum::response::Response;
use codeyard_api_http::context::PageContext;
use codeyard_app::app::AppState;
use codeyard_db_core::models::AccessLevel;
use codeyard_webui::app::Page as AppPage;
use codeyard_webui::pages::discovery as view;
use codeyard_webui::pages::discovery::HomeModel;
use codeyard_webui::pages::discovery::SharedRepositoryCard;
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
    let page_size =
        codeyard_db_core::cells::pointers::Config::default().browser_repository_page_size as i64;
    let page = ctx
        .query
        .get("page")
        .and_then(|page| page.parse::<usize>().ok())
        .unwrap_or(1)
        // The browser model holds a 32-bit page, so keep the 10,000-page limit pages use.
        .clamp(1, 10_000);
    let offset = i64::try_from(page.saturating_sub(1).saturating_mul(page_size as usize))
        .unwrap_or(i64::MAX);
    let wanted = usize::try_from(offset.saturating_add(page_size)).unwrap_or(usize::MAX);
    let remote = if let Some(session) = ctx.session_id.as_deref() {
        match codeyard_app::api_shared::github::pointers::browser(
            &ctx.state.pool,
            &ctx.database_actor(user.id),
            user.is_stub,
            session,
            codeyard_db_core::cells::pointers::List::BrowserRepositories,
            vec![("q".into(), find.clone())],
            wanted,
        )
        .await
        {
            Ok(remote) => remote,
            Err(_) => return unavailable(&ctx, "home_remote_repositories"),
        }
    } else {
        None
    };
    let (own, repository_count) =
        match codeyard_db_repos::repositories::records::list_user_repository_page(
            &ctx.state.pool,
            user.id,
            ctx.session_id.as_deref(),
            &find,
            true,
            page_size,
            offset,
        )
        .await
        {
            Ok(repos) => repos,
            Err(_) => return unavailable(&ctx, "home_repositories"),
        };
    let (mut shared, mut shared_count) = match shared_cards(
        &ctx,
        user.id,
        &find,
        if remote.is_some() {
            wanted
        } else {
            page_size as usize
        },
        if remote.is_some() { 0 } else { offset },
    )
    .await
    {
        Ok(rows) => rows,
        Err(()) => return unavailable(&ctx, "home_shared_repositories"),
    };
    if let Some(remote) = remote {
        let Ok(mut budget) = codeyard_db_core::cells::pointers::Budget::new(Default::default())
        else {
            return unavailable(&ctx, "home_membership_limits");
        };
        for card in &shared {
            if budget.add(card).is_err() {
                return unavailable(&ctx, "home_membership_limits");
            }
        }
        let Some(total) = remote
            .total
            .and_then(|count| shared_count.checked_add(count))
        else {
            return unavailable(&ctx, "home_remote_count");
        };
        shared_count = total;
        for row in remote.rows {
            if budget.add(&row).is_err() {
                return unavailable(&ctx, "home_membership_limits");
            }
            match serde_json::from_value(row) {
                Ok(row) => shared.push(row),
                Err(_) => return unavailable(&ctx, "home_remote_card"),
            }
        }
        let mut seen = std::collections::HashSet::new();
        shared.retain(|card| seen.insert(card.repository.id));
        shared.sort_by(|a, b| {
            a.repository
                .name
                .cmp(&b.repository.name)
                .then(a.repository.id.cmp(&b.repository.id))
        });
        shared = shared
            .into_iter()
            .skip(offset as usize)
            .take(page_size as usize)
            .collect();
        match codeyard_identity::repository_access::credential_user(
            &ctx.state.pool,
            &ctx.database_actor(user.id),
            false,
        )
        .await
        {
            Ok(current) if current.id == user.id => {}
            _ => {
                return ctx.error(
                    axum::http::StatusCode::UNAUTHORIZED,
                    "Session expired; reload the page.",
                )
            }
        }
    }
    // Trending is for other people's repositories, so the viewer's own are left out. The filter
    // narrows every repository list on the dashboard, Trending included.
    let repositories = match dashboard_cards(&ctx.state, own).await {
        Ok(cards) => cards,
        Err(_) => return unavailable(&ctx, "home_repository_owners"),
    };
    let trending = match indexed_trending(&ctx, &username, &find).await {
        Some(cards) => cards,
        None => {
            let public = match codeyard_db_repos::repositories::records::list_trending_repositories(
                &ctx.state.pool,
                user.id,
                &find,
                5,
            )
            .await
            {
                Ok(repos) => repos,
                Err(_) => return unavailable(&ctx, "home_trending"),
            };
            match dashboard_cards(&ctx.state, public).await {
                Ok(cards) => cards,
                Err(_) => return unavailable(&ctx, "home_trending_owners"),
            }
        }
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
            has_older: offset.saturating_add(page_size) < repository_count.max(shared_count),
        })),
    )
}

/// With cells, Trending ranks every cell's public repositories in the shared index; `None`
/// without an index, or when it fails, so this cell's own list is shown.
async fn indexed_trending(
    ctx: &PageContext,
    username: &str,
    find: &str,
) -> Option<Vec<codeyard_webui::pages::discovery::RepositoryCard>> {
    use codeyard_db_core::cells::search::Filter;
    let mut terms = vec![Filter::Not(Box::new(Filter::Term(
        "owner".into(),
        username.into(),
    )))];
    if !find.is_empty() {
        terms.push(Filter::Term("text".into(), find.into()));
    }
    match codeyard_api_github::search::global::public_repositories(
        &ctx.state,
        Filter::All(terms),
        "stars",
        (0, 5),
    )
    .await
    {
        Ok(found) => Some(
            found?
                .hits
                .iter()
                .filter_map(|hit| crate::discovery::index_card(&hit.document))
                .collect(),
        ),
        Err(_) => {
            tracing::warn!("Trending lists this cell only while the search index is unavailable");
            None
        }
    }
}

// Each database page and owner lookup stays at most 100 rows, even when the global
// page needs candidates from several cells. The original session is checked after waits.
async fn shared_cards(
    ctx: &PageContext,
    user: i64,
    find: &str,
    wanted: usize,
    offset: i64,
) -> Result<(Vec<SharedRepositoryCard>, i64), ()> {
    let config = codeyard_db_core::cells::pointers::Config::default();
    if wanted == 0 || wanted > config.maximum_scan {
        return Err(());
    }
    let mut cards = Vec::new();
    let mut budget = codeyard_db_core::cells::pointers::Budget::new(config).map_err(|_| ())?;
    let mut count = 0;
    while cards.len() < wanted {
        let limit = (wanted - cards.len()).min(config.page_size);
        let (rows, total) = codeyard_db_repos::repositories::records::list_shared_repository_page(
            &ctx.state.pool,
            user,
            find,
            true,
            limit as i64,
            offset.saturating_add(cards.len() as i64),
        )
        .await
        .map_err(|_| ())?;
        count = total;
        let done = rows.len() < limit;
        let (repositories, levels): (Vec<_>, Vec<_>) = rows
            .into_iter()
            .map(|row| (row.repository, row.access_level))
            .unzip();
        let page = dashboard_cards(&ctx.state, repositories)
            .await
            .map_err(|_| ())?;
        for (repository, level) in page.into_iter().zip(levels) {
            let card = SharedRepositoryCard {
                repository,
                role: role_name(level).to_owned(),
            };
            budget.add(&card).map_err(|_| ())?;
            cards.push(card);
        }
        if done {
            break;
        }
    }
    Ok((cards, count))
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
