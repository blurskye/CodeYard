//! Warm a derived Git index for authenticated repository mutations. This middleware grants
//! nothing: failed/missing/stale proofs are rechecked and fail closed in the domain transaction.
use crate::backend::{
    api::{github, http::session},
    app::AppState,
    db,
    identity::interactions,
};
use axum::{
    extract::{FromRequestParts, Path, Request},
    http::{header, Method},
    middleware::Next,
    response::Response,
    Extension,
};
use std::{collections::HashMap, sync::Arc};

pub(crate) async fn prepare(
    Extension(state): Extension<Arc<AppState>>,
    request: Request,
    next: Next,
) -> Response {
    if (!request.headers().contains_key(header::COOKIE)
        && !request.headers().contains_key(header::AUTHORIZATION))
        || !matches!(
            *request.method(),
            Method::POST | Method::PUT | Method::PATCH | Method::DELETE
        )
    {
        return next.run(request).await;
    }
    let (mut parts, body) = request.into_parts();
    let params = Path::<HashMap<String, String>>::from_request_parts(&mut parts, &())
        .await
        .ok();
    if let Some(Path(params)) = params {
        if let (Some(owner), Some(name)) = (params.get("owner"), params.get("repo")) {
            if let Ok(Some(repository)) =
                db::repositories::records::get_repository(&state.pool, owner, name).await
            {
                // Avoid token/session work altogether when there is no contributor restriction.
                if db::interactions::effective_restriction(&state.pool, repository.id)
                    .await
                    .ok()
                    .flatten()
                    .is_some_and(|restriction| restriction.limit != "collaborators_only")
                {
                    let actor = if parts.headers.contains_key(header::AUTHORIZATION) {
                        github::interaction_actor(&state, &parts.headers).await
                    } else {
                        session::get_current_user(&state.pool, &parts.headers)
                            .await
                            .ok()
                    };
                    if let Some(actor) = actor {
                        if let Err(error) = interactions::prepare(&state, &actor, &repository).await
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
