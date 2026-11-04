//! Repository security pages use the same alert rows and dismissal transaction as REST.

use crate::backend::api::http::context::PageContext;
use crate::backend::api::http::render::redirect_response;
use crate::backend::api::repos::chrome::Loaded;
use crate::backend::api::repos::forms::authorize_post;
use crate::backend::app::AppState;

use crate::backend::repositories::store::repo_store;
use crate::backend::repositories::store::BlobRange;
use axum::{
    extract::{Path, State},
    http::{HeaderMap, StatusCode, Uri},
    response::Response,
    routing::get,
    Form, Router,
};
use codeyard_webui::{
    app::Page,
    excerpt,
    pages::repos::{page::Page as RepoPage, security as view},
};
use serde_json::Value;
use std::{collections::HashMap, sync::Arc};

pub fn page_router() -> Router<Arc<AppState>> {
    Router::new()
        .route("/:owner/:repo/security/code-scanning", get(list))
        .route("/:owner/:repo/security/code-scanning/:number", get(detail))
        .route(
            "/:owner/:repo/security/code-scanning/:number/dismiss",
            axum::routing::post(dismiss),
        )
}
async fn open(
    state: Arc<AppState>,
    headers: HeaderMap,
    uri: &Uri,
    owner: &str,
    name: &str,
) -> Result<(PageContext, Loaded), Response> {
    let ctx = PageContext::load(state, headers, uri).await?;
    let loaded = crate::backend::api::repos::chrome::load_database(&ctx, owner, name).await?;
    let allowed = current_authority(&ctx, loaded.repository.id)
        .await
        .map_err(|_| crate::backend::api::repos::chrome::unavailable(&ctx))?;
    if !allowed {
        return Err(ctx.error(StatusCode::NOT_FOUND, "Security alerts not found."));
    }
    Ok((ctx, loaded))
}
async fn current_authority(ctx: &PageContext, repo: i64) -> anyhow::Result<bool> {
    let Some(user) = &ctx.user else {
        return Ok(false);
    };
    let Some(session) = crate::backend::api::http::session::extract_session_id(&ctx.headers) else {
        return Ok(false);
    };
    crate::backend::db::code_scanning::security_read_allowed(
        &ctx.state.pool,
        user.id,
        &session,
        repo,
    )
    .await
}

