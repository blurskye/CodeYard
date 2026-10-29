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

fn request_offset(page: usize) -> i64 {
    i64::try_from(
        page.saturating_sub(1)
            .saturating_mul(SIGNUP_REQUESTS_PER_PAGE),
    )
    .unwrap_or(i64::MAX)
}

/// One page of pending requests, the total, and the page number. `per_page` must not be zero.
pub(crate) async fn load_requests(
    ctx: &PageContext,
    page: usize,
) -> Result<(Vec<Signup>, usize, usize), Response> {
    // Existing requests remain reviewable after registration is opened.
    require_account(ctx, true)?;
    let total = db::users::signup_requests::count_signup_requests(&ctx.state.pool, "pending")
        .await
        .map_err(|_| {
            ctx.error(
                StatusCode::SERVICE_UNAVAILABLE,
                "Unable to count signup requests.",
            )
        })? as usize;
    let page = page.clamp(1, total.div_ceil(SIGNUP_REQUESTS_PER_PAGE).max(1));
    let requests = db::users::signup_requests::list_signup_requests(
        &ctx.state.pool,
        "pending",
        SIGNUP_REQUESTS_PER_PAGE as i64,
        request_offset(page),
        0,
    )
    .await
    .map_err(|_| {
        ctx.error(
            StatusCode::SERVICE_UNAVAILABLE,
            "Unable to load signup requests.",
        )
    })?;
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
