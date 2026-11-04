use axum::extract::Form;
use axum::extract::OriginalUri;
use axum::extract::Path;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::http::StatusCode;
use axum::response::Response;
use axum::routing::get;
use axum::routing::post;
use axum::Router;
use codeyard_api_http::context::PageContext;
use codeyard_app::app::AppState;
use codeyard_db_actions::checks::Credential;
use codeyard_db_actions::checks::ANNOTATIONS_PER_PAGE;
use codeyard_db_core::models::Repository;
use codeyard_webui::app::Page;
use codeyard_webui::pages::repos::checks as view;
use codeyard_webui::pages::repos::page::Page as RepoPage;
use std::collections::HashMap;
use std::sync::Arc;

pub fn router() -> Router<Arc<AppState>> {
    Router::new()
        .route("/:owner/:repo/runs/:id", get(page))
        .route("/:owner/:repo/runs/:id/rerequest", post(submit))
        .route("/:owner/:repo/runs/:id/action", post(submit))
        .layer(axum::extract::DefaultBodyLimit::max(16 * 1024))
}

async fn writable(ctx: &PageContext, repo: &Repository) -> bool {
    if let Some(user) = &ctx.user {
        codeyard_identity::repository_access::can_write(&ctx.state.pool, user, repo)
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
    let loaded =
        match codeyard_api_http::shared::repos::chrome::load_database(&ctx, &owner, &name).await {
            Ok(loaded) => loaded,
            Err(response) => return response,
        };
    let page = match ctx
        .query
        .get("page")
        .map(|value| value.parse::<usize>())
        .transpose()
    {
        Ok(value)
            if value.is_none_or(|p| (1..=(1_000_000 / ANNOTATIONS_PER_PAGE + 1)).contains(&p)) =>
        {
            value.unwrap_or(1)
        }
        _ => return ctx.error(StatusCode::BAD_REQUEST, "Invalid annotation page."),
    };
    let session = codeyard_api_http::session::extract_session_id(&ctx.headers);
    let codeyard_db_actions::checks::PageSnapshot {
        run,
        suite,
        annotations,
        total,
    } = match codeyard_db_actions::checks::page_snapshot(
        &ctx.state.pool,
        ctx.user.as_ref().map(|user| user.id),
        session.as_deref(),
        loaded.repository.id,
        id,
        ((page - 1) * ANNOTATIONS_PER_PAGE) as i64,
    )
    .await
    {
        Ok(snapshot) => snapshot,
        Err(error) if error.is::<codeyard_db_actions::checks::Refusal>() => {
            return ctx.error(StatusCode::NOT_FOUND, "Check not found.")
        }
        Err(_) => return codeyard_api_http::shared::repos::chrome::unavailable(&ctx),
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
        images: data
            .images
            .into_iter()
            .map(|image| (image.alt, image.image_url, image.caption))
            .collect(),
        actions: data
            .actions
            .into_iter()
            .map(|action| view::Action {
                label: action.label,
                description: action.description,
                identifier: action.identifier,
            })
            .collect(),
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
    if let Err((status, _)) = codeyard_api_http::shared::repos::access::check_repo_read_access(
        &ctx.state.pool,
        &loaded.repository,
        &ctx.headers,
    )
    .await
    {
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
async fn submit(
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
    let loaded =
        match codeyard_api_http::shared::repos::chrome::load_database(&ctx, &owner, &name).await {
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
    let Some(session) = codeyard_api_http::session::extract_session_id(&ctx.headers) else {
        return ctx.error(StatusCode::UNAUTHORIZED, "Sign in to re-run checks.");
    };
    let run =
        match codeyard_db_actions::checks::run(&ctx.state.pool, loaded.repository.id, id).await {
            Ok(Some(run)) => run,
            Ok(None) => return ctx.error(StatusCode::NOT_FOUND, "Check not found."),
            Err(_) => return codeyard_api_http::shared::repos::chrome::unavailable(&ctx),
        };
    let result = if uri.path().ends_with("/action") {
        codeyard_api_github::checks::request_action(
            &ctx.state,
            &loaded.repository,
            &owner,
            actor,
            Credential::Session(&session),
            id,
            form.get("identifier").cloned().unwrap_or_default(),
        )
        .await
    } else {
        codeyard_api_github::checks::request_again(
            &ctx.state,
            &loaded.repository,
            &owner,
            actor,
            Credential::Session(&session),
            codeyard_db_core::queries::ci::CiCredential::Session(session.clone()),
            run.suite_id,
            None,
        )
        .await
    };
    match result {
        Ok(()) => ctx.redirect(&format!("{}/runs/{id}", loaded.chrome.base())),
        Err(error) => codeyard_api_http::shared::repos::chrome::api_failure(
            &ctx,
            error.into_response().status(),
        ),
    }
}
