//! Repository insights pages. Each reads what is already recorded: the database's daily counts
//! and workflow runs, the contributors and code frequency Git keeps cached, and the forks.
use crate::backend::api::http::context::PageContext;
use crate::backend::api::repos::activity::forks::visible_forks;
use crate::backend::api::repos::chrome::{self, Loaded};
use crate::backend::api::repos::code::contributors::contributor_accounts;
use crate::backend::api::repos::responses::repository_owners;
use crate::backend::app::AppState;
use crate::backend::db;
use crate::backend::repositories::store::repo_store;
use crate::webui::app::Page;
use crate::webui::components::charts::{normalize_days, DailyActivity};
use crate::webui::pages::repos::insights::{
    Contributor, Fork, Page as InsightsPage, Week, WorkflowUsage, FORKS_PER_PAGE,
};
use crate::webui::pages::repos::page::Page as RepoPage;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, Uri};
use axum::response::Response;
use axum::{routing::get, Router};
use std::sync::Arc;

pub fn page_router() -> Router<Arc<AppState>> {
    Router::new()
        .route("/:owner/:repo/insights", get(pulse))
        .route("/:owner/:repo/insights/contributors", get(contributors))
        .route("/:owner/:repo/insights/code-frequency", get(code_frequency))
        .route("/:owner/:repo/insights/actions", get(actions))
        .route("/:owner/:repo/insights/forks", get(forks))
}

fn show(ctx: &PageContext, loaded: Loaded, title: &str, page: InsightsPage) -> Response {
    ctx.page_model(
        title,
        Page::Repositories(RepoPage::Insights {
            repo: loaded.chrome,
            page,
        }),
    )
}

/// The request's context and the repository, read from the database only.
async fn open(
    state: Arc<AppState>,
    headers: HeaderMap,
    uri: &Uri,
    owner: &str,
    name: &str,
) -> Result<(PageContext, Loaded), Response> {
    let ctx = PageContext::load(state, headers, uri).await?;
    let loaded = chrome::load_database(&ctx, owner, name).await?;
    Ok((ctx, loaded))
}

async fn pulse(
    State(state): State<Arc<AppState>>,
    Path((owner, name)): Path<(String, String)>,
    headers: HeaderMap,
    uri: Uri,
) -> Response {
    let (ctx, loaded) = match open(state, headers, &uri, &owner, &name).await {
        Ok(opened) => opened,
        Err(response) => return response,
    };
    let days = normalize_days(ctx.query.get("days").map(String::as_str));
    let points = match db::repository_stats::list_repo_activity_points(
        &ctx.state.pool,
        loaded.repository.id,
        days,
    )
    .await
    {
        Ok(points) => points,
        Err(_) => return chrome::unavailable(&ctx),
    };
    let activity = points
        .into_iter()
        .map(|point| DailyActivity {
            date: point.day.to_string(),
            commits: point.commits,
            pulls_opened: point.pulls_opened,
            pulls_merged: point.pulls_merged,
            issues_opened: point.issues_opened,
            issues_closed: point.issues_closed,
            runs: point.runs,
        })
        .collect();
    show(
        &ctx,
        loaded,
        "Pulse",
        InsightsPage::Pulse { days, activity },
    )
}

async fn contributors(
    State(state): State<Arc<AppState>>,
    Path((owner, name)): Path<(String, String)>,
    headers: HeaderMap,
    uri: Uri,
) -> Response {
    let (ctx, loaded) = match open(state, headers, &uri, &owner, &name).await {
        Ok(opened) => opened,
        Err(response) => return response,
    };
    let found = match repo_store(&loaded.repository, ctx.state.cluster.as_ref())
        .contributors()
        .await
    {
        Ok(found) => found,
        Err(error) => return chrome::store_error(&ctx, Some(&loaded.chrome), error),
    };
    let Ok(accounts) = contributor_accounts(&ctx.state.pool, &found).await else {
        return chrome::unavailable(&ctx);
    };
    let mut rows: Vec<Contributor> = found
        .into_iter()
        .map(|contributor| Contributor {
            username: accounts
                .get(&contributor.email)
                .map(|user| user.username.clone()),
            name: contributor.name,
            commits: i64::from(contributor.commits),
        })
        .collect();
    rows.sort_by(|a, b| b.commits.cmp(&a.commits).then_with(|| a.name.cmp(&b.name)));
    show(
        &ctx,
        loaded,
        "Contributors",
        InsightsPage::Contributors { rows },
    )
}

