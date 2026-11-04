//! HTTP composition: resource routers own their paths and representation-specific layers.
//! Health, crawler, picture and Git endpoints are not resources, so they stay at the root.
pub(crate) mod admin;
mod artifact_metadata_pages;
mod attestations;
pub(crate) mod auth;
pub(crate) mod ci;
pub(crate) mod crawlers;
pub(crate) mod dashboard;
pub(crate) mod discovery;
pub(crate) mod files;
pub(crate) mod gists;
pub(crate) mod git_http;
pub(crate) mod github;
pub(crate) mod graphql;
pub(crate) mod health;
pub(crate) mod help;
pub(crate) mod http;
mod metrics;
pub(crate) mod notifications;
pub(crate) mod organizations;
pub(crate) mod packages;
pub(crate) mod pictures;
pub(crate) mod projects;
pub(crate) mod repos;
pub mod user;
pub(crate) mod users;

use crate::backend::app::AppState;
use anyhow::Result;
use axum::{
    http::{header, Method},
    routing::get,
    Extension, Router,
};
use std::{net::SocketAddr, sync::Arc};
use tower_http::{
    compression::{
        predicate::{DefaultPredicate, NotForContentType, Predicate},
        CompressionLayer,
    },
    cors::{AllowOrigin, CorsLayer},
};
use tracing::info;

pub fn create_router(state: AppState) -> Router {
    let state = Arc::new(state);
    let config = Arc::new(codeyard_model::http_config::WebConfig::from_env());
    let origins = config
        .cors_origins
        .iter()
        .filter_map(|origin| origin.parse().ok())
        .collect::<Vec<_>>();
    let cors = CorsLayer::new()
        .allow_origin(AllowOrigin::list(origins))
        .allow_methods([
            Method::GET,
            Method::POST,
            Method::PUT,
            Method::PATCH,
            Method::DELETE,
            Method::OPTIONS,
        ])
        .allow_headers([
            header::CONTENT_TYPE,
            header::AUTHORIZATION,
            header::COOKIE,
            header::IF_NONE_MATCH,
            header::HeaderName::from_static("x-csrf-token"),
            header::HeaderName::from_static("x-github-api-version"),
        ])
        // Browser API clients read pagination, caching and rate-limit headers, as on GitHub.
        .expose_headers([
            header::ETAG,
            header::LINK,
            header::LOCATION,
            header::RETRY_AFTER,
            header::HeaderName::from_static("x-ratelimit-limit"),
            header::HeaderName::from_static("x-ratelimit-remaining"),
            header::HeaderName::from_static("x-ratelimit-reset"),
            header::HeaderName::from_static("x-ratelimit-resource"),
            header::HeaderName::from_static("x-ratelimit-used"),
            header::HeaderName::from_static("x-oauth-scopes"),
            header::HeaderName::from_static("x-poll-interval"),
            header::HeaderName::from_static("x-github-api-version-selected"),
        ])
        .allow_credentials(true);

    Router::new()
        .merge(health::router())
        .merge(crawlers::router())
        .merge(auth::router())
        .merge(user::router())
        .merge(users::router())
        .merge(admin::router())
        .merge(organizations::router())
        .merge(projects::router())
        .merge(discovery::router())
        .merge(gists::router())
        .merge(help::router())
        .merge(repos::router())
        .merge(ci::router())
        .merge(files::router())
        .merge(notifications::router())
        .merge(dashboard::router())
        .merge(attestations::router())
        .merge(packages::browser_router())
        .layer(axum::middleware::from_fn(
            crate::backend::api::http::middleware::bounded_form_body,
        ))
        // Git owns token-only auth, outside browser/session/CSRF and request-body layers.
        .merge(git_http::router())
        .merge(github::router())
        .merge(packages::router())
        .merge(artifact_metadata_pages::router())
        .merge(ci::results::router())
        .merge(ci::oidc::router())
        .merge(graphql::router())
        .route(
            "/assets/*path",
            get(crate::backend::api::http::static_files::asset),
        )
        .route(
            "/wasm/*path",
            get(crate::backend::api::http::static_files::asset),
        )
        .fallback(crate::backend::api::http::render::not_found)
        // GitHub-style Maven URLs share the repository prefix; only repository-layout artifacts intercept it.
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            packages::maven_requests,
        ))
        .with_state(state.clone())
        // Sees every response, including errors. It reads the state, so it sits inside the Extension layer.
        .layer(axum::middleware::from_fn(
            crate::backend::api::http::render::render_page,
        ))
        .layer(axum::middleware::from_fn(
            crate::backend::api::http::middleware::no_ai_html,
        ))
        .layer(axum::middleware::from_fn(http::traffic::record_views))
        // Outside every handler, so no request keeps a client address that it sent itself.
        .layer(axum::middleware::from_fn(
            crate::backend::api::http::middleware::stamp_client_address,
        ))
        // Resource middleware extracts this before handlers extract router state.
        .layer(Extension(state))
        .layer(Extension(config))
        .layer(cors)
        // tower-http skips upgrades, encoded responses and HEAD bodies.
        .layer(
            CompressionLayer::new().compress_when(
                DefaultPredicate::new()
                    .and(NotForContentType::const_new(
                        "application/x-git-upload-pack-result",
                    ))
                    .and(NotForContentType::const_new(
                        "application/x-git-receive-pack-result",
                    )),
            ),
        )
}

