use super::{authorized_run, unavailable, CiReadError};
use crate::backend::ci::logs::normalize_stage_name;
use crate::backend::{api::error::ApiError, app::AppState, db};
use axum::{
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode},
    Json,
};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

#[derive(Debug, Deserialize)]
pub struct LogHistoryQuery {
    pub after: Option<i64>,
    pub limit: Option<i64>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct CursorLogLine {
    pub id: i64,
    pub stage: String,
    pub line: String,
    pub level: String,
    pub timestamp: i64,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct LogHistoryResponse {
    pub job_id: String,
    pub logs: Vec<CursorLogLine>,
    pub next_cursor: i64,
    pub has_more: bool,
    pub workflow_sha: Option<String>,
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
    if !rows.is_empty() && count == 0 {
        return Err((
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(ApiError::new(
                "legacy_log_too_large",
                "This legacy log row exceeds the bounded history response size",
            )),
        ));
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
) -> Result<Json<LogHistoryResponse>, CiReadError> {
    let (run, rows, next_cursor, has_more) =
        readable_history(&state, &headers, &job_id, &query).await?;
    Ok(Json(LogHistoryResponse {
        job_id,
        next_cursor,
        has_more,
        workflow_sha: run.workflow_sha,
        logs: rows
            .into_iter()
            .map(|row| CursorLogLine {
                id: row.id,
                stage: normalize_stage_name(&row.stage),
                line: row.line,
                level: row.level,
                timestamp: row.created_at.timestamp(),
            })
            .collect(),
    }))
}
