//! Read selection is shared by native pages and APIs; credentials and writes stay primary.
use axum::{extract::Request, http::Method, middleware::Next, response::Response, Extension};
use codeyard_app::app::AppState;
use codeyard_db_core::read_routing::{self, Credential};
use std::sync::Arc;

fn credential(
    pool: &codeyard_db_core::DbPool,
    headers: &axum::http::HeaderMap,
) -> Option<(bool, String)> {
    let token = codeyard_identity::github_auth::credential(headers)
        .map(str::to_owned)
        .or_else(|| crate::credentials::basic(headers).map(|(_, password)| password));
    if let Some(token) = token {
        return Some((true, codeyard_support::token_hash::token_hash(&token)));
    }
    crate::session::extract_local_session_id(pool, headers).map(|value| (false, value))
}
fn borrowed(credential: &(bool, String)) -> Credential<'_> {
    if credential.0 {
        Credential::Token(&credential.1)
    } else {
        Credential::Session(&credential.1)
    }
}

pub async fn route(
    Extension(state): Extension<Arc<AppState>>,
    request: Request,
    next: Next,
) -> Response {
    // GraphQL classifies its parsed operation before entering the same scope.
    if request.uri().path() == "/api/graphql" {
        return next.run(request).await;
    }
    let read = matches!(*request.method(), Method::GET | Method::HEAD);
    let headers = request.headers().clone();
    respond(&state, &headers, read, next.run(request)).await
}

pub async fn respond(
    state: &AppState,
    headers: &axum::http::HeaderMap,
    read: bool,
    work: impl std::future::Future<Output = Response>,
) -> Response {
    if !read_routing::has_replica(&state.pool) {
        return work.await;
    }
    let credential = credential(&state.pool, headers);
    let pinned = if let Some(credential) = &credential {
        // A failed routing hint chooses primary. The handler still authenticates normally.
        read_routing::primary_required(&state.pool, borrowed(credential), !read)
            .await
            .unwrap_or(true)
    } else {
        false
    };
    let response = read_routing::scope(&state.pool, read && !pinned, work).await;
    if !read && (response.status().is_success() || response.status().is_redirection()) {
        if let Some(credential) = &credential {
            if read_routing::primary_required(&state.pool, borrowed(credential), true)
                .await
                .is_err()
            {
                tracing::warn!("could not record primary read routing after a write");
            }
        }
    }
    response
}
