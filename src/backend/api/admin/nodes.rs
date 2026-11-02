//! Admin nodes page: the cluster's members, with approve and revoke.
use crate::backend::api::http::context::PageContext;
use crate::backend::api::user::responses::{mutation_guard, require_account};
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

async fn show(State(state): State<Arc<AppState>>, headers: HeaderMap, uri: Uri) -> Response {
    let ctx = match PageContext::load(state, headers, &uri).await {
        Ok(ctx) => ctx,
        Err(response) => return response,
    };
    if let Err(response) = require_account(&ctx, true) {
        return response;
    }
    render(&ctx, StatusCode::OK, None).await
}

async fn render(ctx: &PageContext, status: StatusCode, message: Option<String>) -> Response {
    let Some(administrator) = ctx.administrator() else {
        return ctx.error(StatusCode::FORBIDDEN, "Administrator access is required.");
    };
    let session =
        crate::backend::api::http::session::extract_session_id(&ctx.headers).unwrap_or_default();
    let authority = (administrator.id, session).into();
    let after = ctx.query.get("after").map(String::as_str).unwrap_or("");
    let mut nodes = match db::cluster::admin_node_page(&ctx.state.pool, &authority, after).await {
        Ok(nodes) => nodes,
        Err(error) if error.is::<db::users::accounts::AccountRestricted>() => {
            return ctx.error(StatusCode::FORBIDDEN, "Administrator access is required.");
        }
        Err(_) => {
            return ctx.error(
                StatusCode::SERVICE_UNAVAILABLE,
                "The node list is unavailable.",
            )
        }
    };
    let more = nodes.len() > 100;
    nodes.truncate(100);
    let next_after = more
        .then(|| nodes.last().map(|node| node.name.clone()))
        .flatten();
    let Ok(cluster_enabled) = db::cluster::cluster_enabled(&ctx.state.pool).await else {
        return ctx.error(
            StatusCode::SERVICE_UNAVAILABLE,
            "The node list is unavailable.",
        );
    };
    let model = view::Page {
        csrf_token: ctx.csrf_token.clone(),
        cluster_enabled,
        next_after,
        has_previous: !after.is_empty(),
        nodes: nodes
            .into_iter()
            .map(|node| view::Node {
                offline: node.status == "active" && !node.is_online(),
                roles: crate::backend::node::describe_roles(&node.roles),
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
    let mut ctx = match PageContext::load(state, headers, &uri).await {
        Ok(ctx) => ctx,
        Err(response) => return response,
    };
    ctx.return_to = Some("/admin/nodes".into());
    if let Err(response) = mutation_guard(&ctx, &form, true) {
        return response;
    }
    let actor = ctx
        .user
        .as_ref()
        .expect("mutation guard checked account")
        .id;
    let session =
        crate::backend::api::http::session::extract_session_id(&ctx.headers).unwrap_or_default();
    let authority = db::cluster::Authority::Administrator {
        user_id: actor,
        session: &session,
    };
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
