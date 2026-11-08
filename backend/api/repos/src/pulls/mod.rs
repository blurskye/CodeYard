use codeyard_api_http::cell_routes::CellRoutes;
pub mod checks;
pub mod collection;
pub mod comments;
pub mod commits;
pub mod diff;
pub mod merge;
pub mod pull;
pub mod ready;
pub mod requested_reviewers;
pub mod review_forms;
pub mod reviews;
mod security;
pub mod stack;
pub mod workflows;

pub fn json_router() -> axum::Router<std::sync::Arc<codeyard_app::app::AppState>> {
    use axum::{
        routing::{delete, get, post},
        Router,
    };
    Router::new()
        .cell_route(
            "/api/repos/:owner/:repo/pulls",
            get(crate::pulls::collection::list_pull_requests)
                .post(crate::pulls::collection::create_pull_request),
        )
        .cell_route(
            "/api/repos/:owner/:repo/pulls/:number",
            get(crate::pulls::pull::get_pull_request).put(crate::pulls::pull::update_pull_request),
        )
        .cell_route(
            "/api/repos/:owner/:repo/pulls/:number/workflows",
            post(crate::pulls::workflows::run_pull_request_workflows),
        )
        .cell_route(
            "/api/repos/:owner/:repo/pulls/:number/ready",
            post(crate::pulls::ready::mark_pull_request_ready),
        )
        .cell_route(
            "/api/repos/:owner/:repo/pulls/:number/merge",
            post(crate::pulls::merge::merge_pull_request),
        )
        .cell_route(
            "/api/repos/:owner/:repo/pulls/:number/comments",
            get(crate::pulls::comments::list_pull_request_comments)
                .post(crate::pulls::comments::create_pull_request_comment),
        )
        .cell_route(
            "/api/repos/:owner/:repo/pulls/:number/comments/:comment_id/resolve",
            post(crate::pulls::comments::resolve_pull_request_comment),
        )
        .cell_route(
            "/api/repos/:owner/:repo/pulls/:number/comments/:comment_id/unresolve",
            post(crate::pulls::comments::unresolve_pull_request_comment),
        )
        .cell_route(
            "/api/repos/:owner/:repo/pulls/:number/reviews",
            get(crate::pulls::reviews::list_pull_request_reviews)
                .post(crate::pulls::reviews::create_pull_request_review),
        )
        .cell_route(
            "/api/repos/:owner/:repo/pulls/:number/reviews/:review_id/dismiss",
            post(crate::pulls::reviews::dismiss_pull_request_review),
        )
        .cell_route(
            "/api/repos/:owner/:repo/pulls/:number/requested_reviewers",
            get(crate::pulls::requested_reviewers::list_requested_reviewers)
                .post(crate::pulls::requested_reviewers::request_reviewer),
        )
        .cell_route(
            "/api/repos/:owner/:repo/pulls/:number/requested_reviewers/:username",
            delete(crate::pulls::requested_reviewers::remove_requested_reviewer),
        )
        .cell_route(
            "/api/repos/:owner/:repo/pulls/:number/diff",
            get(crate::pulls::diff::get_pull_request_diff),
        )
        .cell_route(
            "/api/repos/:owner/:repo/pulls/:number/stack",
            post(crate::pulls::stack::post_stack).delete(crate::pulls::stack::delete_stack),
        )
        .cell_route(
            "/api/repos/:owner/:repo/pulls/:number/stack/rebase",
            post(crate::pulls::stack::post_rebase),
        )
}

/// GitHub links a pull request as `/pull/N`; the pages here live under `/pulls/N`.
async fn pull_alias(
    axum::extract::State(state): axum::extract::State<std::sync::Arc<codeyard_app::app::AppState>>,
    axum::extract::Path((owner, repo, number)): axum::extract::Path<(String, String, String)>,
    headers: axum::http::HeaderMap,
    uri: axum::http::Uri,
) -> axum::response::Response {
    for (suffix, patch) in [(".diff", false), (".patch", true)] {
        if let Some(number) = number
            .strip_suffix(suffix)
            .and_then(|n| n.parse::<i32>().ok())
        {
            // A download is a page: a private node admits only signed-in people to it.
            if let Err(response) =
                codeyard_api_http::context::PageContext::load(state.clone(), headers.clone(), &uri)
                    .await
            {
                return response;
            }
            return pull_text(&state, &owner, &repo, number, &headers, patch).await;
        }
    }
    pull_redirect(&owner, &repo, &number, "", &uri)
}

