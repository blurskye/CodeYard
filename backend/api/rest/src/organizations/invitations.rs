//! The invitee's answer to an organization invitation. Membership starts only on accept.

use crate::organizations::organization::unavailable;
use crate::organizations::organization::unit;
use crate::organizations::organization::Fields;
use crate::organizations::INDEX;
use axum::extract::Form;
use axum::extract::Path;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::http::StatusCode;
use axum::http::Uri;
use axum::response::Response;
use codeyard_api_http::context::PageContext;
use codeyard_app::app::AppState;
use codeyard_db_core::queries::organizations::OrganizationOutcome::Applied;
use codeyard_db_core::queries::organizations::OrganizationOutcome::Denied;
use codeyard_db_core::queries::organizations::OrganizationOutcome::Missing;
use codeyard_db_core::queries::organizations::OrganizationOutcome::Suspended;
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
    // The page names the offer it shows, since one account can hold several from an organization.
    let invitation = match form.get("invitation_id").map(|id| id.parse::<i64>()) {
        Some(Ok(id)) => Some(id),
        Some(Err(_)) => {
            return crate::organizations::organization::respond(
                &ctx,
                None,
                StatusCode::NOT_FOUND,
                Some("This invitation no longer exists.".into()),
                Fields::new(),
            )
            .await
        }
        None => None,
    };
    let pool = &ctx.state.pool;
    let outcome = if accept {
        super::accept_organization_invitation(pool, ctx.database_actor(actor), org, invitation)
            .await
            .map(unit)
    } else {
        super::decline_organization_invitation(pool, ctx.database_actor(actor), org, invitation)
            .await
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
    crate::organizations::organization::respond(
        &ctx,
        None,
        status,
        Some(message.into()),
        Fields::new(),
    )
    .await
}
