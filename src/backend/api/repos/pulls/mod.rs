pub mod collection;
pub mod comments;
pub mod diff;
pub mod merge;
pub mod pull;
pub mod ready;
pub mod requested_reviewers;
pub mod review_forms;
pub mod reviews;
pub mod stack;
pub mod workflows;

pub fn json_router() -> axum::Router<std::sync::Arc<crate::backend::app::AppState>> {
    use axum::{
        routing::{delete, get, post},
        Router,
    };
    Router::new()
        .route(
            "/api/repos/:owner/:repo/pulls",
            get(collection::list_pull_requests).post(collection::create_pull_request),
        )
        .route(
            "/api/repos/:owner/:repo/pulls/:number",
            get(pull::get_pull_request).put(pull::update_pull_request),
        )
        .route(
            "/api/repos/:owner/:repo/pulls/:number/workflows",
            post(workflows::run_pull_request_workflows),
        )
        .route(
            "/api/repos/:owner/:repo/pulls/:number/ready",
            post(ready::mark_pull_request_ready),
        )
        .route(
            "/api/repos/:owner/:repo/pulls/:number/merge",
            post(merge::merge_pull_request),
        )
        .route(
            "/api/repos/:owner/:repo/pulls/:number/comments",
            get(comments::list_pull_request_comments).post(comments::create_pull_request_comment),
        )
        .route(
            "/api/repos/:owner/:repo/pulls/:number/comments/:comment_id/resolve",
            post(comments::resolve_pull_request_comment),
        )
        .route(
            "/api/repos/:owner/:repo/pulls/:number/comments/:comment_id/unresolve",
            post(comments::unresolve_pull_request_comment),
        )
        .route(
            "/api/repos/:owner/:repo/pulls/:number/reviews",
            get(reviews::list_pull_request_reviews).post(reviews::create_pull_request_review),
        )
        .route(
            "/api/repos/:owner/:repo/pulls/:number/reviews/:review_id/dismiss",
            post(reviews::dismiss_pull_request_review),
        )
        .route(
            "/api/repos/:owner/:repo/pulls/:number/requested_reviewers",
            get(requested_reviewers::list_requested_reviewers)
                .post(requested_reviewers::request_reviewer),
        )
        .route(
            "/api/repos/:owner/:repo/pulls/:number/requested_reviewers/:username",
            delete(requested_reviewers::remove_requested_reviewer),
        )
        .route(
            "/api/repos/:owner/:repo/pulls/:number/diff",
            get(diff::get_pull_request_diff),
        )
        .route(
            "/api/repos/:owner/:repo/pulls/:number/stack",
            post(stack::post_stack).delete(stack::delete_stack),
        )
        .route(
            "/api/repos/:owner/:repo/pulls/:number/stack/rebase",
            post(stack::post_rebase),
        )
}

