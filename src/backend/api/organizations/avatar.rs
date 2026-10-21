//! The organization picture forms: choose an image and it becomes the picture, or
//! remove the current one. Owners and admins only; the store enforces that.

use super::organization;
use super::organization::{not_found, Fields};
use crate::backend::api::http::context::PageContext;
use crate::backend::api::pictures;
use crate::backend::api::user::avatar::avatar_status;
use crate::backend::app::AppState;
use crate::backend::identity::avatars;
use axum::extract::multipart::MultipartRejection;
use axum::extract::{DefaultBodyLimit, Form, Multipart, Path, State};
use axum::http::{HeaderMap, StatusCode, Uri};
use axum::response::Response;
use axum::routing::post;
use axum::Router;
use std::sync::Arc;

pub(super) fn router() -> Router<Arc<AppState>> {
    // Axum 0.7.9 uses :id, not the 0.8 {id} syntax.
    Router::new()
        .route(
            "/settings/organizations/:id/avatar",
            post(upload).layer(DefaultBodyLimit::max(pictures::BODY_LIMIT)),
        )
        .route(
            "/settings/organizations/:id/avatar/remove",
            post(remove).layer(DefaultBodyLimit::max(16 * 1024)),
        )
}

/// The organization page again, with what went wrong.
async fn refused(ctx: &PageContext, org: i64, status: StatusCode, message: &str) -> Response {
    organization::respond(
        ctx,
        Some(org),
        status,
        Some(message.to_owned()),
        Fields::new(),
    )
    .await
}

/// The organization this request is about, if the signed-in user can see it and its
/// governance is not suspended. Otherwise the response to send back.
async fn target(ctx: &PageContext, actor: i64, id: &str) -> Result<i64, Response> {
    let Some(org) = id.parse::<i64>().ok().filter(|id| *id > 0) else {
        return Err(not_found(ctx));
    };
    let current = organization::selected(ctx, actor, org).await?;
    if current.governance_suspended {
        return Err(refused(
            ctx,
            org,
            StatusCode::CONFLICT,
            "Organization governance is suspended.",
        )
        .await);
    }
    Ok(org)
}

async fn upload(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    headers: HeaderMap,
    uri: Uri,
    multipart: Result<Multipart, MultipartRejection>,
) -> Response {
    let ctx = match PageContext::load(state, headers, &uri).await {
        Ok(ctx) => ctx,
        Err(response) => return response,
    };
    let actor = match ctx.require_user() {
        Ok(user) => user.clone(),
        Err(response) => return response,
    };
    let Ok(multipart) = multipart else {
        return ctx.error(
            StatusCode::BAD_REQUEST,
            "Send the picture from the organization form.",
        );
    };
    let form = match pictures::read(multipart).await {
        Ok(form) => form,
        Err(error) => return ctx.error(error.status(), error.message()),
    };
    if let Err(response) = ctx.check_csrf(&form.fields) {
        return response;
    }
    let org = match target(&ctx, actor.id, &id).await {
        Ok(org) => org,
        Err(response) => return response,
    };
    let Some(bytes) = form.picture else {
        return refused(
            &ctx,
            org,
            StatusCode::UNPROCESSABLE_ENTITY,
            "Choose a picture to upload.",
        )
        .await;
    };
    match avatars::upload_org_avatar(&ctx.state.pool, &ctx.state.storage, &actor, org, bytes).await
    {
        Ok(()) => ctx.redirect(&organization::destination(&ctx, Some(org), false)),
        Err(error) => refused(&ctx, org, avatar_status(&error).0, &error.to_string()).await,
    }
}

async fn remove(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    headers: HeaderMap,
    uri: Uri,
    Form(form): Form<Fields>,
) -> Response {
    let ctx = match PageContext::load(state, headers, &uri).await {
        Ok(ctx) => ctx,
        Err(response) => return response,
    };
    let actor = match ctx.require_user() {
        Ok(user) => user.clone(),
        Err(response) => return response,
    };
    if let Err(response) = ctx.check_csrf(&form) {
        return response;
    }
    let org = match target(&ctx, actor.id, &id).await {
        Ok(org) => org,
        Err(response) => return response,
    };
    match avatars::set_org_avatar(&ctx.state.pool, &actor, org, None).await {
        Ok(()) => ctx.redirect(&organization::destination(&ctx, Some(org), false)),
        Err(error) => refused(&ctx, org, avatar_status(&error).0, &error.to_string()).await,
    }
}
