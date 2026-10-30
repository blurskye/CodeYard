//! Native security forms share alert reconciliation and live mutation authority with REST.
use super::{
    chrome::{self, Loaded},
    forms::authorize_post,
};
use crate::{
    backend::{
        api::http::{context::PageContext, render::redirect_response},
        app::AppState,
        db::{
            self,
            dependabot_alerts::{self as store, Update},
        },
    },
    webui::{
        app::Page,
        pages::repos::{dependabot as view, page::Page as RepoPage},
    },
};
use axum::{
    extract::{Path, State},
    http::{HeaderMap, StatusCode, Uri},
    response::Response,
    routing::{get, post},
    Form, Router,
};
use serde_json::Value;
use std::{collections::HashMap, sync::Arc};
pub fn page_router() -> Router<Arc<AppState>> {
    Router::new()
        .route("/:owner/:repo/security/dependabot", get(list))
        .route(
            "/:owner/:repo/security/dependabot/:number/dismiss",
            post(dismiss),
        )
        .route("/:owner/:repo/security/dependabot/import", post(import))
}
async fn open(
    state: Arc<AppState>,
    headers: HeaderMap,
    uri: &Uri,
    owner: &str,
    name: &str,
) -> Result<(PageContext, Loaded), Response> {
    let ctx = PageContext::load(state, headers, uri).await?;
    let loaded = chrome::load_database(&ctx, owner, name).await?;
    if ctx.user.is_none() {
        return Err(ctx.error(StatusCode::NOT_FOUND, "Dependabot alerts not found."));
    }
    Ok((ctx, loaded))
}
fn failure(ctx: &PageContext, error: anyhow::Error) -> Response {
    use db::advisories::Refusal;
    if let Some(graph @ (db::dependency_graph::GraphError::Limit | db::dependency_graph::GraphError::Disabled)) = error.downcast_ref::<db::dependency_graph::GraphError>() {
        return ctx.error(StatusCode::UNPROCESSABLE_ENTITY, &graph.to_string());
    }
    match error.downcast_ref::<Refusal>() {
        Some(Refusal::Denied) => {
            ctx.error(StatusCode::FORBIDDEN, "Dependabot administration denied.")
        }
        Some(Refusal::Missing) => ctx.error(StatusCode::NOT_FOUND, "Alert not found."),
        Some(Refusal::Invalid) => ctx.error(
            StatusCode::UNPROCESSABLE_ENTITY,
            "Invalid advisory or dismissal.",
        ),
        Some(Refusal::Conflict) => ctx.error(
            StatusCode::CONFLICT,
            "The dependency is no longer vulnerable.",
        ),
        None => chrome::unavailable(ctx),
    }
}
async fn list(
    State(state): State<Arc<AppState>>,
    Path((owner, name)): Path<(String, String)>,
    headers: HeaderMap,
    uri: Uri,
) -> Response {
    let (ctx, loaded) = match open(state, headers, &uri, &owner, &name).await {
        Ok(v) => v,
        Err(r) => return r,
    };
    let page = match super::paging::number(&ctx, "page", 1, 1..=super::paging::MAX_PAGE) {
        Ok(p) => p,
        Err(r) => return r,
    };
    let filter = ctx.query.get("state").map_or("open", String::as_str);
    if !matches!(filter, "open" | "dismissed" | "fixed" | "all") {
        return ctx.error(StatusCode::BAD_REQUEST, "Invalid alert state.");
    }
    let rows = match store::alerts(
        &ctx.state.pool,
        ctx.database_actor(ctx.user.as_ref().unwrap().id),
        loaded.repository.id,
    )
    .await
    {
        Ok(r) => r,
        Err(e) => return failure(&ctx, e),
    };
    let mut rows = rows
        .into_iter()
        .filter(|r| filter == "all" || r.state == filter)
        .skip((page - 1) * 30)
        .take(31)
        .collect::<Vec<_>>();
    let more = rows.len() > 30;
    rows.truncate(30);
    let mut alerts = Vec::new();
    for row in rows {
        let (Ok(a), Ok(v)) = (
            serde_json::from_str::<Value>(&row.advisory),
            serde_json::from_str::<Value>(&row.vulnerability),
        ) else {
            return chrome::unavailable(&ctx);
        };
        alerts.push(view::Alert {
            number: row.number,
            summary: a["summary"].as_str().unwrap_or("").into(),
            description: a["description"].as_str().unwrap_or("").into(),
            severity: a["severity"].as_str().unwrap_or("").into(),
            ghsa_id: row.ghsa_id,
            package: row.package,
            version: row.version,
            manifest: row.manifest,
            state: row.state,
            reason: row.dismissed_reason,
            comment: row.dismissed_comment,
            patched: v["first_patched_version"].as_str().map(str::to_owned),
        });
    }
    let model = view::Page {
        alerts,
        state: filter.into(),
        page,
        more,
        can_import: ctx.user.as_ref().is_some_and(|u| u.is_admin),
    };
    ctx.page_model(
        "Dependabot alerts",
        Page::Repositories(RepoPage::Dependabot {
            repo: loaded.chrome,
            page: model,
        }),
    )
}
async fn mutate(
    state: Arc<AppState>,
    headers: HeaderMap,
    uri: Uri,
    target: (String, String, Option<i64>),
    values: HashMap<String, String>,
) -> Response {
    let (owner, name, number) = target;
    let (ctx, loaded) = match open(state, headers, &uri, &owner, &name).await {
        Ok(v) => v,
        Err(r) => return r,
    };
    if let Err(r) = authorize_post(&ctx, &values) {
        return r;
    }
    if values
        .get("repository_id")
        .and_then(|v| v.parse::<i64>().ok())
        != Some(loaded.repository.id)
    {
        return ctx.error(StatusCode::CONFLICT, "The repository changed.");
    }
    let actor = ctx.user.as_ref().unwrap().id;
    let result = if let Some(number) = number {
        let input = Update {
            state: values.get("state").cloned().unwrap_or_default(),
            dismissed_reason: values.get("dismissed_reason").cloned(),
            dismissed_comment: values.get("dismissed_comment").cloned(),
        };
        store::update(&ctx.state.pool, ctx.database_actor(actor), loaded.repository.id, number, &input)
            .await
            .map(|_| ())
    } else {
        let document = values
            .get("document")
            .and_then(|s| serde_json::from_str::<Value>(s).ok());
        let Some(document) = document else {
            return ctx.error(StatusCode::UNPROCESSABLE_ENTITY, "Invalid advisory JSON.");
        };
        store::import(&ctx.state.pool, actor, &document)
            .await
            .map(|_| ())
    };
    match result {
        Ok(()) => redirect_response(&format!("{}/security/dependabot", loaded.chrome.base())),
        Err(e) => failure(&ctx, e),
    }
}
async fn dismiss(
    State(state): State<Arc<AppState>>,
    Path((owner, name, number)): Path<(String, String, i64)>,
    headers: HeaderMap,
    uri: Uri,
    Form(values): Form<HashMap<String, String>>,
) -> Response {
    mutate(state, headers, uri, (owner, name, Some(number)), values).await
}
async fn import(
    State(state): State<Arc<AppState>>,
    Path((owner, name)): Path<(String, String)>,
    headers: HeaderMap,
    uri: Uri,
    Form(values): Form<HashMap<String, String>>,
) -> Response {
    mutate(state, headers, uri, (owner, name, None), values).await
}
