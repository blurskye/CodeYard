//! The signed-in user's own issues and pull requests: `/issues` and `/pulls`.
use crate::backend::api::http::context::PageContext;
use crate::backend::app::AppState;
use crate::backend::dashboard;
use crate::backend::db;
use crate::backend::db::issues::IssueRelation;
use crate::backend::db::pull_requests::records::PullRequestRelation;
use crate::webui::app::Page;
use crate::webui::pages::dashboard::{RepositoryChoice, RepositoryChoices, WorkItem, WorkList};
use crate::webui::pages::repos::actions::logs::relative_time;
use crate::webui::pages::repos::issues::{Kind, Label};
use axum::{
    extract::{OriginalUri, State},
    http::{HeaderMap, StatusCode},
    response::Response,
    routing::get,
    Router,
};
use std::sync::Arc;

pub fn router() -> Router<Arc<AppState>> {
    Router::new()
        .route("/issues", get(issues_page))
        .route("/issues/new", get(new_issue_page))
        .route("/pulls", get(pulls_page))
}

/// `/issues/new`: the repositories the signed-in user can open an issue in, their own and the ones
/// shared with them. Each one links to its new issue form.
async fn new_issue_page(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    OriginalUri(uri): OriginalUri,
) -> Response {
    let ctx = match PageContext::load(state, headers, &uri).await {
        Ok(ctx) => ctx,
        Err(response) => return response,
    };
    let user = match ctx.require_user() {
        Ok(user) => user,
        Err(response) => return response,
    };
    let unavailable = || {
        ctx.error(
            StatusCode::SERVICE_UNAVAILABLE,
            "Repositories are temporarily unavailable. Try again shortly.",
        )
    };
    let page = ctx
        .query
        .get("page")
        .and_then(|v| v.parse::<u32>().ok())
        .unwrap_or(1)
        .clamp(1, 10_000);
    let query = ctx.query.get("q").map(String::as_str).unwrap_or("");
    if query.len() > 256 {
        return ctx.error(
            StatusCode::BAD_REQUEST,
            "Search text must be at most 256 bytes.",
        );
    }
    let offset = i64::from(page - 1) * 30;
    let (own, own_count) = match db::repositories::records::list_user_repository_page(
        &ctx.state.pool,
        user.id,
        true,
        query,
        30,
        offset,
    )
    .await
    {
        Ok(repos) => repos,
        Err(_) => return unavailable(),
    };
    let (shared, shared_count) = match db::repositories::records::list_shared_repository_page(
        &ctx.state.pool,
        user.id,
        query,
        30,
        offset,
    )
    .await
    {
        Ok(rows) => rows,
        Err(_) => return unavailable(),
    };
    let repos: Vec<_> = own
        .into_iter()
        .chain(shared.into_iter().map(|row| row.repository))
        .collect();
    let ids: Vec<i64> = repos.iter().map(|repo| repo.id).collect();
    let owners =
        match db::repositories::records::repository_owner_names(&ctx.state.pool, &ids).await {
            Ok(owners) => owners,
            Err(_) => return unavailable(),
        };
    let mut choices: Vec<RepositoryChoice> = repos
        .into_iter()
        .filter_map(|repo| {
            let owner = owners.get(&repo.id)?.clone();
            Some(RepositoryChoice {
                owner,
                name: repo.name,
            })
        })
        .collect();
    choices.sort_by(|a, b| (&a.owner, &a.name).cmp(&(&b.owner, &b.name)));
    ctx.page_model(
        "New issue",
        Page::IssueRepositories(RepositoryChoices {
            repositories: choices,
            page,
            query: query.to_owned(),
            has_older: offset + 30 < own_count || offset + 30 < shared_count,
        }),
    )
}

async fn issues_page(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    OriginalUri(uri): OriginalUri,
) -> Response {
    let ctx = match PageContext::load(state, headers, &uri).await {
        Ok(ctx) => ctx,
        Err(response) => return response,
    };
    let user = match ctx.require_user() {
        Ok(user) => user,
        Err(response) => return response,
    };
    let open = ctx.query.get("state").map(String::as_str) != Some("closed");
    let (tab, relation) = match ctx.query.get("tab").map(String::as_str) {
        Some("created") => ("created", IssueRelation::Created),
        _ => ("assigned", IssueRelation::Assigned),
    };
    let list = match dashboard::list_issues(&ctx.state.pool, user.id, relation, open).await {
        Ok(list) => list,
        Err(_) => {
            return ctx.error(
                StatusCode::SERVICE_UNAVAILABLE,
                "Issues are temporarily unavailable. Try again shortly.",
            )
        }
    };
    let counts = list.counts;
    let items: Vec<WorkItem> = list
        .rows
        .into_iter()
        .map(|row| {
            let created = row.entry.issue.created_at;
            WorkItem {
                owner: row.entry.repo_owner,
                repo: row.entry.repo_name,
                number: row.entry.issue.number,
                title: row.entry.issue.title,
                state: row.entry.issue.state,
                labels: row
                    .labels
                    .into_iter()
                    .map(|label| Label {
                        id: label.id,
                        name: label.name,
                        color: label.color,
                        description: label.description.unwrap_or_default(),
                    })
                    .collect(),
                comments: Some(row.comments),
                created: created.to_rfc3339(),
                opened: relative_time(created.timestamp()),
            }
        })
        .collect();
    let model = WorkList {
        kind: Kind::Issue,
        tab: tab.to_owned(),
        open,
        open_count: counts.open,
        closed_count: counts.closed,
        limited: items.len() as i64 >= dashboard::LIST_LIMIT,
        items,
    };
    ctx.page_model("Issues", Page::Dashboard(model))
}

async fn pulls_page(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    OriginalUri(uri): OriginalUri,
) -> Response {
    let ctx = match PageContext::load(state, headers, &uri).await {
        Ok(ctx) => ctx,
        Err(response) => return response,
    };
    let user = match ctx.require_user() {
        Ok(user) => user,
        Err(response) => return response,
    };
    let open = ctx.query.get("state").map(String::as_str) != Some("closed");
    let (tab, relation) = match ctx.query.get("tab").map(String::as_str) {
        Some("assigned") => ("assigned", PullRequestRelation::Assigned),
        Some("review-requested") => ("review-requested", PullRequestRelation::ReviewRequested),
        Some("involved") => ("involved", PullRequestRelation::Involved),
        _ => ("created", PullRequestRelation::Created),
    };
    let list = match dashboard::list_pull_requests(&ctx.state.pool, user.id, relation, open).await {
        Ok(list) => list,
        Err(_) => {
            return ctx.error(
                StatusCode::SERVICE_UNAVAILABLE,
                "Pull requests are temporarily unavailable. Try again shortly.",
            )
        }
    };
    let counts = list.counts;
    let items: Vec<WorkItem> = list
        .rows
        .into_iter()
        .map(|row| {
            let created = row.pull_request.created_at;
            WorkItem {
                owner: row.repo_owner,
                repo: row.repo_name,
                number: row.pull_request.number,
                title: row.pull_request.title,
                state: row.pull_request.state,
                labels: Vec::new(),
                comments: None,
                created: created.to_rfc3339(),
                opened: relative_time(created.timestamp()),
            }
        })
        .collect();
    let model = WorkList {
        kind: Kind::Pull,
        tab: tab.to_owned(),
        open,
        open_count: counts.open,
        closed_count: counts.closed,
        limited: items.len() as i64 >= dashboard::LIST_LIMIT,
        items,
    };
    ctx.page_model("Pull requests", Page::Dashboard(model))
}
