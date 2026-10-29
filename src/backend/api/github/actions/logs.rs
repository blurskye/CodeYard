//! Log redirects are minute-long capabilities over immutable, bounded native snapshots.
use super::id;
use crate::backend::{
    api::{
        github::{api_url, auth, repository, Error, Result},
        http::session,
    },
    app::AppState,
    ci::{logs::archive, runtime},
    db::{
        self,
        action_logs::{self, Target},
    },
};
use axum::{
    extract::{OriginalUri, Path, Query, State},
    http::{header, HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, MethodRouter},
};
use serde::Deserialize;
use std::{collections::HashMap, sync::Arc};

#[derive(Deserialize)]
struct Parameters {
    codeyard_download: Option<String>,
    after: Option<i64>,
    limit: Option<i64>,
}

async fn live_logs(
    state: &AppState,
    run: db::models::CiRun,
    after: i64,
    limit: Option<i64>,
) -> Result<Response> {
    use crate::webui::pages::repos::actions::{
        logs::LogLine, Deployment, RunLogUpdate, RunProgress,
    };
    if after < 0 {
        return Err(Error::invalid(
            "after must be a nonnegative persisted log ID",
        ));
    }
    let limit = limit.unwrap_or(500).clamp(1, 1000) as usize;
    let mut rows = db::ci::list_ci_log_history(&state.pool, run.id, after, limit as i64 + 1)
        .await
        .map_err(Error::unavailable)?;
    let mut has_more = rows.len() > limit;
    rows.truncate(limit);
    let mut bytes = 0_usize;
    let count = rows
        .iter()
        .take_while(|row| {
            bytes = bytes
                .saturating_add(row.line.len())
                .saturating_add(row.stage.len())
                .saturating_add(128);
            bytes <= 2 * 1024 * 1024
        })
        .count();
    has_more |= count < rows.len();
    rows.truncate(count);
    let next_cursor = rows.last().map_or(after, |row| row.id);
    let deployment = db::ci::get_ci_deployment_by_run_id(&state.pool, run.id)
        .await
        .map_err(Error::unavailable)?
        .map(|row| Deployment {
            environment: row.environment,
            status: row.status,
            approved_at: row.approved_at.map(|time| time.timestamp()),
            started_at: row.started_at.map(|time| time.timestamp()),
            completed_at: row.completed_at.map(|time| time.timestamp()),
        });
    Ok(axum::Json(RunLogUpdate {
        job_id: run.run_id.to_string(),
        logs: rows
            .into_iter()
            .map(|row| LogLine {
                id: row.id,
                stage: crate::backend::ci::logs::normalize_stage_name(&row.stage),
                line: row.line,
                level: row.level,
                timestamp: row.created_at.timestamp(),
            })
            .collect(),
        next_cursor,
        has_more,
        workflow_sha: run.workflow_sha,
        progress: RunProgress {
            status: run.status,
            started_at: run.started_at.map(|time| time.timestamp()),
            completed_at: run.completed_at.map(|time| time.timestamp()),
            error_message: crate::backend::ci::visible_run_error(run.error_message.as_deref()),
        },
        deployment,
    })
    .into_response())
}
async fn download(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(params): Path<HashMap<String, String>>,
    Query(query): Query<Parameters>,
    OriginalUri(uri): OriginalUri,
) -> Result<Response> {
    let resource = uri.path();
    if let Some(token) = query.codeyard_download {
        // A snapshot holds up to 32 MiB; the shared download slots bound how many are loaded,
        // encoded and sent at once, and the slot is held until the client has the last byte.
        let permit = crate::backend::api::http::middleware::wait_for_slot(&state.repository_downloads)
            .await
            .ok_or_else(|| Error::unavailable(()))?;
        let snapshot = action_logs::read(&state.pool, &runtime::token_hash(&token), resource)
            .await
            .map_err(Error::unavailable)?
            .ok_or_else(Error::missing)?;
        let (bytes, kind, filename) =
            tokio::task::spawn_blocking(move || archive::encode(snapshot))
                .await
                .map_err(Error::unavailable)?
                .map_err(Error::unavailable)?;
        return Ok((
            [
                (header::CONTENT_TYPE, kind.to_owned()),
                (
                    header::CONTENT_DISPOSITION,
                    format!("attachment; filename=\"{filename}\""),
                ),
            ],
            crate::backend::api::http::middleware::buffered_download_body(bytes, permit),
        )
            .into_response());
    }
    let owner = params.get("owner").ok_or_else(Error::missing)?;
    let name = params.get("repo").ok_or_else(Error::missing)?;
    // Only this read-only operation accepts the same-origin browser's session. An explicit
    // Authorization header always takes precedence, including invalid or restricted tokens.
    let (repo, actor) =
        if !headers.contains_key(header::AUTHORIZATION) && session::has_session_cookie(&headers) {
            let user = session::get_current_user(&state.pool, &headers)
                .await
                .map_err(|(status, axum::Json(error))| Error::new(status, error.message))?;
            let repo = db::repositories::records::get_repository(&state.pool, owner, name)
                .await
                .map_err(Error::unavailable)?
                .ok_or_else(Error::missing)?;
            if !crate::backend::repositories::access::can_read(&state.pool, Some(&user), &repo)
                .await
                .map_err(Error::unavailable)?
            {
                return Err(Error::missing());
            }
            (repo, db::credentials::Actor::from((user.id, session::extract_session_id(&headers).unwrap_or_default())))
        } else {
            let (repo, authentication) = repository(&state, &headers, owner, name, false).await?;
            (
                repo,
                authentication.as_ref().ok_or_else(auth::required)?.database_actor(&headers)?,
            )
        };
    if query.after.is_some() && params.contains_key("job_id") {
        return Err(Error::invalid("Live logs require a workflow run"));
    }
    let target = if params.contains_key("job_id") {
        let job = db::action_jobs::get(&state.pool, repo.id, id(&params, "job_id")?, false)
            .await
            .map_err(Error::unavailable)?
            .ok_or_else(Error::missing)?;
        let step = if params.contains_key("step_number") {
            Some(i32::try_from(id(&params, "step_number")?).map_err(|_| Error::missing())?)
        } else {
            None
        };
        Target {
            run: job.run_id,
            job: Some(job.id),
            step,
        }
    } else {
        let attempt = if params.contains_key("attempt_number") {
            Some(i32::try_from(id(&params, "attempt_number")?).map_err(|_| Error::missing())?)
        } else {
            None
        };
        let run = db::action_runs::get(&state.pool, repo.id, id(&params, "run_id")?, attempt)
            .await
            .map_err(Error::unavailable)?
            .ok_or_else(Error::missing)?;
        if let Some(after) = query.after {
            return live_logs(&state, run, after, query.limit).await;
        }
        Target {
            run: run.id,
            job: None,
            step: None,
        }
    };
    let token = runtime::secret();
    if !action_logs::issue(
        &state.pool,
        actor,
        repo.id,
        resource,
        target,
        &runtime::token_hash(&token),
    )
    .await
    .map_err(Error::unavailable)?
    {
        return Err(Error::missing());
    }
    let location = api_url(&format!(
        "{}?codeyard_download={token}",
        resource.strip_prefix("/api/v3").unwrap_or(resource)
    ));
    Ok((StatusCode::FOUND, [(header::LOCATION, location)]).into_response())
}

pub(super) fn handlers() -> Vec<(&'static str, MethodRouter<Arc<AppState>>)> {
    vec![
        ("actions/download-job-logs-for-workflow-run", get(download)),
        (
            "actions/download-step-logs-for-workflow-run-job",
            get(download),
        ),
        ("actions/download-workflow-run-logs", get(download)),
        ("actions/download-workflow-run-attempt-logs", get(download)),
    ]
}
