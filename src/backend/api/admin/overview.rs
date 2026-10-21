//! The admin overview page: totals, system status, recent signups and the activity chart.
use super::storage::database_name;
use crate::backend::api::http::context::PageContext;
use crate::backend::api::user::responses::require_account;
use crate::backend::app::AppState;
use crate::backend::db;
use crate::webui::pages::{
    account,
    admin::{normalize_days, Account, Activity, Admin, SystemStatus},
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
    let accounts = db::users::accounts::recent_admin_accounts(pool)
        .await
        .map_err(|_| unavailable())?
        .into_iter()
        .map(|(id, username, created)| Account {
            id,
            username,
            created: created.format("%b %e, %Y").to_string(),
        })
        .collect();
    let (users, banned, storage_bytes) = db::users::accounts::admin_totals(pool)
        .await
        .map_err(|_| unavailable())?;
    let (repositories, organizations, queued_runs) = db::users::accounts::admin_site_counts(pool)
        .await
        .map_err(|_| unavailable())?;
    let activity = db::users::accounts::list_admin_activity_points(pool, days)
        .await
        .map_err(|_| unavailable())?
        .into_iter()
        .map(|point| Activity {
            date: point.day.to_string(),
            contributions: point.daily_contributions,
            cumulative_contributions: point.cumulative_contributions,
            bytes: point.daily_storage_bytes,
            cumulative_bytes: point.cumulative_storage_bytes,
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
    let nodes = if ctx.state.cluster.is_some() {
        let nodes = db::cluster::list_nodes(pool)
            .await
            .map_err(|_| unavailable())?;
        Some(nodes.iter().filter(|node| node.status == "active").count())
    } else {
        None
    };
    Ok(Admin {
        accounts,
        users: users as usize,
        banned: banned as usize,
        repositories: repositories as usize,
        organizations: organizations as usize,
        pending_requests,
        storage_bytes,
        activity,
        days,
        status: SystemStatus {
            version: env!("CARGO_PKG_VERSION").to_owned(),
            database: database_name(pool).to_owned(),
            private,
            nodes,
            queued_runs: queued_runs as usize,
        },
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
