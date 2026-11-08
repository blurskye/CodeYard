//! Native private vulnerability reporting. Every confidential read and write goes through the DAL.

use crate::refs::references::resolve_references;
use axum::extract::DefaultBodyLimit;
use axum::extract::Form;
use axum::extract::Path;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::http::StatusCode;
use axum::http::Uri;
use axum::response::Response;
use axum::routing::get;
use axum::routing::post;
use axum::Router;
use codeyard_api_http::cell_routes::CellRoutes;
use codeyard_api_http::context::PageContext;
use codeyard_api_http::shared::repos::chrome::Loaded;
use codeyard_api_http::shared::repos::forms::authorize_post;
use codeyard_api_http::shared::repos::forms::field;
use codeyard_api_http::shared::repos::forms::with_status;
use codeyard_app::app::AppState;
use codeyard_webui::app::Page;
use codeyard_webui::pages::repos::disclosure::Message;
use codeyard_webui::pages::repos::disclosure::Page as SecurityPage;
use codeyard_webui::pages::repos::disclosure::ReportRow;
use codeyard_webui::pages::repos::page::Page as RepoPage;
use std::collections::HashMap;
use std::sync::Arc;

pub fn page_router() -> Router<Arc<AppState>> {
    Router::new()
        .cell_route("/:owner/:repo/security", get(index).post(create))
        .cell_route(
            "/:owner/:repo/security/reports/:report_id",
            get(thread).post(change),
        )
        .cell_route("/:owner/:repo/_forms/private-reporting", post(toggle))
        .layer(DefaultBodyLimit::max(256 * 1024))
}
async fn toggle(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    uri: Uri,
    Path((owner, name)): Path<(String, String)>,
    Form(form): Form<HashMap<String, String>>,
) -> Response {
    let (ctx, loaded) = match open(state, headers, &uri, &owner, &name).await {
        Ok(opened) => opened,
        Err(response) => return response,
    };
    if let Err(response) = authorize_post(&ctx, &form) {
        return response;
    }
    if !loaded.chrome.can_admin
        || field(&form, "repository_id").parse::<i64>().ok() != Some(loaded.repository.id)
    {
        return ctx.error(
            StatusCode::FORBIDDEN,
            "Private reporting administration denied.",
        );
    }
    let enabled = match field(&form, "enabled").parse::<bool>() {
        Ok(value) => value,
        Err(_) => {
            return ctx.error(
                StatusCode::UNPROCESSABLE_ENTITY,
                "Invalid private reporting setting.",
            )
        }
    };
    match codeyard_db_repos::repositories::security::set_enabled(
        &ctx.state.pool,
        ctx.database_actor(ctx.user.as_ref().unwrap().id),
        loaded.repository.id,
        enabled,
    )
    .await
    {
        Ok(()) => axum::response::IntoResponse::into_response(axum::response::Redirect::to(
            &format!("{}/security", loaded.chrome.base()),
        )),
        Err(error) => refused(&ctx, error),
    }
}
fn failure(error: &anyhow::Error) -> (StatusCode, &str) {
    if error.is::<codeyard_types::credentials::AccountRestricted>() {
        return (
            StatusCode::FORBIDDEN,
            "A live unrestricted credential is required.",
        );
    }
    if let Some(refusal) = error.downcast_ref::<codeyard_model::advisories::Refusal>() {
        let status = match refusal {
            codeyard_model::advisories::Refusal::Missing
            | codeyard_model::advisories::Refusal::Denied => StatusCode::NOT_FOUND,
            codeyard_model::advisories::Refusal::Invalid
            | codeyard_model::advisories::Refusal::Conflict => StatusCode::UNPROCESSABLE_ENTITY,
        };
        return (
            status,
            codeyard_api_http::shared::repos::chrome::safe_error(status),
        );
    }
    match error.downcast_ref::<codeyard_model::security_reports::Refusal>() {
        Some(
            codeyard_model::security_reports::Refusal::Missing
            | codeyard_model::security_reports::Refusal::Denied,
        ) => (
            StatusCode::NOT_FOUND,
            "Security report not found or not available to this account.",
        ),
        Some(codeyard_model::security_reports::Refusal::Invalid(message)) => {
            (StatusCode::UNPROCESSABLE_ENTITY, message)
        }
        Some(codeyard_model::security_reports::Refusal::Limit) => (
            StatusCode::TOO_MANY_REQUESTS,
            "Security reporting limit reached. Try again later.",
        ),
        None => (
            StatusCode::SERVICE_UNAVAILABLE,
            "Security report storage is unavailable.",
        ),
    }
}
fn refused(ctx: &PageContext, error: anyhow::Error) -> Response {
    let (status, message) = failure(&error);
    ctx.error(status, message)
}
fn page_number(ctx: &PageContext) -> Result<usize, Response> {
    let page = ctx
        .query
        .get("page")
        .map_or(Ok(1), |page| page.parse::<usize>())
        .map_err(|_| ctx.error(StatusCode::BAD_REQUEST, "Invalid page"))?;
    if !(1..=5000).contains(&page) {
        return Err(ctx.error(StatusCode::BAD_REQUEST, "Invalid page"));
    }
    Ok(page)
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
    Ok((ctx, loaded))
}
fn show(ctx: &PageContext, loaded: Loaded, page: SecurityPage) -> Response {
    ctx.page_model(
        "Security",
        Page::Repositories(RepoPage::DisclosureSecurity {
            repo: loaded.chrome,
            page: Box::new(page),
        }),
    )
}
async fn index_model(
    ctx: &PageContext,
    loaded: Loaded,
    page: usize,
    error: Option<String>,
    title: String,
    body: String,
) -> Response {
    let enabled = match codeyard_db_repos::repositories::security::enabled(
        &ctx.state.pool,
        loaded.repository.id,
    )
    .await
    {
        Ok(enabled) => enabled,
        Err(error) => return refused(ctx, error),
    };
    let mut rows = if let Some(user) = &ctx.user {
        match codeyard_db_repos::repositories::security::list_reports(
            &ctx.state.pool,
            ctx.database_actor(user.id),
            loaded.repository.id,
            21,
            ((page - 1) * 20) as i64,
        )
        .await
        {
            Ok(rows) => rows,
            Err(error) => return refused(ctx, error),
        }
    } else {
        Vec::new()
    };
    let more = rows.len() > 20;
    rows.truncate(20);
    let rows = rows
        .into_iter()
        .map(|row| ReportRow {
            id: row.id,
            title: row.title,
            reporter: row.reporter.unwrap_or_else(|| "Deleted account".into()),
            date: row.created_at.to_rfc3339(),
            state: row.state,
        })
        .collect();
    show(
        ctx,
        loaded,
        SecurityPage::Index {
            enabled,
            rows,
            page,
            more,
            error,
            title,
            body,
        },
    )
}
async fn thread_model(
    ctx: &PageContext,
    loaded: Loaded,
    id: i64,
    page: usize,
    error: Option<String>,
    reply: String,
) -> Response {
    let Some(actor) = ctx.user.as_ref().map(|user| user.id) else {
        return ctx.error(
            StatusCode::UNAUTHORIZED,
            "Sign in to read private security reports.",
        );
    };
    let mut thread = match codeyard_db_repos::repositories::security::report_thread(
        &ctx.state.pool,
        ctx.database_actor(actor),
        loaded.repository.id,
        id,
        ((page - 1) * 20) as i64,
    )
    .await
    {
        Ok(thread) => thread,
        Err(error) => return refused(ctx, error),
    };
    let more = thread.replies.len() > 20;
    thread.replies.truncate(20);
    let mut texts = vec![thread.report.body.as_str(), reply.as_str()];
    texts.extend(thread.replies.iter().map(|reply| reply.body.as_str()));
    let references = match resolve_references(
        ctx,
        &loaded.repository,
        &loaded.chrome.owner,
        &loaded.chrome.name,
        &texts,
        false,
    )
    .await
    {
        Ok(references) => references,
        Err(response) => return response,
    };
    let report = Message {
        author: thread
            .report
            .reporter
            .unwrap_or_else(|| "Deleted account".into()),
        date: thread.report.created_at.to_rfc3339(),
        body: thread.report.body,
    };
    let replies = thread
        .replies
        .into_iter()
        .map(|row| Message {
            author: row.author.unwrap_or_else(|| "Deleted account".into()),
            date: row.created_at.to_rfc3339(),
            body: row.body,
        })
        .collect();
    show(
        ctx,
        loaded,
        SecurityPage::Thread {
            id,
            title: thread.report.title,
            report,
            state: thread.report.state,
            replies,
            page,
            more,
            error,
            reply,
            references: Box::new(references),
        },
    )
}
async fn index(
    State(state): State<Arc<AppState>>,
    Path((owner, name)): Path<(String, String)>,
    headers: HeaderMap,
    uri: Uri,
) -> Response {
    let (ctx, loaded) = match open(state, headers, &uri, &owner, &name).await {
        Ok(opened) => opened,
        Err(response) => return response,
    };
    let page = match page_number(&ctx) {
        Ok(page) => page,
        Err(response) => return response,
    };
    index_model(&ctx, loaded, page, None, String::new(), String::new()).await
}
async fn thread(
    State(state): State<Arc<AppState>>,
    Path((owner, name, id)): Path<(String, String, i64)>,
    headers: HeaderMap,
    uri: Uri,
) -> Response {
    let (ctx, loaded) = match open(state, headers, &uri, &owner, &name).await {
        Ok(opened) => opened,
        Err(response) => return response,
    };
    let page = match page_number(&ctx) {
        Ok(page) => page,
        Err(response) => return response,
    };
    thread_model(&ctx, loaded, id, page, None, String::new()).await
}
async fn create(
    State(state): State<Arc<AppState>>,
    Path((owner, name)): Path<(String, String)>,
    headers: HeaderMap,
    uri: Uri,
    Form(form): Form<HashMap<String, String>>,
) -> Response {
    let (ctx, loaded) = match open(state, headers, &uri, &owner, &name).await {
        Ok(opened) => opened,
        Err(response) => return response,
    };
    if let Err(response) = authorize_post(&ctx, &form) {
        return response;
    }
    let Some(actor) = ctx.user.as_ref().map(|user| user.id) else {
        return ctx.error(
            StatusCode::UNAUTHORIZED,
            "Sign in to report a vulnerability.",
        );
    };
    if field(&form, "repository_id").parse::<i64>().ok() != Some(loaded.repository.id) {
        return ctx.error(
            StatusCode::CONFLICT,
            "The repository changed. Reload before reporting.",
        );
    }
    let title = field(&form, "title");
    let body = field(&form, "body");
    let result = match codeyard_model::security_reports::NewReport::new(&title, &body) {
        Ok(_) => codeyard_db_security::advisories::create(
            &ctx.state.pool,
            ctx.database_actor(actor),
            loaded.repository.id,
            &serde_json::json!({"summary":title,"description":body,"vulnerabilities":[]}),
            true,
        )
        .await
        .and_then(|advisory| {
            advisory
                .report_id
                .ok_or_else(|| anyhow::anyhow!("Missing security report identity"))
        }),
        Err(error) => Err(error.into()),
    };
    match result {
        Ok(id) => ctx.redirect(&format!("{}/security/reports/{id}", loaded.chrome.base())),
        Err(error) => {
            let (status, message) = failure(&error);
            with_status(
                index_model(&ctx, loaded, 1, Some(message.to_owned()), title, body).await,
                status,
            )
        }
    }
}
async fn change(
    State(state): State<Arc<AppState>>,
    Path((owner, name, id)): Path<(String, String, i64)>,
    headers: HeaderMap,
    uri: Uri,
    Form(form): Form<HashMap<String, String>>,
) -> Response {
    let (ctx, loaded) = match open(state, headers, &uri, &owner, &name).await {
        Ok(opened) => opened,
        Err(response) => return response,
    };
    if let Err(response) = authorize_post(&ctx, &form) {
        return response;
    }
    let Some(actor) = ctx.user.as_ref().map(|user| user.id) else {
        return ctx.error(
            StatusCode::UNAUTHORIZED,
            "Sign in to reply to a private report.",
        );
    };
    if field(&form, "repository_id").parse::<i64>().ok() != Some(loaded.repository.id)
        || field(&form, "report_id").parse::<i64>().ok() != Some(id)
    {
        return ctx.error(
            StatusCode::CONFLICT,
            "The report changed. Reload before replying.",
        );
    }
    let body = field(&form, "body");
    let intent = field(&form, "intent");
    let result = match intent.as_str() {
        // A reply returns how many replies came before it, which names the page that shows it.
        "reply" => codeyard_db_repos::repositories::security::reply(
            &ctx.state.pool,
            ctx.database_actor(actor),
            loaded.repository.id,
            id,
            &body,
        )
        .await
        .map(Some),
        "close" => codeyard_db_repos::repositories::security::set_state(
            &ctx.state.pool,
            ctx.database_actor(actor),
            loaded.repository.id,
            id,
            true,
        )
        .await
        .map(|()| None),
        "reopen" => codeyard_db_repos::repositories::security::set_state(
            &ctx.state.pool,
            ctx.database_actor(actor),
            loaded.repository.id,
            id,
            false,
        )
        .await
        .map(|()| None),
        _ => return ctx.error(StatusCode::BAD_REQUEST, "Unknown report action"),
    };
    match result {
        Ok(None) if !body.is_empty() => thread_model(&ctx, loaded, id, 1, None, body).await,
        Ok(None) => ctx.redirect(&format!("{}/security/reports/{id}", loaded.chrome.base())),
        Ok(Some(earlier)) => ctx.redirect(&format!(
            "{}/security/reports/{id}?page={}",
            loaded.chrome.base(),
            earlier / 20 + 1
        )),
        Err(error) => {
            let (status, message) = failure(&error);
            with_status(
                thread_model(&ctx, loaded, id, 1, Some(message.to_owned()), body).await,
                status,
            )
        }
    }
}
