//! The admin overview page: waiting sign-up requests and the activity charts.
use crate::user::fine_grained::admin::Admin;
use crate::user::responses::require_account;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::http::StatusCode;
use axum::http::Uri;
use axum::response::Response;
use codeyard_api_http::context::PageContext;
use codeyard_app::app::AppState;
use codeyard_webui::components::charts::normalize_days;
use codeyard_webui::components::charts::DailyActivity;
use codeyard_webui::pages::account;
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
    let points = codeyard_db_identity::users::accounts::list_admin_activity_points(pool, days)
        .await
        .map_err(|_| unavailable())?;
    let storage_bytes = points
        .iter()
        .map(|point| point.daily_storage_bytes)
        .collect();
    let activity = points
        .into_iter()
        .map(|point| DailyActivity {
            date: point.day.to_string(),
            commits: point.commits,
            pulls_opened: point.pulls_opened,
            pulls_merged: point.pulls_merged,
            issues_opened: point.issues_opened,
            issues_closed: point.issues_closed,
            runs: point.runs,
        })
        .collect();
    let pending =
        codeyard_db_identity::users::signup_requests::count_signup_requests(pool, "pending")
            .await
            .map_err(|_| unavailable())?;
    let pending_requests = Some(pending as usize);
    Ok(Admin {
        pending_requests,
        activity,
        storage_bytes,
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
            codeyard_webui::app::Page::Identity(account::Page::Admin { model }),
        ),
        Err(response) => response,
    }
}
