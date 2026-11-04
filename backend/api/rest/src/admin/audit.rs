//! The audit log page: recorded administrative changes, newest first, filtered by the kind of
//! change and the actor.
use crate::user::responses::require_account;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::http::StatusCode;
use axum::http::Uri;
use axum::response::Response;
use codeyard_api_http::context::PageContext;
use codeyard_app::app::AppState;
use codeyard_webui::app::Page;
use codeyard_webui::pages::account;
use codeyard_webui::pages::admin::AdminAudit;
use codeyard_webui::pages::admin::AuditEntry;
use codeyard_webui::pages::admin::AUDIT_ENTRIES_PER_PAGE;
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
    let user = require_account(ctx, true)?;
    let session = codeyard_api_http::session::extract_session_id(&ctx.headers)
        .ok_or_else(|| ctx.error(StatusCode::FORBIDDEN, "Administrator session required."))?;
    let pool = &ctx.state.pool;
    let unavailable = || {
        ctx.error(
            StatusCode::SERVICE_UNAVAILABLE,
            "Unable to load the audit log.",
        )
    };
    let (total, entries) = codeyard_db_identity::audit::page(
        pool,
        (user.id, session).into(),
        action,
        actor,
        i64::try_from(page).unwrap_or(i64::MAX),
        AUDIT_ENTRIES_PER_PAGE as i64,
    )
    .await
    .map_err(|_| unavailable())?
    .ok_or_else(|| ctx.error(StatusCode::FORBIDDEN, "Administrator session required."))?;
    let total = total as usize;
    let pages = total.div_ceil(AUDIT_ENTRIES_PER_PAGE).max(1);
    let page = page.clamp(1, pages);
    let entries = entries
        .into_iter()
        .map(|entry| AuditEntry {
            id: entry.id,
            actor: entry.actor.unwrap_or_default(),
            action: entry.action,
            // A user target that no longer exists has no name, and the page says so.
            target: entry.target_name.unwrap_or_default(),
            org: entry.org_name.unwrap_or_default(),
            details: details_text(entry.details),
            time: entry.created_at.to_rfc3339(),
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
