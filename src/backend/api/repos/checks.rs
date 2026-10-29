use super::chrome;
use crate::backend::{
    api::{
        github,
        http::{context::PageContext, session},
    },
    app::AppState,
    db::{
        checks::{self, Credential},
        models::Repository,
    },
    repositories::access,
};
use crate::webui::{
    app::Page,
    pages::repos::{checks as view, page::Page as RepoPage},
};
use axum::{
    extract::{Form, OriginalUri, Path, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
    Router,
};
use std::{collections::HashMap, sync::Arc};

pub fn router() -> Router<Arc<AppState>> {
    Router::new()
        .route("/:owner/:repo/runs/:id", get(page))
        .route("/:owner/:repo/runs/:id/rerequest", post(rerequest))
        .layer(axum::extract::DefaultBodyLimit::max(16 * 1024))
}

async fn writable(ctx: &PageContext, repo: &Repository) -> bool {
    if let Some(user) = &ctx.user {
        access::can_write(&ctx.state.pool, user, repo)
            .await
            .unwrap_or(false)
    } else {
        false
    }
}
async fn page(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    OriginalUri(uri): OriginalUri,
    Path((owner, name, id)): Path<(String, String, i64)>,
) -> Response {
    let ctx = match PageContext::load(state, headers, &uri).await {
        Ok(ctx) => ctx,
        Err(response) => return response,
    };
    let loaded = match chrome::load_database(&ctx, &owner, &name).await {
        Ok(loaded) => loaded,
        Err(response) => return response,
    };
    let page = match ctx
        .query
        .get("page")
        .map(|value| value.parse::<usize>())
        .transpose()
    {
        Ok(value) if value.is_none_or(|p| (1..=(1_000_000 / ANNOTATIONS_PER_PAGE + 1)).contains(&p)) => value.unwrap_or(1),
        _ => return ctx.error(StatusCode::BAD_REQUEST, "Invalid annotation page."),
    };
    let run = match checks::run(&ctx.state.pool, loaded.repository.id, id).await {
        Ok(Some(run)) => run,
        Ok(None) => return ctx.error(StatusCode::NOT_FOUND, "Check not found."),
        Err(_) => return chrome::unavailable(&ctx),
    };
    let suite = match checks::suite(&ctx.state.pool, loaded.repository.id, run.suite_id).await {
        Ok(Some(suite)) => suite,
        _ => return chrome::unavailable(&ctx),
    };
    // Eight rows leave room for the run output even at six-byte JSON escaping per character.
    const ANNOTATIONS_PER_PAGE: usize = 8;
    let annotations =
        match checks::annotations(&ctx.state.pool, id, ((page - 1) * ANNOTATIONS_PER_PAGE) as i64, (ANNOTATIONS_PER_PAGE + 1) as i64).await {
            Ok(annotations) => annotations,
            Err(_) => return chrome::unavailable(&ctx),
        };
    let total = match checks::annotation_count(&ctx.state.pool, id).await {
        Ok(total) => total,
        Err(_) => return chrome::unavailable(&ctx),
    };
    let can_rerequest = writable(&ctx, &loaded.repository).await;
    let data = run.data.0;
    let check = view::Check {
        id,
        name: run.name,
        sha: suite.head_sha,
        status: run.status,
        conclusion: run.conclusion,
        title: data.title,
        summary: data.summary,
        text: data.text,
        details_url: data.details_url,
        total,
        page,
        has_more: annotations.len() > ANNOTATIONS_PER_PAGE,
        can_rerequest,
        annotations: annotations
            .into_iter()
            .take(ANNOTATIONS_PER_PAGE)
            .map(|a| view::Annotation {
                path: a.path.clone(),
                location: format!("lines {}–{}", a.start_line, a.end_line),
                level: a.annotation_level.clone(),
                title: a.title.clone(),
                message: a.message.clone(),
                details: a.raw_details.clone(),
            })
            .collect(),
    };
    if let Err((status, _)) = super::access::check_repo_read_access(&ctx.state.pool, &loaded.repository, &ctx.headers).await {
        return ctx.error(status, "Repository not found.");
    }
    ctx.page_model(
        "Check run",
        Page::Repositories(RepoPage::Check {
            repo: loaded.chrome,
            check: Box::new(check),
        }),
    )
}
async fn rerequest(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    OriginalUri(uri): OriginalUri,
    Path((owner, name, id)): Path<(String, String, i64)>,
    Form(form): Form<HashMap<String, String>>,
) -> Response {
    let ctx = match PageContext::load(state, headers, &uri).await {
        Ok(ctx) => ctx,
        Err(response) => return response,
    };
    if let Err(response) = ctx.check_csrf(&form) {
        return response;
    }
    let actor = match ctx.require_user() {
        Ok(user) => user.id,
        Err(response) => return response,
    };
    let loaded = match chrome::load_database(&ctx, &owner, &name).await {
        Ok(loaded) => loaded,
        Err(response) => return response,
    };
    if form
        .get("repository_id")
        .and_then(|value| value.parse::<i64>().ok())
        != Some(loaded.repository.id)
    {
        return ctx.error(
            StatusCode::CONFLICT,
            "The repository changed. Reload the check.",
        );
    }
    let Some(session) = session::extract_session_id(&ctx.headers) else {
        return ctx.error(StatusCode::UNAUTHORIZED, "Sign in to re-run checks.");
    };
    let run = match checks::run(&ctx.state.pool, loaded.repository.id, id).await {
        Ok(Some(run)) => run,
        Ok(None) => return ctx.error(StatusCode::NOT_FOUND, "Check not found."),
        Err(_) => return chrome::unavailable(&ctx),
    };
    match github::checks::request_again(
        &ctx.state,
        &loaded.repository,
        &owner,
        actor,
        Credential::Session(&session),
        run.suite_id,
        None,
    )
    .await
    {
        Ok(()) => ctx.redirect(&format!("{}/runs/{id}", loaded.chrome.base())),
        Err(error) => chrome::api_failure(&ctx, error.into_response().status()),
    }
}
