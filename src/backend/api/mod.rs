//! HTTP composition: resource routers own their paths and representation-specific layers.
//! Health, crawler, picture and Git endpoints are not resources, so they stay at the root.
pub(crate) mod admin;
pub(crate) mod auth;
pub(crate) mod ci;
pub(crate) mod crawlers;
pub(crate) mod dashboard;
pub(crate) mod discovery;
pub(crate) mod files;
pub(crate) mod git_http;
pub(crate) mod health;
pub(crate) mod help;
pub(crate) mod http;
pub(crate) mod notifications;
pub(crate) mod organizations;
pub(crate) mod pictures;
pub(crate) mod repos;
pub mod user;
pub(crate) mod users;

use self::http::{config, middleware, render, static_files};
use crate::backend::app::AppState;
use anyhow::Result;
use axum::{
    http::{header, Method},
    routing::get,
    Extension, Router,
};
use std::{net::SocketAddr, sync::Arc};
use tower_http::{
    compression::CompressionLayer,
    cors::{AllowOrigin, CorsLayer},
};
use tracing::info;

pub fn create_router(state: AppState) -> Router {
    let state = Arc::new(state);
    let config = Arc::new(config::WebConfig::from_env());
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
            header::HeaderName::from_static("x-csrf-token"),
        ])
        .allow_credentials(true);

    Router::new()
        .merge(health::router())
        .merge(crawlers::router())
        // Needed before setup and in private mode.
        .route("/api/config", get(config::get_public_config))
        .merge(auth::router())
        .merge(user::router())
        .merge(users::router())
        .merge(admin::router())
        .merge(organizations::router())
        .merge(discovery::router())
        .merge(help::router())
        .merge(repos::router())
        .merge(ci::router())
        .merge(files::router())
        .merge(notifications::router())
        .merge(dashboard::router())
        .layer(axum::middleware::from_fn(middleware::bounded_form_body))
        // Git owns token-only auth, outside browser/session/CSRF and request-body layers.
        .merge(git_http::router())
        .route("/assets/*path", get(static_files::asset))
        .route("/wasm/*path", get(static_files::asset))
        .fallback(render::not_found)
        .with_state(state.clone())
        // Sees every response, including errors. It reads the state, so it sits inside the Extension layer.
        .layer(axum::middleware::from_fn(middleware::no_ai_html))
        // Resource middleware extracts this before handlers extract router state.
        .layer(Extension(state))
        .layer(Extension(config))
        .layer(cors)
        // tower-http skips upgrades, encoded responses and HEAD bodies.
        .layer(CompressionLayer::new())
}

/// Serves until `stop` is set, then waits for in-flight requests and the Git tasks they started.
pub async fn run_web_server(
    addr: SocketAddr,
    state: AppState,
    mut stop: tokio::sync::watch::Receiver<bool>,
) -> Result<()> {
    let git_tasks = state.git_tasks.clone();
    let app = create_router(state).layer(axum::middleware::from_fn_with_state(
        stop.clone(),
        cancel_on_shutdown,
    ));
    info!("Git HTTP server starting on http://{}", addr);
    let listener = tokio::net::TcpListener::bind(addr).await?;
    let mut listener_stop = stop.clone();
    let server = async move {
        axum::serve(listener, app)
            .with_graceful_shutdown(async move {
                let _ = listener_stop.wait_for(|stopped| *stopped).await;
            })
            .await
    };
    tokio::pin!(server);
    tokio::select! {
        result = &mut server => {
            git_tasks.drain().await;
            result?;
        }
        () = wait_for_stop(&mut stop) => {
            let (result, ()) = tokio::join!(server, git_tasks.drain());
            result?;
        }
    }
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
