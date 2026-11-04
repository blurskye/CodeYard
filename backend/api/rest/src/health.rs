//! Liveness and role-aware readiness for load balancers.
use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::get;
use axum::Json;
use axum::Router;
use codeyard_app::app::AppState;
use codeyard_model::node_roles::NodeRole;
use std::io::Write;
use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

pub fn router() -> Router<Arc<AppState>> {
    Router::new()
        .route(
            "/health",
            get(|| async { Json(serde_json::json!({ "status": "ok" })) }),
        )
        .route("/ready", get(ready))
        .route("/metrics", get(crate::metrics::metrics))
}

/// Storage-only nodes expose probes and metrics, never the web application.
pub async fn run_server(
    addr: SocketAddr,
    state: AppState,
    mut stop: tokio::sync::watch::Receiver<bool>,
) -> anyhow::Result<()> {
    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, router().with_state(Arc::new(state)))
        .with_graceful_shutdown(async move {
            let _ = stop.wait_for(|stopped| *stopped).await;
        })
        .await?;
    Ok(())
}

fn storage_check(root: &Path, minimum: u64) -> Result<(), &'static str> {
    let mut probe =
        tempfile::NamedTempFile::new_in(root).map_err(|_| "repository storage not writable")?;
    probe
        .write_all(b"ready")
        .map_err(|_| "repository storage not writable")?;
    probe
        .close()
        .map_err(|_| "repository storage not writable")?;
    let free = codeyard_cluster::disk_free(root).map_err(|_| "storage free space unavailable")?;
    if u64::try_from(free).unwrap_or(0) < minimum {
        return Err("storage free space below minimum");
    }
    Ok(())
}

pub(crate) async fn accepting(state: &AppState) -> bool {
    let Some(context) = &state.cluster else {
        return true;
    };
    matches!(tokio::time::timeout(Duration::from_secs(2), codeyard_db_cluster::cluster::get_node_by_id(&state.pool, context.node_id)).await,
        Ok(Ok(Some(node))) if node.state == "active" && node.status == "active")
}

pub(crate) async fn admission(
    State(state): State<Arc<AppState>>,
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    let path = request.uri().path();
    if path == "/health"
        || path == "/ready"
        || path == "/metrics"
        || path == "/admin/nodes"
        || path.starts_with("/admin/nodes/")
        || path.starts_with("/api/admin/nodes/")
        || accepting(&state).await
    {
        next.run(request).await
    } else {
        (
            StatusCode::SERVICE_UNAVAILABLE,
            [(axum::http::header::RETRY_AFTER, "15")],
            Json(serde_json::json!({"message":"Node is draining; retry on another node."})),
        )
            .into_response()
    }
}

async fn ready(State(state): State<Arc<AppState>>) -> (StatusCode, Json<serde_json::Value>) {
    let failure = if !accepting(&state).await {
        Some("node is draining or unavailable")
    } else if !matches!(
        tokio::time::timeout(Duration::from_secs(2), state.pool.ping()).await,
        Ok(Ok(()))
    ) {
        Some("database unavailable")
    } else if state.roles.contains(NodeRole::Storage) {
        // A stalled filesystem probe retains the permit, bounding blocking work across requests.
        match state.storage_readiness.clone().try_acquire_owned() {
            Ok(permit) => {
                let root = state.repos_path.clone();
                let minimum = state.storage_min_free_bytes;
                let task = tokio_util::task::AbortOnDropHandle::new(tokio::task::spawn_blocking(
                    move || {
                        let _permit = permit;
                        storage_check(&root, minimum)
                    },
                ));
                match tokio::time::timeout(Duration::from_secs(2), task).await {
                    Ok(Ok(Ok(()))) => None,
                    Ok(Ok(Err(check))) => Some(check),
                    _ => Some("repository storage unavailable"),
                }
            }
            Err(_) => Some("repository storage check busy"),
        }
    } else {
        None
    };
    match failure {
        None => (
            StatusCode::OK,
            Json(serde_json::json!({ "status": "ready" })),
        ),
        Some(check) => (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({ "status": check })),
        ),
    }
}
