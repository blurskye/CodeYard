pub mod assignees;
pub mod collection;
pub mod comments;
pub mod issue;
pub mod labels;

pub fn json_router() -> axum::Router<std::sync::Arc<crate::backend::app::AppState>> {
    use axum::{
        routing::{delete, get, post},
        Router,
    };
    Router::new()
        .route(
            "/api/repos/:owner/:repo/issues",
            get(collection::list_issues).post(collection::create_issue),
        )
        .route(
            "/api/repos/:owner/:repo/issues/:number",
            get(issue::get_issue).put(issue::update_issue),
        )
        .route(
            "/api/repos/:owner/:repo/issues/:number/comments",
            get(comments::list_issue_comments).post(comments::create_issue_comment),
        )
        .route(
            "/api/repos/:owner/:repo/issues/:number/labels",
            post(labels::add_issue_labels),
        )
        .route(
            "/api/repos/:owner/:repo/issues/:number/labels/:label_id",
            delete(labels::remove_issue_label),
        )
}

pub fn page_router() -> axum::Router<std::sync::Arc<crate::backend::app::AppState>> {
    use axum::{
        routing::{get, post},
        Router,
    };
    Router::new()
        .route("/:owner/:repo/issues", get(collection::native_index))
        .route(
            "/:owner/:repo/issues/new",
            get(collection::native_new_page).post(collection::native_create),
        )
        .route("/:owner/:repo/issues/:number", get(issue::native_detail))
        .route(
            "/:owner/:repo/issues/:number/edit",
            post(issue::native_edit),
        )
        .route(
            "/:owner/:repo/issues/:number/state",
            post(issue::native_change_state),
        )
        .route(
            "/:owner/:repo/issues/:number/comments",
            post(comments::native_comment),
        )
        .route(
            "/:owner/:repo/issues/:number/labels",
            post(labels::native_add_label),
        )
        .route(
            "/:owner/:repo/issues/:number/labels/:label_id/remove",
            post(labels::native_remove_label),
        )
        .route(
            "/:owner/:repo/issues/:number/assignees",
            post(assignees::native_assign),
        )
        .route(
            "/:owner/:repo/issues/:number/assignees/:username/remove",
            post(assignees::native_unassign),
        )
}

pub(crate) fn operation_error(
    error: crate::backend::issues::OperationError,
) -> (
    axum::http::StatusCode,
    axum::Json<crate::backend::api::error::ApiError>,
) {
    use crate::backend::api::error::ApiError;
    use crate::backend::issues::OperationError;
    use axum::{http::StatusCode, Json};
    let (status, code, message) = match error {
        OperationError::Invalid { code, message } => (StatusCode::BAD_REQUEST, code, message),
        OperationError::Missing { code, message } => (StatusCode::NOT_FOUND, code, message),
        OperationError::Denied { code, message } => (StatusCode::FORBIDDEN, code, message),
        OperationError::Unauthenticated { code, message } => {
            (StatusCode::UNAUTHORIZED, code, message)
        }
        OperationError::Conflict { code, message } => (StatusCode::CONFLICT, code, message),
        OperationError::Unavailable { code, message } => {
            (StatusCode::SERVICE_UNAVAILABLE, code, message)
        }
    };
    (status, Json(ApiError::new(code, &message)))
}
