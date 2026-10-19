use super::history::{readable_history, LogHistoryQuery};
use super::{authorized_run, unavailable, CiReadError};
use crate::backend::ci::logs::normalize_stage_name;
use crate::backend::{app::AppState, db};
use axum::{
    extract::{Path, Query, State},
    http::HeaderMap,
    response::{IntoResponse, Response},
    Json,
};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

#[derive(Debug, Serialize, Deserialize)]
pub struct JobLogLine {
    pub stage: String,
    pub line: String,
    pub level: String,
    pub timestamp: i64,
}

#[derive(Debug, Deserialize)]
pub struct JobLogsQuery {
    pub limit: Option<i64>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct JobLogsResponse {
    pub job_id: String,
    pub logs: Vec<JobLogLine>,
}

pub async fn get_ci_job_logs(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(job_id): Path<String>,
    Query(query): Query<JobLogsQuery>,
) -> Result<Json<JobLogsResponse>, CiReadError> {
    let run = authorized_run(&state, &headers, &job_id).await?;
    let rows = db::ci::list_ci_logs(
        &state.pool,
        run.id,
        query.limit.unwrap_or(2000).clamp(1, 5000),
    )
    .await
    .map_err(unavailable)?;
    Ok(Json(JobLogsResponse {
        job_id,
        logs: rows
            .into_iter()
            .map(|row| JobLogLine {
                stage: normalize_stage_name(&row.stage),
                line: row.line,
                level: row.level,
                timestamp: row.created_at.timestamp(),
            })
            .collect(),
    }))
}
/// Download bounded chunks by following X-CI-Next-Cursor with ?after=... until
/// X-CI-Has-More=false. Live histories can gain rows after a page is returned.
pub async fn download_ci_job_logs(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(job_id): Path<String>,
    Query(query): Query<LogHistoryQuery>,
) -> Result<Response, CiReadError> {
    let (run, rows, next, more) = readable_history(&state, &headers, &job_id, &query).await?;
    let mut text = String::new();
    use std::fmt::Write;
    for row in rows {
        let _ = writeln!(
            text,
            "{} [{}] [{}] {}",
            row.id, row.stage, row.level, row.line
        );
    }
    let mut response = text.into_response();
    let headers = response.headers_mut();
    headers.insert(
        "content-type",
        "text/plain; charset=utf-8"
            .parse()
            .expect("static content type"),
    );
    headers.insert(
        "cache-control",
        "private, no-store".parse().expect("static cache policy"),
    );
    headers.insert(
        "x-content-type-options",
        "nosniff".parse().expect("static nosniff"),
    );
    headers.insert(
        "content-disposition",
        format!("attachment; filename=\"ci-{}-logs.txt\"", run.run_id)
            .parse()
            .expect("UUID attachment name"),
    );
    headers.insert(
        "x-ci-next-cursor",
        next.to_string().parse().expect("numeric cursor"),
    );
    headers.insert(
        "x-ci-has-more",
        (if more { "true" } else { "false" })
            .parse()
            .expect("boolean header"),
    );
    Ok(response)
}