/// `/pull/N.diff` and `/pull/N.patch`, as GitHub serves them.
async fn pull_text(
    state: &codeyard_app::app::AppState,
    owner: &str,
    repo: &str,
    number: i32,
    headers: &axum::http::HeaderMap,
    patch: bool,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    let repository =
        match crate::access::readable_repository(&state.pool, owner, repo, headers).await {
            Ok(repository) => repository,
            Err(error) => return error.into_response(),
        };
    let pull = match codeyard_db_repos::pull_requests::records::get_pull_request(
        &state.pool,
        repository.id,
        number,
    )
    .await
    {
        Ok(Some(pull)) => pull,
        Ok(None) => return axum::http::StatusCode::NOT_FOUND.into_response(),
        Err(_) => return axum::http::StatusCode::SERVICE_UNAVAILABLE.into_response(),
    };
    let response = codeyard_api_github::pull_diff_text(state, &repository, &pull, patch).await;
    if let Err(error) = codeyard_api_http::shared::repos::access::check_repo_read_access(
        &state.pool,
        &repository,
        headers,
    )
    .await
    {
        return error.into_response();
    }
    codeyard_api_http::render::secure_html_headers(response)
}

async fn pull_alias_with_tab(
    axum::extract::Path((owner, repo, number, rest)): axum::extract::Path<(
        String,
        String,
        String,
        String,
    )>,
    uri: axum::http::Uri,
) -> axum::response::Response {
    pull_redirect(&owner, &repo, &number, &rest, &uri)
}

/// The path parts arrive decoded, so they are encoded again; a decoded `/` in the owner would
/// otherwise make the target `//host/...`, which browsers follow off the site.
fn pull_redirect(
    owner: &str,
    repo: &str,
    number: &str,
    rest: &str,
    uri: &axum::http::Uri,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    use codeyard_types::paths::encode_component;
    let tab = rest
        .split('/')
        .filter(|part| !part.is_empty())
        .map(|part| format!("/{}", encode_component(part)))
        .collect::<String>();
    let query = uri.query().map(|q| format!("?{q}")).unwrap_or_default();
    let target = format!(
        "/{}/{}/pulls/{}{tab}{query}",
        encode_component(owner),
        encode_component(repo),
        encode_component(number)
    );
    if !codeyard_api_http::origin::safe_redirect_path(&target) {
        return axum::http::StatusCode::NOT_FOUND.into_response();
    }
    axum::response::Redirect::permanent(&target).into_response()
}

pub fn page_router() -> axum::Router<std::sync::Arc<codeyard_app::app::AppState>> {
    use axum::{
        routing::{get, post},
        Router,
    };
    Router::new()
        .cell_route("/:owner/:repo/pull/:number", get(pull_alias))
        .cell_route("/:owner/:repo/pull/:number/*rest", get(pull_alias_with_tab))
        .cell_route(
            "/:owner/:repo/pulls",
            get(crate::issues::collection::native_index),
        )
        .cell_route(
            "/:owner/:repo/pulls/new",
            get(crate::issues::collection::native_new_page)
                .post(crate::issues::collection::native_create),
        )
        .cell_route(
            "/:owner/:repo/pulls/:number",
            get(crate::issues::issue::native_detail),
        )
        .cell_route(
            "/:owner/:repo/pulls/:number/edit",
            post(crate::issues::issue::native_edit),
        )
        .cell_route(
            "/:owner/:repo/pulls/:number/state",
            post(crate::issues::issue::native_change_state),
        )
        .cell_route(
            "/:owner/:repo/pulls/:number/tasks",
            post(crate::issues::tasks::native_toggle_task),
        )
        .cell_route(
            "/:owner/:repo/pulls/:number/comments",
            post(crate::pulls::review_forms::native_pull_comment),
        )
        .cell_route(
            "/:owner/:repo/pulls/:number/lock",
            post(crate::issues::locking::native_lock),
        )
        .cell_route(
            "/:owner/:repo/pulls/:number/comments/:comment/pin",
            post(crate::issues::pins::native_pin),
        )
        .cell_route(
            "/:owner/:repo/pulls/:number/react",
            post(crate::issues::reactions::native_react),
        )
        .cell_route(
            "/:owner/:repo/pulls/:number/comments/:comment_id/edit",
            post(crate::pulls::review_forms::native_edit_conversation_comment),
        )
        .cell_route(
            "/:owner/:repo/pulls/:number/comments/:comment_id/delete",
            post(crate::pulls::review_forms::native_delete_conversation_comment),
        )
        .cell_route(
            "/:owner/:repo/pulls/:number/comments/:comment_id/resolve",
            post(crate::pulls::review_forms::native_resolve_comment),
        )
        .cell_route(
            "/:owner/:repo/pulls/:number/comments/:comment_id/unresolve",
            post(crate::pulls::review_forms::native_unresolve_comment),
        )
        .cell_route(
            "/:owner/:repo/pulls/:number/reviews",
            post(crate::pulls::review_forms::native_review),
        )
        .cell_route(
            "/:owner/:repo/pulls/:number/reviews/pending/discard",
            post(crate::pulls::review_forms::native_discard_review),
        )
        .cell_route(
            "/:owner/:repo/pulls/:number/reviews/:review_id/dismiss",
            post(crate::pulls::review_forms::native_dismiss_review),
        )
        .cell_route(
            "/:owner/:repo/pulls/:number/requested_reviewers",
            post(crate::pulls::review_forms::native_request_reviewer),
        )
        .cell_route(
            "/:owner/:repo/pulls/:number/requested_reviewers/:username/remove",
            post(crate::pulls::review_forms::native_remove_requested_reviewer),
        )
        .cell_route(
            "/:owner/:repo/pulls/:number/commits",
            get(crate::pulls::commits::native_commits),
        )
        .cell_route(
            "/:owner/:repo/pulls/:number/checks",
            get(crate::pulls::checks::native_checks),
        )
        .cell_route(
            "/:owner/:repo/pulls/:number/diff",
            get(crate::pulls::diff::native_diff),
        )
        // GitHub's address for the same page, so shared links to it open the Files changed view.
        .cell_route(
            "/:owner/:repo/pulls/:number/files",
            get(crate::pulls::diff::native_diff),
        )
        .cell_route(
            "/:owner/:repo/pulls/:number/viewed",
            post(crate::pulls::review_forms::native_set_viewed),
        )
        .cell_route(
            "/:owner/:repo/pulls/:number/ready",
            post(crate::pulls::ready::native_ready),
        )
        .cell_route(
            "/:owner/:repo/pulls/:number/workflows",
            post(crate::pulls::workflows::native_run_workflows),
        )
        .cell_route(
            "/:owner/:repo/pulls/:number/merge",
            post(crate::pulls::merge::native_merge),
        )
        .cell_route(
            "/:owner/:repo/pulls/:number/stack",
            post(crate::pulls::stack::native_join),
        )
        .cell_route(
            "/:owner/:repo/pulls/:number/unstack",
            post(crate::pulls::stack::native_unstack),
        )
        .cell_route(
            "/:owner/:repo/pulls/:number/stack/rebase",
            post(crate::pulls::stack::native_rebase),
        )
        .cell_route(
            "/:owner/:repo/pulls/:number/labels",
            post(crate::issues::labels::native_add_label),
        )
        .cell_route(
            "/:owner/:repo/pulls/:number/labels/:label_id/remove",
            post(crate::issues::labels::native_remove_label),
        )
        .cell_route(
            "/:owner/:repo/pulls/:number/assignees",
            post(crate::issues::assignees::native_assign),
        )
        .cell_route(
            "/:owner/:repo/pulls/:number/assignees/:username/remove",
            post(crate::issues::assignees::native_unassign),
        )
}

