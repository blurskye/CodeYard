//! Observe only actual successful navigation responses, after SSR has completed.
use crate::context::PageContext;
use crate::render::accepts_page_model;
use axum::extract::Request;
use axum::http::header;
use axum::http::Method;
use axum::middleware::Next;
use axum::response::Response;
use axum::Extension;
use codeyard_app::app::AppState;
use codeyard_db_repos::repositories::traffic::Identity;
use codeyard_db_repos::repositories::traffic::Metric;
use codeyard_webui::app::Page;
use codeyard_webui::pages::repos::page::Page as RepoPage;
use std::sync::Arc;

#[derive(Clone)]
pub(super) struct Observation {
    repo: i64,
    identity: Identity,
    metric: Metric,
}
pub(super) fn page(ctx: &PageContext, title: &str, page: &Page) -> Option<Observation> {
    // Traffic readers need push access, not confidential report or governance access.
    if matches!(
        page,
        Page::Repositories(
            RepoPage::Security { .. }
                | RepoPage::DisclosureSecurity { .. }
                | RepoPage::Advisories { .. }
                | RepoPage::SecretSecurity { .. }
                | RepoPage::Dependencies { .. }
                | RepoPage::Settings { .. }
        )
    ) {
        return None;
    }
    if accepts_page_model(&ctx.headers)
        && ctx
            .headers
            .get("x-codeyard-navigation")
            .and_then(|header| header.to_str().ok())
            != Some("navigate")
    {
        return None;
    }
    let repo = page.repo_chrome()?.id;
    if repo <= 0 {
        return None;
    }
    let identity = match ctx.user.as_ref() {
        Some(actor) => Identity::account(actor.id),
        None => Identity::client(
            codeyard_app::api_shared::http::session::client_address(&ctx.headers)?,
            ctx.headers
                .get(header::USER_AGENT)
                .and_then(|header| header.to_str().ok())
                .unwrap_or(""),
        ),
    }?;
    let metric = Metric::view(
        &ctx.path,
        title,
        ctx.headers
            .get(header::REFERER)
            .and_then(|header| header.to_str().ok()),
        &codeyard_model::http_config::link_origin(),
    )
    .ok()?;
    Some(Observation {
        repo,
        identity,
        metric,
    })
}
pub async fn record_views(
    Extension(state): Extension<Arc<AppState>>,
    request: Request,
    next: Next,
) -> Response {
    let navigation = *request.method() == Method::GET;
    let mut response = next.run(request).await;
    let observation = response.extensions_mut().remove::<Observation>();
    if navigation && response.status().is_success() {
        if let Some(observation) = observation {
            codeyard_app::repositories::traffic::observe(
                &state.pool,
                observation.repo,
                &observation.identity,
                &observation.metric,
            )
            .await;
        }
    }
    response
}
