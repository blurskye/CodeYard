//! Liveness and role-aware readiness for load balancers.
use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::get;
use axum::Json;
use axum::Router;
use codeyard_app::app::AppState;
use codeyard_model::node_roles::NodeRole;
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
    let free =
        codeyard_app::cluster::disk_free(root).map_err(|_| "storage free space unavailable")?;
    if u64::try_from(free).unwrap_or(0) < minimum {
        return Err("storage free space below minimum");
    }
    Ok(())
}

async fn ready(State(state): State<Arc<AppState>>) -> (StatusCode, Json<serde_json::Value>) {
    let failure = if !matches!(
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
