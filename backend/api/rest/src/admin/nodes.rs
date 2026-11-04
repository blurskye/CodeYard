//! Admin nodes page: the cluster's members, with approve and revoke.
use crate::user::responses::mutation_guard;
use crate::user::responses::require_account;
use axum::extract::Path;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::http::StatusCode;
use axum::http::Uri;
use axum::response::Response;
use axum::routing::get;
use axum::routing::post;
use axum::Form;
use axum::Router;
use codeyard_api_http::context::PageContext;
use codeyard_app::app::AppState;
use codeyard_webui::app::Page;
use codeyard_webui::components::format_bytes;
use codeyard_webui::pages::admin::nodes as view;
use std::collections::HashMap;
use std::sync::Arc;

pub fn router() -> Router<Arc<AppState>> {
    Router::new()
        .route("/admin/nodes", get(show))
        .route("/admin/nodes/:name/approve", post(approve))
        .route("/admin/nodes/:name/revoke", post(revoke))
        .route("/admin/nodes/:name/drain", post(drain))
        .route("/admin/nodes/:name/cancel-drain", post(cancel_drain))
        .route(
            "/api/admin/nodes/:name/drain",
            post(api_drain).delete(api_cancel_drain),
        )
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
    let session = codeyard_api_http::session::extract_session_id(&ctx.headers).unwrap_or_default();
    let authority = (administrator.id, session).into();
    let after = ctx.query.get("after").map(String::as_str).unwrap_or("");
    let mut nodes =
        match codeyard_db_cluster::cluster::admin_node_page(&ctx.state.pool, &authority, after)
            .await
        {
            Ok(nodes) => nodes,
            Err(error) if error.is::<codeyard_support::credentials::AccountRestricted>() => {
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
    let Ok(cluster_enabled) = codeyard_db_cluster::cluster::cluster_enabled(&ctx.state.pool).await
    else {
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
                roles: codeyard_model::node_roles::describe_roles(&node.roles),
                version: node.version.unwrap_or_default(),
                last_seen: node
                    .last_seen
                    .map(|seen| seen.format("%Y-%m-%d %H:%M:%S UTC").to_string())
                    .unwrap_or_else(|| "never".into()),
                disk_free: node.disk_free.map(format_bytes).unwrap_or_default(),
                name: node.name,
                status: node.status,
                state: node.state,
                drain_total: node.drain_total,
                drain_completed: node.drain_completed,
                drain_error: node.drain_error,
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
    let session = codeyard_api_http::session::extract_session_id(&ctx.headers).unwrap_or_default();
    let authority = codeyard_db_cluster::cluster::Authority::Administrator {
        user_id: actor,
        session: &session,
    };
    let pool = &ctx.state.pool;
    let result = match change {
        Change::Approve => codeyard_cluster_core::pairing::approve(pool, authority, &name).await,
        Change::Revoke => codeyard_cluster_core::pairing::revoke(pool, authority, &name).await,
    };
    match result {
        Ok(codeyard_db_core::models::NodeStatusChange::Changed { .. }) => {
            ctx.redirect("/admin/nodes")
        }
        // The account stopped being an administrator after the page check above.
        Ok(codeyard_db_core::models::NodeStatusChange::Denied) => {
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

async fn drain(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    uri: Uri,
    Path(name): Path<String>,
    Form(form): Form<HashMap<String, String>>,
) -> Response {
    change_drain(state, headers, uri, name, form, false).await
}

async fn cancel_drain(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    uri: Uri,
    Path(name): Path<String>,
    Form(form): Form<HashMap<String, String>>,
) -> Response {
    change_drain(state, headers, uri, name, form, true).await
}

fn drain_error(error: &anyhow::Error) -> (StatusCode, String, Option<i64>) {
    use codeyard_db_cluster::drain::Refusal;
    match error.downcast_ref::<Refusal>() {
        Some(Refusal::Denied) => (StatusCode::FORBIDDEN, error.to_string(), None),
        Some(Refusal::Invalid) => (StatusCode::UNPROCESSABLE_ENTITY, error.to_string(), None),
        Some(Refusal::Missing) => (StatusCode::NOT_FOUND, error.to_string(), None),
        Some(Refusal::Offline) => (StatusCode::CONFLICT, error.to_string(), None),
        Some(Refusal::Capacity { missing_bytes }) => (
            StatusCode::CONFLICT,
            error.to_string(),
            Some(*missing_bytes),
        ),
        None => (
            StatusCode::SERVICE_UNAVAILABLE,
            "Node drain is unavailable. Try again.".into(),
            None,
        ),
    }
}

async fn change_drain(
    state: Arc<AppState>,
    headers: HeaderMap,
    uri: Uri,
    name: String,
    form: HashMap<String, String>,
    cancel: bool,
) -> Response {
    let ctx = match PageContext::load(state, headers, &uri).await {
        Ok(ctx) => ctx,
        Err(response) => return response,
    };
    if let Err(response) = mutation_guard(&ctx, &form, true) {
        return response;
    }
    let user = ctx.user.as_ref().expect("mutation guard checked account");
    let session = codeyard_api_http::session::extract_session_id(&ctx.headers).unwrap_or_default();
    let actor = (user.id, session).into();
    match codeyard_cluster::drain::change(&ctx.state.pool, Some(&actor), &name, cancel).await {
        Ok(_) => ctx.redirect("/admin/nodes"),
        Err(error) => {
            let (status, message, _) = drain_error(&error);
            render(&ctx, status, Some(message)).await
        }
    }
}

async fn api_drain(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(name): Path<String>,
) -> Response {
    api_change_drain(state, headers, name, false).await
}

async fn api_cancel_drain(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(name): Path<String>,
) -> Response {
    api_change_drain(state, headers, name, true).await
}

async fn api_change_drain(
    state: Arc<AppState>,
    headers: HeaderMap,
    name: String,
    cancel: bool,
) -> Response {
    use axum::response::IntoResponse;
    let auth =
        match codeyard_identity::github_auth::authenticate(state.as_ref(), &headers, true).await {
            Ok(Some(auth)) => auth,
            _ => {
                return (
                    StatusCode::UNAUTHORIZED,
                    axum::Json(serde_json::json!({"message":"Requires authentication"})),
                )
                    .into_response()
            }
        };
    if !auth.user.is_admin
        || !matches!(
            codeyard_identity::github_apps::authority(&state.pool, auth.token.id).await,
            Ok(None)
        )
        || !matches!(
            codeyard_db_identity::oauth_apps::token(&state.pool, auth.token.id).await,
            Ok(None)
        )
    {
        return (
            StatusCode::FORBIDDEN,
            axum::Json(serde_json::json!({"message":"Administrator access is required."})),
        )
            .into_response();
    }
    match codeyard_cluster::drain::change(&state.pool, Some(&auth.database_actor()), &name, cancel)
        .await
    {
        Ok(node) => (StatusCode::ACCEPTED, axum::Json(node)).into_response(),
        Err(error) => {
            let (status, message, missing) = drain_error(&error);
            let mut body = serde_json::json!({"message":message});
            if let Some(bytes) = missing {
                body["missing_bytes"] = bytes.into();
            }
            (status, axum::Json(body)).into_response()
        }
    }
}