pub fn page_router() -> axum::Router<std::sync::Arc<crate::backend::app::AppState>> {
    use crate::backend::api::repos::issues;
    use axum::{
        routing::{get, post},
        Router,
    };
    Router::new()
        .route("/:owner/:repo/pulls", get(issues::collection::native_index))
        .route(
            "/:owner/:repo/pulls/new",
            get(issues::collection::native_new_page).post(issues::collection::native_create),
        )
        .route(
            "/:owner/:repo/pulls/:number",
            get(issues::issue::native_detail),
        )
        .route(
            "/:owner/:repo/pulls/:number/edit",
            post(issues::issue::native_edit),
        )
        .route(
            "/:owner/:repo/pulls/:number/state",
            post(issues::issue::native_change_state),
        )
        .route(
            "/:owner/:repo/pulls/:number/comments",
            post(review_forms::native_pull_comment),
        )
        .route(
            "/:owner/:repo/pulls/:number/comments/:comment_id/resolve",
            post(review_forms::native_resolve_comment),
        )
        .route(
            "/:owner/:repo/pulls/:number/comments/:comment_id/unresolve",
            post(review_forms::native_unresolve_comment),
        )
        .route(
            "/:owner/:repo/pulls/:number/reviews",
            post(review_forms::native_review),
        )
        .route(
            "/:owner/:repo/pulls/:number/reviews/:review_id/dismiss",
            post(review_forms::native_dismiss_review),
        )
        .route(
            "/:owner/:repo/pulls/:number/requested_reviewers",
            post(review_forms::native_request_reviewer),
        )
        .route(
            "/:owner/:repo/pulls/:number/requested_reviewers/:username/remove",
            post(review_forms::native_remove_requested_reviewer),
        )
        .route("/:owner/:repo/pulls/:number/diff", get(diff::native_diff))
        .route(
            "/:owner/:repo/pulls/:number/viewed",
            post(review_forms::native_set_viewed),
        )
        .route(
            "/:owner/:repo/pulls/:number/ready",
            post(ready::native_ready),
        )
        .route(
            "/:owner/:repo/pulls/:number/workflows",
            post(workflows::native_run_workflows),
        )
        .route(
            "/:owner/:repo/pulls/:number/merge",
            post(merge::native_merge),
        )
        .route(
            "/:owner/:repo/pulls/:number/stack",
            post(stack::native_join),
        )
        .route(
            "/:owner/:repo/pulls/:number/unstack",
            post(stack::native_unstack),
        )
        .route(
            "/:owner/:repo/pulls/:number/stack/rebase",
            post(stack::native_rebase),
        )
        .route(
            "/:owner/:repo/pulls/:number/labels",
            post(issues::labels::native_add_label),
        )
        .route(
            "/:owner/:repo/pulls/:number/labels/:label_id/remove",
            post(issues::labels::native_remove_label),
        )
        .route(
            "/:owner/:repo/pulls/:number/assignees",
            post(issues::assignees::native_assign),
        )
        .route(
            "/:owner/:repo/pulls/:number/assignees/:username/remove",
            post(issues::assignees::native_unassign),
        )
}

pub(crate) fn operation_error(
    error: crate::backend::repositories::pull_requests::OperationError,
) -> (
    axum::http::StatusCode,
    axum::Json<crate::backend::api::error::ApiError>,
) {
    use crate::backend::api::error::ApiError;
    use crate::backend::repositories::pull_requests::OperationError;
    use axum::{http::StatusCode, Json};
    let (status, code, message) = match error {
        OperationError::Invalid { code, message } => (StatusCode::BAD_REQUEST, code, message),
        OperationError::Missing { code, message } => (StatusCode::NOT_FOUND, code, message),
        OperationError::Denied { code, message } => (StatusCode::FORBIDDEN, code, message),
        OperationError::Unauthenticated { code, message } => (StatusCode::UNAUTHORIZED, code, message),
        OperationError::Conflict { code, message } => (StatusCode::CONFLICT, code, message),
        OperationError::Unavailable { code, message } => (StatusCode::SERVICE_UNAVAILABLE, code, message),
        OperationError::TooLarge { code, message } => (StatusCode::PAYLOAD_TOO_LARGE, code, message),
        OperationError::BranchMerged { .. } => (StatusCode::SERVICE_UNAVAILABLE, "database_error", "The branch was merged, but the pull request status could not be updated.".into()),
        OperationError::Opened { .. } => (StatusCode::SERVICE_UNAVAILABLE, "git_unavailable", "Pull request opened but its head could not be fetched; retry after storage is available".into()),
        OperationError::Updated { .. } => (StatusCode::SERVICE_UNAVAILABLE, "database_error", "The change was saved, but pull request revisions could not be refreshed. Reload before retrying.".into()),
    };
    (status, Json(ApiError::new(code, &message)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::repositories::pull_requests::OperationError;

    #[test]
    fn merge_partial_failure_has_a_truthful_public_projection() {
        let (status, axum::Json(error)) = operation_error(OperationError::BranchMerged {
            sha: "a".repeat(40),
        });
        assert_eq!(status, axum::http::StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(error.error, "database_error");
        assert!(error.message.contains("branch was merged"));
        let (_, message) = pull::safe_api_error(&(status, axum::Json(error)));
        assert!(message.contains("branch was merged"));
    }
}
