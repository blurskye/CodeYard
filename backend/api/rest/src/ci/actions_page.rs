use axum::extract::OriginalUri;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::response::Response;
use codeyard_app::app::AppState;
use std::sync::Arc;

pub(crate) async fn global_actions(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    OriginalUri(uri): OriginalUri,
) -> Response {
    actions(state, headers, uri, None).await
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn selected_run_cannot_cross_repository_scope() {
        assert!(run_matches_repository(9, Some(9)));
        assert!(!run_matches_repository(9, Some(10)));
        assert!(run_matches_repository(9, None));
    }
}

use codeyard_api_http::shared::ci::actions_page::actions;

use codeyard_api_http::shared::ci::actions_page::run_matches_repository;
