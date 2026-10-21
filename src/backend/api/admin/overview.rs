//! The admin overview page: waiting sign-up requests and the activity charts.
use crate::backend::api::http::context::PageContext;
use crate::backend::api::user::responses::require_account;
use crate::backend::app::AppState;
use crate::backend::db;
use crate::webui::pages::{
    account,
    admin::{normalize_days, Activity, Admin},
};
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode, Uri};
use axum::response::Response;
use std::sync::Arc;

async fn load_admin(ctx: &PageContext, days: i32) -> Result<Admin, Response> {
    require_account(ctx, true)?;
    let pool = &ctx.state.pool;
    let unavailable = || {
        ctx.error(
            StatusCode::SERVICE_UNAVAILABLE,
            "Unable to load the administration overview.",
        )
    };
    // Reads persisted facts only: repository sizes are recorded after pushes, never by a GET.
    let activity = db::users::accounts::list_admin_activity_points(pool, days)
        .await
        .map_err(|_| unavailable())?
        .into_iter()
        .map(|point| Activity {
            date: point.day.to_string(),
            commits: point.commits,
            pulls_opened: point.pulls_opened,
            pulls_merged: point.pulls_merged,
            issues_opened: point.issues_opened,
            issues_closed: point.issues_closed,
            runs: point.runs,
            storage_bytes: point.daily_storage_bytes,
        })
        .collect();
    let private = db::node_config::get_node_configuration(pool)
        .await
        .map_err(|_| unavailable())?
        .is_some_and(|config| config.is_private());
    let pending_requests = if private {
        let pending = db::users::signup_requests::count_signup_requests(pool, "pending")
            .await
            .map_err(|_| unavailable())?;
        Some(pending as usize)
    } else {
        None
    };
    Ok(Admin {
        pending_requests,
        activity,
        days,
        version: env!("CARGO_PKG_VERSION").to_owned(),
    })
}

pub(crate) async fn admin_get(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    uri: Uri,
) -> Response {
    let ctx = match PageContext::load(state, headers, &uri).await {
        Ok(ctx) => ctx,
        Err(response) => return response,
    };
    let days = normalize_days(ctx.query.get("days").map(String::as_str));
    match load_admin(&ctx, days).await {
        Ok(model) => ctx.page_model_status(
            StatusCode::OK,
            "Administration",
            crate::webui::app::Page::Identity(account::Page::Admin { model }),
        ),
        Err(response) => response,
    }
}
