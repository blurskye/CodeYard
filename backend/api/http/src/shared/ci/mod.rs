use crate::cell_routes::CellRoutes;
use crate::error::ApiError;
use axum::http::StatusCode;
use axum::routing::get;
use axum::routing::post;
use axum::Json;
use axum::Router;
use codeyard_app::app::AppState;
use codeyard_db_actions::ci_admission::AdmissionError;
use std::sync::Arc;
pub fn repository_router() -> Router<Arc<AppState>> {
    Router::new()
        .cell_route(
            "/:owner/:repo/actions/runs/:run_id/artifacts/:artifact/download",
            get(crate::shared::ci::run_forms::download_artifact),
        )
        .cell_route(
            "/:owner/:repo/actions/runs/:run_id/artifacts/:artifact/delete",
            post(crate::shared::ci::run_forms::delete_artifact),
        )
        .cell_route(
            "/:owner/:repo/actions",
            get(crate::shared::ci::actions_page::repository_actions),
        )
        .cell_route(
            "/:owner/:repo/actions/dispatch",
            post(crate::shared::ci::trigger::repository_dispatch),
        )
        .cell_route(
            "/:owner/:repo/actions/dispatch-event",
            get(crate::shared::ci::repository_events::form)
                .post(crate::shared::ci::repository_events::send)
                .layer(axum::extract::DefaultBodyLimit::max(256 * 1024)),
        )
        .cell_route(
            "/:owner/:repo/actions/runs/:run_id/cancel",
            post(crate::shared::ci::run_forms::cancel_run),
        )
        .cell_route(
            "/:owner/:repo/actions/runs/:run_id/rerun",
            post(crate::shared::ci::run_forms::rerun_run),
        )
}

pub fn admission_error(error: AdmissionError) -> (StatusCode, Json<ApiError>) {
    let status = match &error {
        AdmissionError::Missing(_) => StatusCode::NOT_FOUND,
        AdmissionError::Denied(_) => StatusCode::FORBIDDEN,
        AdmissionError::Conflict(_) | AdmissionError::WorkflowDisabled(_) => StatusCode::CONFLICT,
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

pub mod actions_page;

pub mod jobs;

pub mod repository_events;

pub mod responses;

pub mod results;

pub mod run_forms;

pub mod trigger;