async fn model(
    ctx: &PageContext,
    loaded: &Loaded,
    number: Option<i64>,
    page_number: usize,
) -> anyhow::Result<view::Page> {
    let limit = crate::backend::api::repos::paging::DEFAULT_PER_PAGE;
    let offset = (page_number - 1) * limit;
    let explicit = ctx
        .query
        .get("branch")
        .filter(|branch| !branch.is_empty())
        .map(|branch| {
            if ["refs/heads/", "refs/tags/", "refs/pull/"]
                .iter()
                .any(|prefix| branch.starts_with(prefix))
            {
                branch.clone()
            } else {
                format!("refs/heads/{branch}")
            }
        });
    let filters = crate::backend::db::code_scanning::AlertFilters {
        reference: explicit.as_deref(),
        tool_name: ctx
            .query
            .get("tool")
            .filter(|value| !value.is_empty())
            .map(String::as_str),
        state: ctx
            .query
            .get("state")
            .filter(|value| !value.is_empty())
            .map(String::as_str),
        severity: ctx
            .query
            .get("severity")
            .filter(|value| !value.is_empty())
            .map(String::as_str),
        ..Default::default()
    };
    let actor = ctx
        .user
        .as_ref()
        .ok_or(crate::backend::db::code_scanning::Refusal::Denied)?;
    let session = crate::backend::api::http::session::extract_session_id(&ctx.headers)
        .ok_or(crate::backend::db::code_scanning::Refusal::Denied)?;
    let category = ctx.query.get("category").map(String::as_str);
    let snapshot = crate::backend::db::code_scanning::read_alert_snapshot(
        &ctx.state.pool,
        actor.id,
        &session,
        &crate::backend::db::code_scanning::AlertSelection {
            repo: loaded.repository.id,
            number,
            filters: &filters,
            category,
            offset: offset as i64,
            limit: (limit + 1) as i64,
        },
    )
    .await?;
    let rows = snapshot.rows;
    let mut instances = snapshot.instances;
    let has_next = number.is_none()
        && crate::backend::api::repos::paging::has_next_page(
            page_number,
            rows.len(),
            limit,
            crate::backend::api::repos::paging::MAX_PAGE,
        );
    let mut alerts = Vec::new();
    let mut selected = None;
    for row in rows.into_iter().take(limit) {
        let Some(instance) = instances.remove(&row.id) else {
            continue;
        };
        let rule: Value = serde_json::from_str(&row.rule)?;
        let tool: Value = serde_json::from_str(&row.tool)?;
        let location: Value = serde_json::from_str(&instance.location)?;
        let severity = rule["security_severity_level"]
            .as_str()
            .or_else(|| rule["severity"].as_str())
            .unwrap_or("warning")
            .to_owned();
        let mut alert = view::Alert {
            number: row.number,
            rule: excerpt(rule["id"].as_str().unwrap_or(""), 256),
            message: excerpt(
                &instance.message,
                if number.is_some() { 64 * 1024 } else { 1024 },
            ),
            tool: excerpt(tool["name"].as_str().unwrap_or(""), 256),
            severity,
            state: if row.state == "dismissed" {
                row.state
            } else {
                instance.state.clone()
            },
            branch: instance.ref_name.clone(),
            path: excerpt(location["path"].as_str().unwrap_or(""), 2048),
            line: location["start_line"].as_i64().unwrap_or(1),
            help: if number.is_some() {
                excerpt(rule["help"].as_str().unwrap_or(""), 64 * 1024)
            } else {
                String::new()
            },
            snippet: String::new(),
            code_url: String::new(),
        };
        let source_path = location["path"].as_str().unwrap_or("");
        if source_path.len() <= 4096 {
            // The code view with the line query reaches lines past the first preview.
            alert.code_url = format!(
                "{}?view=code&line={line}#L{line}",
                codeyard_webui::pages::repos::ref_path::file_url(
                    &loaded.chrome.base(),
                    "blob",
                    &instance.commit_sha,
                    source_path
                ),
                line = alert.line
            );
        }
        if number == Some(alert.number) && source_path.len() <= 4096 {
            let blob = repo_store(&loaded.repository, ctx.state.cluster.as_ref())
                .blob_range(
                    &instance.commit_sha,
                    source_path,
                    BlobRange::Window {
                        offset: 0,
                        length: 256 * 1024,
                    },
                )
                .await;
            let blob = match blob {
                Ok(blob) => Some(blob),
                Err(error)
                    if matches!(
                        error.kind,
                        codeyard_git::error::ErrorKind::NotFound
                            | codeyard_git::error::ErrorKind::MissingPath
                    ) =>
                {
                    None
                }
                Err(error) => return Err(error.into()),
            };
            if let Some(blob) = blob.filter(|blob| !blob.is_binary) {
                let start = alert.line.saturating_sub(4) as usize;
                alert.snippet = String::from_utf8_lossy(&blob.bytes)
                    .lines()
                    .enumerate()
                    .skip(start)
                    .take(9)
                    .map(|(i, line)| format!("{}  {line}", i + 1))
                    .collect::<Vec<_>>()
                    .join("\n");
            }
        }
        if number == Some(alert.number) {
            selected = Some(alert);
        } else {
            alerts.push(alert);
        }
    }
    anyhow::ensure!(
        current_authority(ctx, loaded.repository.id).await?,
        crate::backend::db::code_scanning::Refusal::Denied
    );
    Ok(view::Page {
        alerts,
        selected,
        filters: ctx.query.clone().into_iter().collect(),
        page_number,
        has_next,
    })
}
fn show(ctx: &PageContext, loaded: Loaded, page: view::Page) -> Response {
    ctx.page_model(
        "Code scanning",
        Page::Repositories(RepoPage::Security {
            repo: loaded.chrome,
            page,
        }),
    )
}
async fn list(
    State(state): State<Arc<AppState>>,
    Path((owner, name)): Path<(String, String)>,
    headers: HeaderMap,
    uri: Uri,
) -> Response {
    let (ctx, loaded) = match open(state, headers, &uri, &owner, &name).await {
        Ok(value) => value,
        Err(response) => return response,
    };
    let page_number = match crate::backend::api::repos::paging::number(
        &ctx,
        "page",
        1,
        1..=crate::backend::api::repos::paging::MAX_PAGE,
    ) {
        Ok(number) => number,
        Err(response) => return response,
    };
    match model(&ctx, &loaded, None, page_number).await {
        Ok(page) => show(&ctx, loaded, page),
        Err(_) => crate::backend::api::repos::chrome::unavailable(&ctx),
    }
}
async fn detail(
    State(state): State<Arc<AppState>>,
    Path((owner, name, number)): Path<(String, String, i64)>,
    headers: HeaderMap,
    uri: Uri,
) -> Response {
    let (ctx, loaded) = match open(state, headers, &uri, &owner, &name).await {
        Ok(value) => value,
        Err(response) => return response,
    };
    match model(&ctx, &loaded, Some(number), 1).await {
        Ok(page) if page.selected.is_some() => show(&ctx, loaded, page),
        Ok(_) => ctx.error(StatusCode::NOT_FOUND, "Code scanning alert not found."),
        Err(_) => crate::backend::api::repos::chrome::unavailable(&ctx),
    }
}
async fn dismiss(
    State(state): State<Arc<AppState>>,
    Path((owner, name, number)): Path<(String, String, i64)>,
    headers: HeaderMap,
    uri: Uri,
    Form(values): Form<HashMap<String, String>>,
) -> Response {
    let (ctx, loaded) = match open(state, headers, &uri, &owner, &name).await {
        Ok(value) => value,
        Err(response) => return response,
    };
    if let Err(response) = authorize_post(&ctx, &values) {
        return response;
    }
    if values
        .get("repository_id")
        .and_then(|id| id.parse::<i64>().ok())
        != Some(loaded.repository.id)
    {
        return ctx.error(StatusCode::CONFLICT, "The repository changed.");
    }
    let input = crate::backend::db::code_scanning::Dismissal {
        state: values.get("state").map_or("", String::as_str),
        reason: values.get("reason").map(String::as_str),
        comment: values.get("comment").map(String::as_str),
    };
    if input.validate().is_err() {
        return ctx.error(
            StatusCode::UNPROCESSABLE_ENTITY,
            "Choose a dismissal reason and a comment of at most 280 characters.",
        );
    }
    let Some(session) = crate::backend::api::http::session::extract_session_id(&ctx.headers) else {
        return ctx.error(StatusCode::FORBIDDEN, "Sign in again to change this alert.");
    };
    match crate::backend::code_scanning::dismiss(
        &ctx.state.pool,
        (ctx.user.as_ref().expect("authorized user").id, session),
        loaded.repository.id,
        number,
        &input,
    )
    .await
    {
        Ok(_) => {
            let suffix = values
                .get("branch")
                .filter(|branch| branch.len() <= 4096)
                .map(|branch| format!("?branch={}", codeyard_webui::encode_component(branch)))
                .unwrap_or_default();
            redirect_response(&format!(
                "{}/security/code-scanning/{number}{suffix}",
                loaded.chrome.base()
            ))
        }
        Err(error) => match error.downcast_ref::<crate::backend::db::code_scanning::Refusal>() {
            Some(crate::backend::db::code_scanning::Refusal::Denied) => {
                ctx.error(StatusCode::FORBIDDEN, "Alert dismissal denied.")
            }
            Some(crate::backend::db::code_scanning::Refusal::Missing) => {
                ctx.error(StatusCode::NOT_FOUND, "Alert not found.")
            }
            _ => crate::backend::api::repos::chrome::unavailable(&ctx),
        },
    }
}
