use super::{authorized_run, unavailable, CiReadError};
use crate::backend::ci::logs::normalize_stage_name;
use crate::backend::{api::http::error::ApiError, app::AppState, db};
use crate::webui::pages::repos::actions::{logs::LogLine, Deployment, RunLogUpdate, RunProgress};
use axum::{
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode},
    Json,
};
use serde::Deserialize;
use std::sync::Arc;

#[derive(Debug, Deserialize)]
pub struct LogHistoryQuery {
    pub after: Option<i64>,
    pub limit: Option<i64>,
}

pub(super) async fn readable_history(
    state: &AppState,
    headers: &HeaderMap,
    job_id: &str,
    query: &LogHistoryQuery,
) -> Result<(db::models::CiRun, Vec<db::models::CiLogLine>, i64, bool), CiReadError> {
    let run = authorized_run(state, headers, job_id).await?;
    let after = query.after.unwrap_or(0);
    if after < 0 {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(ApiError::new(
                "validation_error",
                "after must be a nonnegative persisted log ID",
            )),
        ));
    }
    let limit = query.limit.unwrap_or(500).clamp(1, 1000);
    let mut rows = db::ci::list_ci_log_history(&state.pool, run.id, after, limit + 1)
        .await
        .map_err(unavailable)?;
    let mut has_more = rows.len() > limit as usize;
    rows.truncate(limit as usize);
    // Bound response bytes too. Never advance the cursor past an omitted row.
    let mut bytes = 0_usize;
    let mut count = 0;
    for row in &rows {
        let size = row
            .line
            .len()
            .saturating_add(row.stage.len())
            .saturating_add(128);
        if bytes.saturating_add(size) > 2 * 1024 * 1024 {
            break;
        }
        bytes += size;
        count += 1;
    }
    has_more |= count < rows.len();
    rows.truncate(count);
    let next = rows.last().map(|row| row.id).unwrap_or(after);
    Ok((run, rows, next, has_more))
}

pub async fn get_ci_job_history(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(job_id): Path<String>,
    Query(query): Query<LogHistoryQuery>,
) -> Result<Json<RunLogUpdate>, CiReadError> {
    let (run, rows, next_cursor, has_more) =
        readable_history(&state, &headers, &job_id, &query).await?;
    let deployment = db::ci::get_ci_deployment_by_run_id(&state.pool, run.id)
        .await
        .map_err(unavailable)?
        .map(|row| Deployment {
            environment: row.environment,
            status: row.status,
            approved_at: row.approved_at.map(|time| time.timestamp()),
            started_at: row.started_at.map(|time| time.timestamp()),
            completed_at: row.completed_at.map(|time| time.timestamp()),
        });
    Ok(Json(RunLogUpdate {
        progress: RunProgress {
            status: run.status,
            started_at: run.started_at.map(|time| time.timestamp()),
            completed_at: run.completed_at.map(|time| time.timestamp()),
            error_message: crate::backend::ci::visible_run_error(run.error_message.as_deref()),
        },
        deployment,
        job_id,
        next_cursor,
        has_more,
        workflow_sha: run.workflow_sha,
        logs: rows
            .into_iter()
            .map(|row| LogLine {
                id: row.id,
                stage: normalize_stage_name(&row.stage),
                line: row.line,
                level: row.level,
                timestamp: row.created_at.timestamp(),
            })
            .collect(),
    }))
}
