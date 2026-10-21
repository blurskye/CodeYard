//! JSON and browser CI resources retain their distinct authentication and response layers.
pub mod actions_page;
pub mod jobs;
pub mod responses;
pub mod run_forms;
pub mod trigger;
pub mod workflows;

use crate::backend::{
    api::{http::csrf, http::error::ApiError, http::middleware},
    app::AppState,
    ci::admission::AdmissionError,
};
use axum::{
    http::StatusCode,
    middleware::from_fn,
    routing::{get, post},
    Json, Router,
};
use std::sync::Arc;

pub fn router() -> Router<Arc<AppState>> {
    let json = Router::new()
        .route(
            "/api/ci/trigger",
            post(trigger::trigger_ci).layer(from_fn(csrf::cookie_api_submission)),
        )
        .route("/api/ci/workflows", get(workflows::list_ci_workflows))
        .merge(jobs::router())
        .route_layer(from_fn(middleware::require_node_access))
        .layer(from_fn(middleware::private_sensitive_response));
    Router::new()
        .merge(json)
        .route("/ci", get(actions_page::global_actions))
        .route("/ci/dispatch", post(trigger::global_dispatch))
        .route(
            "/:owner/:repo/actions",
            get(actions_page::repository_actions),
        )
        .route(
            "/:owner/:repo/actions/dispatch",
            post(trigger::repository_dispatch),
        )
        .route(
            "/:owner/:repo/actions/runs/:run_id/cancel",
            post(run_forms::cancel_run),
        )
        .route(
            "/:owner/:repo/actions/runs/:run_id/rerun",
            post(run_forms::rerun_run),
        )
}

pub(super) fn admission_error(error: AdmissionError) -> (StatusCode, Json<ApiError>) {
    let status = match &error {
        AdmissionError::Missing(_) => StatusCode::NOT_FOUND,
        AdmissionError::Denied(_) => StatusCode::FORBIDDEN,
        AdmissionError::Conflict(_) => StatusCode::CONFLICT,
        AdmissionError::Invalid(_) | AdmissionError::InvalidRequest(_) => StatusCode::BAD_REQUEST,
        AdmissionError::Unavailable => StatusCode::SERVICE_UNAVAILABLE,
    };
    let code = match &error {
        AdmissionError::Missing(_) => "not_found",
        AdmissionError::InvalidRequest(_) => "validation_error",
        _ => "ci_error",
    };
    (status, Json(ApiError::new(code, &error.to_string())))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn typed_refusals_keep_http_categories() {
        assert_eq!(
            admission_error(AdmissionError::Denied("CI actor is restricted")).0,
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            admission_error(AdmissionError::Conflict("CI ref unavailable")).0,
            StatusCode::CONFLICT
        );
        assert_eq!(
            admission_error(AdmissionError::Invalid("invalid CI ref")).0,
            StatusCode::BAD_REQUEST
        );
    }
    #[test]
    fn database_refusals_keep_public_http_categories() {
        use crate::backend::db::ci::CiAdmissionRefusal;
        for (refusal, expected) in [
            (
                CiAdmissionRefusal::Missing("Run not found"),
                StatusCode::NOT_FOUND,
            ),
            (
                CiAdmissionRefusal::Denied("CI actor is restricted"),
                StatusCode::FORBIDDEN,
            ),
            (
                CiAdmissionRefusal::Conflict("CI repository is archived"),
                StatusCode::CONFLICT,
            ),
            (
                CiAdmissionRefusal::Invalid("invalid CI ref"),
                StatusCode::BAD_REQUEST,
            ),
        ] {
            let message = refusal.to_string();
            let (status, Json(body)) = admission_error(anyhow::Error::new(refusal).into());
            assert_eq!(status, expected);
            let body = serde_json::to_value(body).unwrap();
            assert_eq!(body["message"], message);
            assert_eq!(
                body["error"],
                if status == StatusCode::NOT_FOUND {
                    "not_found"
                } else {
                    "ci_error"
                }
            );
        }
        assert_eq!(
            admission_error(anyhow::Error::new(sqlx::Error::PoolClosed).into()).0,
            StatusCode::SERVICE_UNAVAILABLE
        );
    }

    #[test]
    fn internal_errors_are_not_public_diagnostics() {
        let error = anyhow::anyhow!("SQL password=secret /private/repo.git");
        let (status, Json(body)) = admission_error(error.into());
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        let json = serde_json::to_string(&body).unwrap();
        assert!(!json.contains("secret"));
        assert!(!json.contains("private/repo"));
    }
}