/// The refusals that report a change that was partly made. `safe_api_error` recognizes them by
/// these messages.
pub(crate) const BRANCH_MERGED_MESSAGE: &str =
    "The branch was merged, but the pull request status could not be updated.";
pub(crate) const OPENED_MESSAGE: &str =
    "Pull request opened but its head could not be fetched; retry after storage is available";
pub(crate) const UPDATED_MESSAGE: &str = "The change was saved, but pull request revisions could not be refreshed. Reload before retrying.";

pub(crate) fn operation_error(
    error: codeyard_git_storage::repositories::pull_requests::OperationError,
) -> (
    axum::http::StatusCode,
    axum::Json<codeyard_api_http::error::ApiError>,
) {
    use axum::{http::StatusCode, Json};
    use codeyard_api_http::error::ApiError;
    use codeyard_git_storage::repositories::pull_requests::OperationError;
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
        OperationError::TooLarge { code, message } => {
            (StatusCode::PAYLOAD_TOO_LARGE, code, message)
        }
        OperationError::BranchMerged { .. } => (
            StatusCode::SERVICE_UNAVAILABLE,
            "database_error",
            BRANCH_MERGED_MESSAGE.into(),
        ),
        OperationError::Opened { .. } => (
            StatusCode::SERVICE_UNAVAILABLE,
            "git_unavailable",
            OPENED_MESSAGE.into(),
        ),
        OperationError::Updated { .. } => (
            StatusCode::SERVICE_UNAVAILABLE,
            "database_error",
            UPDATED_MESSAGE.into(),
        ),
    };
    (status, Json(ApiError::new(code, &message)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::discussion::safe_api_error;
    use codeyard_git_storage::repositories::pull_requests::OperationError;

    #[test]
    fn merge_partial_failure_has_a_truthful_public_projection() {
        let (status, axum::Json(error)) = operation_error(OperationError::BranchMerged {
            sha: "a".repeat(40),
        });
        assert_eq!(status, axum::http::StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(error.error, "database_error");
        assert!(error.message.contains("branch was merged"));
        let (_, message) = safe_api_error(&(status, axum::Json(error)));
        assert!(message.contains("branch was merged"));
    }
}
