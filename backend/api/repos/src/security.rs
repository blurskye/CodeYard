//! Repository security pages use the same alert rows and dismissal transaction as REST.

use axum::extract::Path;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::http::StatusCode;
use axum::http::Uri;
use axum::response::Response;
use axum::routing::get;
use axum::Form;
use axum::Router;
use codeyard_api_http::cell_routes::CellRoutes;
use codeyard_api_http::context::PageContext;
use codeyard_api_http::render::redirect_response;
use codeyard_api_http::shared::repos::chrome::Loaded;
use codeyard_api_http::shared::repos::forms::authorize_post;
use codeyard_app::app::AppState;
use codeyard_repos_core::repositories::store::repo_store;
use codeyard_storage::range::ByteRange as BlobRange;
use codeyard_webui::app::Page;
use codeyard_webui::excerpt;
use codeyard_webui::pages::repos::page::Page as RepoPage;
use codeyard_webui::pages::repos::security as view;
use serde_json::Value;
use std::collections::HashMap;
use std::sync::Arc;

pub fn page_router() -> Router<Arc<AppState>> {
    Router::new()
        .cell_route("/:owner/:repo/security/code-scanning", get(list))
        .cell_route("/:owner/:repo/security/code-scanning/:number", get(detail))
        .cell_route(
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
    let loaded = codeyard_api_http::shared::repos::chrome::load_database(&ctx, owner, name).await?;
    let allowed = current_authority(&ctx, loaded.repository.id)
        .await
        .map_err(|_| codeyard_api_http::shared::repos::chrome::unavailable(&ctx))?;
    if !allowed {
        return Err(ctx.error(StatusCode::NOT_FOUND, "Security alerts not found."));
    }
    Ok((ctx, loaded))
}
async fn current_authority(ctx: &PageContext, repo: i64) -> anyhow::Result<bool> {
    let Some(user) = &ctx.user else {
        return Ok(false);
    };
    let Some(session) = codeyard_api_http::session::extract_session_id(&ctx.headers) else {
        return Ok(false);
    };
    codeyard_db_security::code_scanning::security_read_allowed(
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
    let limit = crate::paging::DEFAULT_PER_PAGE;
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
    let filters = codeyard_db_security::code_scanning::AlertFilters {
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
        .ok_or(codeyard_db_security::code_scanning::Refusal::Denied)?;
    let session = codeyard_api_http::session::extract_session_id(&ctx.headers)
        .ok_or(codeyard_db_security::code_scanning::Refusal::Denied)?;
    let category = ctx.query.get("category").map(String::as_str);
    let snapshot = codeyard_db_security::code_scanning::read_alert_snapshot(
        &ctx.state.pool,
        actor.id,
        &session,
        &codeyard_db_security::code_scanning::AlertSelection {
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
        && crate::paging::has_next_page(page_number, rows.len(), limit, crate::paging::MAX_PAGE);
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
                codeyard_types::ref_path::file_url(
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
        codeyard_db_security::code_scanning::Refusal::Denied
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
    let page_number = match crate::paging::number(&ctx, "page", 1, 1..=crate::paging::MAX_PAGE) {
        Ok(number) => number,
        Err(response) => return response,
    };
    match model(&ctx, &loaded, None, page_number).await {
        Ok(page) => show(&ctx, loaded, page),
        Err(_) => codeyard_api_http::shared::repos::chrome::unavailable(&ctx),
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
        Err(_) => codeyard_api_http::shared::repos::chrome::unavailable(&ctx),
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
    let input = codeyard_db_security::code_scanning::Dismissal {
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
    let Some(session) = codeyard_api_http::session::extract_session_id(&ctx.headers) else {
        return ctx.error(StatusCode::FORBIDDEN, "Sign in again to change this alert.");
    };
    match codeyard_repos_pulls::code_scanning::dismiss(
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
                .map(|branch| {
                    format!(
                        "?branch={}",
                        codeyard_types::paths::encode_component(branch)
                    )
                })
                .unwrap_or_default();
            redirect_response(&format!(
                "{}/security/code-scanning/{number}{suffix}",
                loaded.chrome.base()
            ))
        }
        Err(error) => match error.downcast_ref::<codeyard_db_security::code_scanning::Refusal>() {
            Some(codeyard_db_security::code_scanning::Refusal::Denied) => {
                ctx.error(StatusCode::FORBIDDEN, "Alert dismissal denied.")
            }
            Some(codeyard_db_security::code_scanning::Refusal::Missing) => {
                ctx.error(StatusCode::NOT_FOUND, "Alert not found.")
            }
            _ => codeyard_api_http::shared::repos::chrome::unavailable(&ctx),
        },
    }
}