async fn code_frequency(
    State(state): State<Arc<AppState>>,
    Path((owner, name)): Path<(String, String)>,
    headers: HeaderMap,
    uri: Uri,
) -> Response {
    let ctx = match PageContext::load(state, headers, &uri).await {
        Ok(ctx) => ctx,
        Err(response) => return response,
    };
    // The chart follows the default branch, so this page needs Git for its name.
    let loaded = match chrome::load(&ctx, &owner, &name).await {
        Ok(loaded) => loaded,
        Err(response) => return response,
    };
    let weeks = match repo_store(&loaded.repository, ctx.state.cluster.as_ref())
        .code_frequency(&loaded.default_branch)
        .await
    {
        Ok(weeks) => weeks,
        Err(error) => return chrome::store_error(&ctx, Some(&loaded.chrome), error),
    };
    let weeks = weeks
        .into_iter()
        .map(|week| Week {
            week: week.week,
            additions: i64::try_from(week.additions).unwrap_or(i64::MAX),
            deletions: i64::try_from(week.deletions).unwrap_or(i64::MAX),
        })
        .collect();
    show(
        &ctx,
        loaded,
        "Code frequency",
        InsightsPage::CodeFrequency { weeks },
    )
}

async fn actions(
    State(state): State<Arc<AppState>>,
    Path((owner, name)): Path<(String, String)>,
    headers: HeaderMap,
    uri: Uri,
) -> Response {
    let (ctx, loaded) = match open(state, headers, &uri, &owner, &name).await {
        Ok(opened) => opened,
        Err(response) => return response,
    };
    let days = normalize_days(ctx.query.get("days").map(String::as_str));
    let usage = match db::repository_stats::list_workflow_usage(
        &ctx.state.pool,
        loaded.repository.id,
        days,
    )
    .await
    {
        Ok(usage) => usage,
        Err(_) => return chrome::unavailable(&ctx),
    };
    let workflows = usage
        .into_iter()
        .map(|usage| WorkflowUsage {
            path: usage.workflow_path,
            runs: usage.runs,
            succeeded: usage.succeeded,
            failed: usage.failed,
            average_seconds: usage.average_seconds.map(|seconds| seconds.round() as i64),
        })
        .collect();
    show(
        &ctx,
        loaded,
        "Actions usage",
        InsightsPage::Actions { days, workflows },
    )
}

async fn forks(
    State(state): State<Arc<AppState>>,
    Path((owner, name)): Path<(String, String)>,
    headers: HeaderMap,
    uri: Uri,
) -> Response {
    let (ctx, loaded) = match open(state, headers, &uri, &owner, &name).await {
        Ok(opened) => opened,
        Err(response) => return response,
    };
    let page = ctx
        .query
        .get("page")
        .and_then(|value| value.parse::<i64>().ok())
        .unwrap_or(1)
        .clamp(1, 10_000);
    let viewer = ctx.user.as_ref().map(|user| user.id);
    let offset = (page - 1) * FORKS_PER_PAGE;
    let Ok((found, has_more)) = visible_forks(
        &ctx.state.pool,
        viewer,
        loaded.repository.id,
        FORKS_PER_PAGE,
        offset,
    )
    .await
    else {
        return chrome::unavailable(&ctx);
    };
    let ids: Vec<i64> = found.iter().map(|fork| fork.id).collect();
    let Ok(mut owners) = repository_owners(&ctx.state.pool, &ids).await else {
        return chrome::unavailable(&ctx);
    };
    let rows = found
        .into_iter()
        .filter_map(|fork| {
            Some(Fork {
                owner: owners.remove(&fork.id)?,
                name: fork.name,
                description: fork.description.unwrap_or_default(),
                stars: fork.stars_count,
                updated_at: fork.updated_at.to_rfc3339(),
            })
        })
        .collect();
    show(
        &ctx,
        loaded,
        "Forks",
        InsightsPage::Forks {
            rows,
            page,
            has_more,
        },
    )
}
