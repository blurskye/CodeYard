//! JSON and browser CI resources retain their distinct authentication and response layers.
use axum::middleware::from_fn;
use axum::routing::get;
use axum::routing::post;
use axum::Router;
use codeyard_app::app::AppState;
use std::sync::Arc;
pub mod actions_page;
pub mod jobs;
pub(crate) mod oidc;
pub mod repository_events;
pub mod responses;
pub(crate) mod results;
pub mod run_forms;
mod runners;
pub mod trigger;
pub mod workflows;

pub fn router() -> Router<Arc<AppState>> {
    let json = Router::new()
        .route(
            "/api/ci/trigger",
            post(crate::ci::trigger::trigger_ci)
                .layer(from_fn(codeyard_api_http::csrf::cookie_api_submission)),
        )
        .route(
            "/api/ci/workflows",
            get(crate::ci::workflows::list_ci_workflows),
        )
        .merge(crate::ci::jobs::router())
        .route_layer(from_fn(codeyard_api_http::middleware::require_node_access))
        .layer(from_fn(
            codeyard_api_http::middleware::private_sensitive_response,
        ));
    repository_router()
        .merge(json)
        .merge(crate::ci::runners::router())
        .route("/ci", get(crate::ci::actions_page::global_actions))
        .route("/ci/dispatch", post(crate::ci::trigger::global_dispatch))
}

#[cfg(test)]
mod tests {
    use axum::http::StatusCode;
    use axum::Json;
    use codeyard_api_http::shared::ci::admission_error;
    use codeyard_db_core::ci_admission::AdmissionError;
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
        use codeyard_db_core::ci_policy::CiAdmissionRefusal;
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

use codeyard_api_http::shared::ci::repository_router;
