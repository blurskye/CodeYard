//! The Insights page: pulse, contributors, commits and code frequency. Figures come from the
//! database and from the default branch's commit history. Anyone who can read the repository may
//! see them, and nothing here is cached.
use crate::backend::api::http::context::PageContext;
use crate::backend::api::repos::chrome::{default_ref, load, store_error, unavailable, Loaded};
use crate::backend::app::AppState;
use crate::backend::db;
use crate::backend::git::CommitActivityLog;
use crate::backend::repositories::insights as figures;
use crate::backend::repositories::store::repo_store;
use crate::webui::pages::repos as view;
use axum::extract::{OriginalUri, Path, State};
use axum::http::HeaderMap;
use axum::response::Response;
use chrono::{DateTime, Utc};
use std::collections::HashMap;
use std::sync::Arc;

/// The history the figures summarize. `None` means Git refused it as too large.
async fn read_history(
    ctx: &PageContext,
    loaded: &Loaded,
) -> Result<Option<CommitActivityLog>, Response> {
    let store = repo_store(&loaded.repository, ctx.state.cluster.as_ref());
    match store.commit_activity(&default_ref(loaded)).await {
        Ok(log) => Ok(Some(log)),
        Err(error) if error.is_response_too_large() => Ok(None),
        Err(error) => Err(store_error(ctx, Some(&loaded.chrome), error)),
    }
}

async fn insights_view(
    ctx: &PageContext,
    loaded: &Loaded,
    section: &str,
) -> Result<view::insights::InsightsView, Response> {
    use view::insights::InsightsView;
    if loaded.refs.is_empty() {
        return Ok(InsightsView::Empty);
    }
    let history = read_history(ctx, loaded).await?;
    let history = history.as_ref();
    let now = Utc::now().timestamp();
    let query_period = ctx.query.get("period").map(String::as_str);
    Ok(match section {
        "contributors" => InsightsView::Contributors(figures::contributors(
            now,
            figures::contributor_period(query_period),
            history,
        )),
        "commits" => InsightsView::Commits(figures::commit_pattern(now, history)),
        "code-frequency" => InsightsView::CodeFrequency(figures::code_frequency(now, history)),
        _ => {
            let period = figures::pulse_period(query_period);
            let since = DateTime::from_timestamp(figures::pulse_since(now, period), 0)
                .unwrap_or(DateTime::UNIX_EPOCH);
            let pool = &ctx.state.pool;
            let repo_id = loaded.chrome.id;
            let counts = db::repository_insights::count_activity(pool, repo_id, since)
                .await
                .map_err(|_| unavailable(ctx))?;
            let merged = db::repository_insights::list_merged_pulls(
                pool,
                repo_id,
                since,
                figures::LIST_LIMIT,
            )
            .await
            .map_err(|_| unavailable(ctx))?;
            let issues = db::repository_insights::list_opened_issues(
                pool,
                repo_id,
                since,
                figures::LIST_LIMIT,
            )
            .await
            .map_err(|_| unavailable(ctx))?;
            InsightsView::Pulse(figures::pulse(now, period, counts, merged, issues, history))
        }
    })
}

async fn insights_page(ctx: &PageContext, loaded: &Loaded) -> Result<Response, Response> {
    let section = view::insights::SECTIONS
        .iter()
        .map(|(key, _)| *key)
        .find(|key| Some(*key) == ctx.query.get("section").map(String::as_str))
        .unwrap_or("pulse");
    let view = insights_view(ctx, loaded, section).await?;
    let page = view::insights::InsightsPage {
        section: section.to_owned(),
        view,
    };
    Ok(ctx.page_model(
        &format!("Insights · {}/{}", loaded.chrome.owner, loaded.chrome.name),
        crate::webui::app::Page::Repositories(view::page::Page::Insights {
            repo: loaded.chrome.clone(),
            page,
        }),
    ))
}

pub(in crate::backend::api::repos) async fn page(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    OriginalUri(uri): OriginalUri,
    Path(params): Path<HashMap<String, String>>,
) -> Response {
    let ctx = match PageContext::load(state, headers, &uri).await {
        Ok(ctx) => ctx,
        Err(response) => return response,
    };
    let loaded = match load(&ctx, &params["owner"], &params["repo"]).await {
        Ok(loaded) => loaded,
        Err(response) => return response,
    };
    match insights_page(&ctx, &loaded).await {
        Ok(response) | Err(response) => response,
    }
}
