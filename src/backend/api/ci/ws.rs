use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};

/// The old CI event socket sent every repository's events to every connection.
/// It stays registered only so old clients get a clear answer: never upgrade a
/// browser connection until a replacement scoped to one user and repository
/// exists. The Actions pages read the stored runs and logs instead.
pub async fn ws_handler() -> Response {
    (
        StatusCode::GONE,
        [(header::CACHE_CONTROL, "no-store")],
        "The unscoped CI event socket has been disabled. Use authorized repository run/log endpoints.\n",
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn legacy_socket_is_gone_without_an_upgrade() {
        let response = ws_handler().await;
        assert_eq!(response.status(), StatusCode::GONE);
        assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
        assert!(!response.headers().contains_key(header::UPGRADE));
    }
}
