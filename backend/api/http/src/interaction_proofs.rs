//! Warm a derived Git index for authenticated repository mutations. This middleware grants
//! nothing: failed/missing/stale proofs are rechecked and fail closed in the domain transaction.

use axum::extract::Path;
use axum::extract::Request;
use axum::http::header;
use axum::http::Method;
use axum::http::StatusCode;
use axum::middleware::Next;
use axum::response::Response;
use axum::Extension;
use codeyard_app::app::AppState;
use std::collections::HashMap;
use std::sync::Arc;

pub async fn prepare(
    Extension(state): Extension<Arc<AppState>>,
    request: Request,
    next: Next,
) -> Response {
    // Review comment replies and review submissions end in /replies and /events.
    let contribution = [
        "/issues",
        "/issues/new",
        "/pulls",
        "/pulls/new",
        "/comments",
        "/replies",
        "/reviews",
        "/review",
        "/events",
    ]
    .iter()
    .any(|suffix| request.uri().path().ends_with(suffix));
    if !contribution
        || request.method() != Method::POST
        || (!request.headers().contains_key(header::COOKIE)
            && !request.headers().contains_key(header::AUTHORIZATION))
        || !matches!(
            *request.method(),
            Method::POST | Method::PUT | Method::PATCH | Method::DELETE
        )
    {
        return next.run(request).await;
    }
    let token_route = request.uri().path().starts_with("/api/v3/");
    let (mut parts, mut body) = request.into_parts();
    if !token_route {
        let mut headers = parts.headers.clone();
        if headers
            .get(header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .is_some_and(|value| value.starts_with("application/x-www-form-urlencoded"))
        {
            let bytes = match axum::body::to_bytes(body, 1024 * 1024).await {
                Ok(bytes) => bytes,
                Err(_) => return StatusCode::PAYLOAD_TOO_LARGE.into_response(),
            };
            let form: HashMap<String, String> =
                url::form_urlencoded::parse(&bytes).into_owned().collect();
            body = axum::body::Body::from(bytes);
            headers.remove("x-csrf-token");
            if let Some(token) = form.get("csrf_token").and_then(|token| token.parse().ok()) {
                headers.insert("x-csrf-token", token);
            }
        }
        if let Err(error) = crate::csrf::check(state.clone(), &headers).await {
            return error.into_response();
        }
    }
    let params = Path::<HashMap<String, String>>::from_request_parts(&mut parts, &())
        .await
        .ok();
    if let Some(Path(params)) = params {
        if let (Some(owner), Some(name)) = (params.get("owner"), params.get("repo")) {
            if let Ok(Some(repository)) =
                codeyard_db_repos::repositories::records::get_repository(&state.pool, owner, name)
                    .await
            {
                // Avoid token/session work altogether when there is no contributor restriction.
                if codeyard_db_identity::interactions::effective_restriction(
                    &state.pool,
                    repository.id,
                )
                .await
                .ok()
                .flatten()
                .is_some_and(|restriction| restriction.limit != "collaborators_only")
                {
                    let actor = if token_route {
                        match codeyard_app::api_shared::github::auth::authenticate(
                            &state,
                            &parts.headers,
                            true,
                        )
                        .await
                        {
                            Ok(Some(actor)) if actor.require_write().is_ok() => {
                                crate::shared::github::interaction_actor(&state, &parts.headers)
                                    .await
                            }
                            _ => None,
                        }
                    } else {
                        crate::session::get_current_user(&state.pool, &parts.headers)
                            .await
                            .ok()
                    };
                    if let Some(actor) = actor {
                        if let Err(error) =
                            codeyard_app::repositories::identity_interactions::prepare(
                                &state,
                                &actor,
                                &repository,
                            )
                            .await
                        {
                            tracing::warn!(repo_id=repository.id,user_id=actor.id,%error,"Contributor proof preparation unavailable");
                        }
                    }
                }
            }
        }
    }
    next.run(Request::from_parts(parts, body)).await
}
