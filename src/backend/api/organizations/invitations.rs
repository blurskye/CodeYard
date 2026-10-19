//! The invitee's answer to an organization invitation. Membership starts only on accept.

use super::organization::{self, unavailable, unit, Fields};
use super::INDEX;
use crate::backend::api::http::context::PageContext;
use crate::backend::app::AppState;
use crate::backend::db::organizations;
use crate::backend::db::organizations::OrganizationOutcome::{Applied, Denied, Missing, Suspended};
use axum::extract::{Form, Path, State};
use axum::http::{HeaderMap, StatusCode, Uri};
use axum::response::Response;
use std::sync::Arc;

pub(super) async fn accept(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    uri: Uri,
    Path(org): Path<i64>,
    Form(form): Form<Fields>,
) -> Response {
    answer(state, headers, uri, org, form, true).await
}

pub(super) async fn decline(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    uri: Uri,
    Path(org): Path<i64>,
    Form(form): Form<Fields>,
) -> Response {
    answer(state, headers, uri, org, form, false).await
}

async fn answer(
    state: Arc<AppState>,
    headers: HeaderMap,
    uri: Uri,
    org: i64,
    form: Fields,
    accept: bool,
) -> Response {
    let ctx = match PageContext::load(state, headers, &uri).await {
        Ok(ctx) => ctx,
        Err(response) => return response,
    };
    let actor = match ctx.require_user() {
        Ok(user) => user.id,
        Err(response) => return response,
    };
    if let Err(response) = ctx.check_csrf(&form) {
        return response;
    }
    let pool = &ctx.state.pool;
    let outcome = if accept {
        organizations::accept_organization_invitation(pool, actor, org)
            .await
            .map(unit)
    } else {
        organizations::decline_organization_invitation(pool, actor, org).await
    };
    let outcome = match outcome {
        Ok(outcome) => outcome,
        Err(_) => return unavailable(&ctx),
    };
    let (status, message) = match outcome {
        Applied(()) if accept => return ctx.redirect(&format!("{INDEX}/{org}")),
        Applied(()) => return ctx.redirect(INDEX),
        Missing => (StatusCode::NOT_FOUND, "This invitation no longer exists."),
        Suspended => (
            StatusCode::CONFLICT,
            "This organization has no active owner, so the invitation cannot be accepted.",
        ),
        Denied => (
            StatusCode::FORBIDDEN,
            "This invitation cannot be answered from your account.",
        ),
        _ => (StatusCode::CONFLICT, "The invitation changed. Try again."),
    };
    organization::respond(&ctx, None, status, Some(message.into()), Fields::new()).await
}
