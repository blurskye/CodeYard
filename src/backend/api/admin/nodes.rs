//! Admin nodes page: the cluster's members, with approve and revoke.
use crate::backend::api::http::context::PageContext;
use crate::backend::{app::AppState, cluster, db};
use crate::webui::{app::Page, components::format_bytes, pages::admin::nodes as view};
use axum::{
    extract::{Path, State},
    http::{HeaderMap, StatusCode, Uri},
    response::Response,
    routing::{get, post},
    Form, Router,
};
use std::{collections::HashMap, sync::Arc};

pub fn router() -> Router<Arc<AppState>> {
    Router::new()
        .route("/admin/nodes", get(show))
        .route("/admin/nodes/:name/approve", post(approve))
        .route("/admin/nodes/:name/revoke", post(revoke))
}

/// The signed-in account's id, if it may change cluster membership.
fn administrator(ctx: &PageContext) -> Option<i64> {
    ctx.user
        .as_ref()
        .filter(|user| user.is_admin && !user.is_banned && !user.must_change_password)
        .map(|user| user.id)
}

async fn show(State(state): State<Arc<AppState>>, headers: HeaderMap, uri: Uri) -> Response {
    let ctx = match PageContext::load(state, headers, &uri).await {
        Ok(ctx) => ctx,
        Err(response) => return response,
    };
    if administrator(&ctx).is_none() {
        return ctx.error(StatusCode::FORBIDDEN, "Administrator access is required.");
    }
    render(&ctx, StatusCode::OK, None).await
}

async fn render(ctx: &PageContext, status: StatusCode, message: Option<String>) -> Response {
    let nodes = match db::cluster::list_nodes(&ctx.state.pool).await {
        Ok(nodes) => nodes,
        Err(_) => {
            return ctx.error(
                StatusCode::SERVICE_UNAVAILABLE,
                "The node list is unavailable.",
            )
        }
    };
    let model = view::Page {
        csrf_token: ctx.csrf_token.clone(),
        nodes: nodes
            .into_iter()
            .map(|node| view::Node {
                offline: node.status == "active" && !node.is_online(),
                roles: node.roles.join(", "),
                version: node.version.unwrap_or_default(),
                last_seen: node
                    .last_seen
                    .map(|seen| seen.format("%Y-%m-%d %H:%M:%S UTC").to_string())
                    .unwrap_or_else(|| "never".into()),
                disk_free: node.disk_free.map(format_bytes).unwrap_or_default(),
                name: node.name,
                status: node.status,
            })
            .collect(),
        message,
    };
    ctx.page_model_status(status, "Nodes", Page::Nodes(model))
}

enum Change {
    Approve,
    Revoke,
}

async fn approve(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    uri: Uri,
    Path(name): Path<String>,
    Form(form): Form<HashMap<String, String>>,
) -> Response {
    change(state, headers, uri, name, form, Change::Approve).await
}

async fn revoke(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    uri: Uri,
    Path(name): Path<String>,
    Form(form): Form<HashMap<String, String>>,
) -> Response {
    change(state, headers, uri, name, form, Change::Revoke).await
}

async fn change(
    state: Arc<AppState>,
    headers: HeaderMap,
    uri: Uri,
    name: String,
    form: HashMap<String, String>,
    change: Change,
) -> Response {
    let ctx = match PageContext::load(state, headers, &uri).await {
        Ok(ctx) => ctx,
        Err(response) => return response,
    };
    if let Err(response) = ctx.check_csrf(&form) {
        return response;
    }
    let Some(actor) = administrator(&ctx) else {
        return ctx.error(StatusCode::FORBIDDEN, "Administrator access is required.");
    };
    let authority = db::cluster::Authority::Administrator(actor);
    let pool = &ctx.state.pool;
    let result = match change {
        Change::Approve => cluster::approve(pool, authority, &name).await,
        Change::Revoke => cluster::revoke(pool, authority, &name).await,
    };
    match result {
        Ok(db::models::NodeStatusChange::Changed { .. }) => ctx.redirect("/admin/nodes"),
        // The account stopped being an administrator after the page check above.
        Ok(db::models::NodeStatusChange::Denied) => {
            ctx.error(StatusCode::FORBIDDEN, "Administrator access is required.")
        }
        _ => {
            render(
                &ctx,
                StatusCode::SERVICE_UNAVAILABLE,
                Some("Node membership could not be updated. Refresh before retrying.".to_owned()),
            )
            .await
        }
    }
}
