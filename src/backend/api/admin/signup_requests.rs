use crate::backend::api::http::context::PageContext;
use crate::backend::api::user::responses::require_account;
use crate::backend::{db, identity};
use crate::webui::pages::admin::{Signup, SIGNUP_REQUESTS_PER_PAGE};
use axum::http::StatusCode;
use axum::response::Response;

pub(crate) fn signup_decision(action: &str) -> Option<identity::accounts::SignupDecision> {
    match action.trim().to_ascii_lowercase().as_str() {
        "approve" => Some(identity::accounts::SignupDecision::Approve),
        "reject" => Some(identity::accounts::SignupDecision::Reject),
        _ => None,
    }
}

/// One page of pending requests, the total, and the page number. `per_page` must not be zero.
pub(crate) async fn load_requests(
    ctx: &PageContext,
    page: usize,
) -> Result<(Vec<Signup>, usize, usize), Response> {
    let admin = require_account(ctx, true)?;
    let session =
        crate::backend::api::http::session::extract_session_id(&ctx.headers).unwrap_or_default();
    let authority = (admin.id, session).into();
    let snapshot = db::users::signup_requests::admin_pending_page(
        &ctx.state.pool,
        &authority,
        SIGNUP_REQUESTS_PER_PAGE as i64,
        page,
    )
    .await
    .map_err(|error| {
        if matches!(
            error.downcast_ref::<db::users::signup_requests::ReviewError>(),
            Some(db::users::signup_requests::ReviewError::Denied)
        ) {
            ctx.error(StatusCode::FORBIDDEN, "Administrator access is required.")
        } else {
            ctx.error(
                StatusCode::SERVICE_UNAVAILABLE,
                "Unable to load signup requests.",
            )
        }
    })?;
    let (requests, total, page) = (snapshot.requests, snapshot.total as usize, snapshot.page);
    Ok((
        requests
            .into_iter()
            .map(|request| Signup {
                id: request.id,
                username: request.username,
                email: request.email.unwrap_or_default(),
                created: request.created_at.format("%b %e, %Y %H:%M UTC").to_string(),
            })
            .collect(),
        total,
        page,
    ))
}