/// Serves until `stop` is set, then waits for in-flight requests and the Git tasks they started.
pub async fn run_web_server(
    addr: SocketAddr,
    state: AppState,
    stop: tokio::sync::watch::Receiver<bool>,
) -> Result<()> {
    let listener = tokio::net::TcpListener::bind(addr).await?;
    run_web_listener(listener, state, stop).await
}

pub(crate) async fn run_web_listener(
    listener: tokio::net::TcpListener,
    state: AppState,
    stop: tokio::sync::watch::Receiver<bool>,
) -> Result<()> {
    let addr = listener.local_addr()?;
    let dependency_pool = state.pool.clone();
    let dependency_ci = state.ci_runs.clone();
    let dependency_notifications = state.notification_broadcaster.clone();
    let git_tasks = state.git_tasks.clone();
    let app = create_router(state).layer(axum::middleware::from_fn_with_state(
        stop.clone(),
        cancel_on_shutdown,
    ));
    info!("Git HTTP server starting on http://{}", addr);
    let mut listener_stop = stop.clone();
    let server = async move {
        axum::serve(
            listener,
            app.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .with_graceful_shutdown(async move {
            let _ = listener_stop.wait_for(|stopped| *stopped).await;
        })
        .await
    };
    let result = {
        let workers = crate::backend::repositories::dependencies::worker::run(
            dependency_pool,
            dependency_ci,
            dependency_notifications,
            stop.clone(),
        );
        tokio::pin!(server, workers);
        tokio::select! {result=&mut server=>result,()=&mut workers=>server.await}
    };
    // Accepted handlers may still be waiting for a Git session slot. Keep task
    // admission open until those handlers finish or their shutdown grace expires.
    git_tasks.drain().await;
    result?;
    Ok(())
}

/// Axum's connection tasks outlive the listener future. Cancel their unfinished handlers
/// after the drain grace, releasing any database connection held by a blocked query.
async fn cancel_on_shutdown(
    axum::extract::State(mut stop): axum::extract::State<tokio::sync::watch::Receiver<bool>>,
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    tokio::select! {
        response = next.run(request) => response,
        () = async {
            wait_for_stop(&mut stop).await;
            tokio::time::sleep(std::time::Duration::from_secs(30)).await;
        } => axum::http::StatusCode::SERVICE_UNAVAILABLE.into_response(),
    }
}

/// Resolves once `stop` is set. It returns nothing, so no watch guard is held in the
/// select output; that guard is not `Send`, and the server future must be `Send`.
async fn wait_for_stop(stop: &mut tokio::sync::watch::Receiver<bool>) {
    let _ = stop.wait_for(|stopped| *stopped).await;
}

#[cfg(test)]
mod tests;
