//! Private Prometheus gauges. Node ids and fixed states are the only labels.
use axum::response::IntoResponse;
use std::fmt::Write;
use subtle::ConstantTimeEq;

use axum::extract::State;
use axum::http::header;
use axum::http::HeaderMap;
use axum::http::StatusCode;
use axum::response::Response;
use codeyard_app::app::AppState;
use std::sync::Arc;
use std::time::Duration;

async fn allowed(state: &AppState, headers: &HeaderMap) -> bool {
    let Some(bearer) = codeyard_api_http::credentials::bearer(headers) else {
        return false;
    };
    if state.metrics_token_hash.as_ref().is_some_and(|expected| {
        bool::from(
            expected
                .as_bytes()
                .ct_eq(codeyard_support::token_hash::token_hash(bearer).as_bytes()),
        )
    }) {
        return true;
    }
    let Ok(Some(auth)) = codeyard_identity::github_auth::authenticate(state, headers, true).await
    else {
        return false;
    };
    if !auth.user.is_admin || auth.user.is_banned || auth.user.must_change_password {
        return false;
    }
    // Repository-scoped and OAuth credentials do not grant site-wide monitoring access.
    matches!(
        codeyard_identity::github_apps::authority(&state.pool, auth.token.id).await,
        Ok(None)
    ) && matches!(
        codeyard_db_identity::oauth_apps::token(&state.pool, auth.token.id).await,
        Ok(None)
    )
}

pub(super) async fn metrics(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    if !matches!(
        tokio::time::timeout(Duration::from_secs(2), allowed(&state, &headers)).await,
        Ok(true)
    ) {
        return StatusCode::NOT_FOUND.into_response();
    }
    match tokio::time::timeout(Duration::from_secs(2), collect(&state)).await {
        Ok(Ok(body)) => (
            [(
                header::CONTENT_TYPE,
                "text/plain; version=0.0.4; charset=utf-8",
            )],
            body,
        )
            .into_response(),
        _ => (StatusCode::SERVICE_UNAVAILABLE, "metrics unavailable").into_response(),
    }
}

fn gauge(body: &mut String, name: &str) {
    writeln!(body, "# TYPE codeyard_{name} gauge").expect("write to string");
}

async fn collect(state: &AppState) -> anyhow::Result<String> {
    let nodes = codeyard_db_cluster::metrics::storage_nodes(&state.pool).await?;
    let lag = codeyard_db_cluster::metrics::replica_lag(&state.pool).await?;
    let ci = codeyard_db_cluster::metrics::ci_queue(&state.pool).await?;
    let counts = codeyard_db_cluster::metrics::node_counts(&state.pool).await?;
    let mut body = String::new();
    gauge(&mut body, "repository_bytes");
    for node in &nodes {
        writeln!(
            body,
            "codeyard_repository_bytes{{node_id=\"{}\"}} {}",
            node.node_id, node.repository_bytes
        )?;
    }
    gauge(&mut body, "storage_free_bytes");
    for node in nodes {
        let free = node
            .disk_free
            .map_or_else(|| "NaN".to_owned(), |bytes| bytes.to_string());
        writeln!(
            body,
            "codeyard_storage_free_bytes{{node_id=\"{}\"}} {free}",
            node.node_id
        )?;
    }
    for (name, value) in [
        ("replica_lag_versions", lag),
        ("ci_queued_runs", ci.queued),
        ("ci_oldest_queued_seconds", ci.oldest_seconds),
        ("database_pool_size", i64::from(state.pool.size())),
    ] {
        gauge(&mut body, name);
        writeln!(body, "codeyard_{name} {value}")?;
    }
    let drains = codeyard_db_cluster::metrics::drains(&state.pool).await?;
    for name in ["drain_repositories_total", "drain_repositories_completed"] {
        gauge(&mut body, name);
        for node in &drains {
            let value = if name == "drain_repositories_total" {
                node.total
            } else {
                node.completed
            };
            writeln!(
                body,
                "codeyard_{name}{{node_id=\"{}\",state=\"{}\"}} {value}",
                node.node_id, node.state
            )?;
        }
    }
    writeln!(body, "# TYPE codeyard_quorum_failures_total counter")?;
    writeln!(
        body,
        "codeyard_quorum_failures_total {}",
        codeyard_cluster_core::writes::quorum_failures()
    )?;
    // SQLx exposes size and idle connections, but not its acquisition queue.
    gauge(&mut body, "database_pool_waiters");
    writeln!(body, "codeyard_database_pool_waiters NaN")?;
    gauge(&mut body, "nodes");
    for status in ["pending", "active", "revoked"] {
        let count = counts
            .iter()
            .find(|row| row.status == status)
            .map_or(0, |row| row.count);
        writeln!(body, "codeyard_nodes{{state=\"{status}\"}} {count}")?;
    }
    Ok(body)
}
