//! Personal tokens have no repository grants or approval state. Refusals still require an owner.
use super::owner;
use crate::backend::{
    api::github::{routes::Operation, Error},
    app::AppState,
};
use axum::{
    extract::{Path, Request},
    middleware::{from_fn, Next},
    response::Response,
    routing::MethodRouter,
};
use std::{collections::HashMap, sync::Arc};

pub(in crate::backend::api::github) fn authorize(
    operation: &Operation,
    route: MethodRouter<Arc<AppState>>,
) -> MethodRouter<Arc<AppState>> {
    if matches!(
        operation.id,
        "orgs/list-pat-grants"
            | "orgs/list-pat-grant-repositories"
            | "orgs/list-pat-grant-requests"
            | "orgs/list-pat-grant-request-repositories"
            | "orgs/review-pat-grant-request"
            | "orgs/review-pat-grant-requests-in-bulk"
            | "orgs/update-pat-access"
            | "orgs/update-pat-accesses"
    ) {
        route.route_layer(from_fn(require_owner))
    } else {
        route
    }
}

async fn require_owner(
    axum::Extension(state): axum::Extension<Arc<AppState>>,
    Path(params): Path<HashMap<String, String>>,
    request: Request,
    next: Next,
) -> Result<Response, Error> {
    let name = params.get("org").ok_or_else(Error::missing)?;
    owner(&state, request.headers(), name).await?;
    Ok(next.run(request).await)
}
