pub mod assignees;
pub mod attributes;
pub mod collection;
pub mod comments;
pub mod issue;
pub mod labels;
pub mod locking;
pub(crate) mod management;
pub mod pins;
pub mod reactions;
pub mod relationships;
pub mod suggestions;
pub mod tasks;

pub fn json_router() -> axum::Router<std::sync::Arc<codeyard_app::app::AppState>> {
    use axum::{
        routing::{delete, get, post},
        Router,
    };
    Router::new()
        .route(
            "/api/repos/:owner/:repo/issues",
            get(crate::issues::collection::list_issues)
                .post(crate::issues::collection::create_issue),
        )
        .route(
            "/api/repos/:owner/:repo/issues/:number",
            get(crate::issues::issue::get_issue).put(crate::issues::issue::update_issue),
        )
        .route(
            "/api/repos/:owner/:repo/issues/:number/comments",
            get(crate::issues::comments::list_issue_comments)
                .post(crate::issues::comments::create_issue_comment),
        )
        .route(
            "/api/repos/:owner/:repo/issues/:number/suggestions",
            post(crate::issues::suggestions::agent_propose),
        )
        .route(
            "/api/repos/:owner/:repo/issues/:number/labels",
            post(crate::issues::labels::add_issue_labels),
        )
        .route(
            "/api/repos/:owner/:repo/issues/:number/labels/:label_id",
            delete(crate::issues::labels::remove_issue_label),
        )
}

pub fn page_router() -> axum::Router<std::sync::Arc<codeyard_app::app::AppState>> {
    use axum::{
        routing::{get, post},
        Router,
    };
    Router::new()
        .route(
            "/:owner/:repo/issues",
            get(crate::issues::collection::native_index),
        )
        .route(
            "/:owner/:repo/issues/new",
            get(crate::issues::collection::native_new_page)
                .post(crate::issues::collection::native_create),
        )
        .route(
            "/:owner/:repo/issues/:number",
            get(crate::issues::issue::native_detail),
        )
        .route(
            "/:owner/:repo/issues/:number/manage",
            post(crate::issues::management::native_change),
        )
        .route(
            "/:owner/:repo/issues/:number/edit",
            post(crate::issues::issue::native_edit),
        )
        .route(
            "/:owner/:repo/issues/:number/state",
            post(crate::issues::issue::native_change_state),
        )
        .route(
            "/:owner/:repo/issues/:number/comments",
            post(crate::issues::comments::native_comment),
        )
        .route(
            "/:owner/:repo/issues/:number/comments/:comment_id/edit",
            post(crate::issues::comments::native_edit_comment),
        )
        .route(
            "/:owner/:repo/issues/:number/comments/:comment_id/delete",
            post(crate::issues::comments::native_delete_comment),
        )
        .route(
            "/:owner/:repo/issues/:number/tasks",
            post(crate::issues::tasks::native_toggle_task),
        )
        .route(
            "/:owner/:repo/issues/:number/lock",
            post(crate::issues::locking::native_lock),
        )
        .route(
            "/:owner/:repo/issues/:number/relationships",
            post(crate::issues::relationships::native_change),
        )
        .route(
            "/:owner/:repo/issues/:number/attributes",
            post(crate::issues::attributes::native_change),
        )
        .route(
            "/:owner/:repo/issues/:number/suggestions",
            post(crate::issues::suggestions::native_change),
        )
        .route(
            "/:owner/:repo/issues/:number/comments/:comment/pin",
            post(crate::issues::pins::native_pin),
        )
        .route(
            "/:owner/:repo/issues/:number/react",
            post(crate::issues::reactions::native_react),
        )
        .route(
            "/:owner/:repo/issues/:number/labels",
            post(crate::issues::labels::native_add_label),
        )
        .route(
            "/:owner/:repo/issues/:number/labels/:label_id/remove",
            post(crate::issues::labels::native_remove_label),
        )
        .route(
            "/:owner/:repo/issues/:number/assignees",
            post(crate::issues::assignees::native_assign),
        )
        .route(
            "/:owner/:repo/issues/:number/assignees/:username/remove",
            post(crate::issues::assignees::native_unassign),
        )
}

pub(crate) fn operation_error(
    error: codeyard_repos_pulls::issues::OperationError,
) -> (
    axum::http::StatusCode,
    axum::Json<codeyard_api_http::error::ApiError>,
) {
    use axum::{http::StatusCode, Json};
    use codeyard_api_http::error::ApiError;
    use codeyard_repos_pulls::issues::OperationError;
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
