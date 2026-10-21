//! The audit log page: recorded administrative changes, newest first, filtered by action and
//! actor.
use crate::backend::api::http::context::PageContext;
use crate::backend::api::user::responses::require_account;
use crate::backend::app::AppState;
use crate::backend::db;
use crate::webui::app::Page;
use crate::webui::pages::{
    account,
    admin::{AdminAudit, AuditEntry, AUDIT_ENTRIES_PER_PAGE},
};
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode, Uri};
use axum::response::Response;
use std::sync::Arc;

/// Details that say nothing are left out of the page.
fn details_text(details: String) -> String {
    if details == "{}" || details == "null" {
        String::new()
    } else {
        details
    }
}

async fn load_audit(
    ctx: &PageContext,
    action: &str,
    actor: &str,
    page: usize,
) -> Result<AdminAudit, Response> {
    require_account(ctx, true)?;
    let pool = &ctx.state.pool;
    let unavailable = || {
        ctx.error(
            StatusCode::SERVICE_UNAVAILABLE,
            "Unable to load the audit log.",
        )
    };
    let total = db::audit::count_audit_entries(pool, action, actor)
        .await
        .map_err(|_| unavailable())? as usize;
    let pages = total.div_ceil(AUDIT_ENTRIES_PER_PAGE).max(1);
    let page = page.clamp(1, pages);
    let entries = db::audit::list_audit_entries(
        pool,
        action,
        actor,
        AUDIT_ENTRIES_PER_PAGE as i64,
        ((page - 1) * AUDIT_ENTRIES_PER_PAGE) as i64,
    )
    .await
    .map_err(|_| unavailable())?
    .into_iter()
    .map(|entry| AuditEntry {
        id: entry.id,
        actor: entry.actor.unwrap_or_default(),
        action: entry.action,
        target: match (entry.target_type, entry.target_id) {
            (Some(kind), Some(id)) => format!("{kind} {id}"),
            (Some(kind), None) => kind,
            _ => String::new(),
        },
        details: details_text(entry.details),
        time: entry.created_at.format("%Y-%m-%d %H:%M UTC").to_string(),
    })
    .collect();
    Ok(AdminAudit {
        entries,
        action: action.to_owned(),
        actor: actor.to_owned(),
        page,
        pages,
        total,
    })
}

pub(crate) async fn audit_get(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    uri: Uri,
) -> Response {
    let ctx = match PageContext::load(state, headers, &uri).await {
        Ok(ctx) => ctx,
        Err(response) => return response,
    };
    let action = ctx.query.get("action").map_or("", |text| text.trim());
    let actor = ctx.query.get("actor").map_or("", |text| text.trim());
    let page = ctx
        .query
        .get("page")
        .and_then(|page| page.parse().ok())
        .unwrap_or(1);
    match load_audit(&ctx, action, actor, page).await {
        Ok(model) => ctx.page_model_status(
            StatusCode::OK,
            "Audit log",
            Page::Identity(account::Page::AdminAudit { model }),
        ),
        Err(response) => response,
    }
}
