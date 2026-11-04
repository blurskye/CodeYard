//! Job and step reads use actual runner observations; no parsed YAML is presented as execution.
use crate::actions::id;
use axum::extract::OriginalUri;
use axum::extract::Path;
use axum::extract::Query;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::response::Response;
use axum::routing::get;
use axum::routing::MethodRouter;
use axum::Json;
use codeyard_api_http::shared::github::pagination::Pagination;
use codeyard_app::api_shared::github::repository;
use codeyard_app::app::AppState;
use codeyard_model::github_error::Error;
use codeyard_model::github_error::Result;
use serde::Deserialize;
use serde_json::json;
use serde_json::Value;
use std::collections::HashMap;
use std::sync::Arc;

async fn document(
    state: &AppState,
    repo: &codeyard_db_core::models::Repository,
    owner: &str,
    job: &codeyard_db_actions::action_jobs::Job,
) -> Result<Value> {
    let run = codeyard_db_actions::ci::get_ci_run_by_id(&state.pool, repo.id, job.run_id)
        .await
        .map_err(Error::unavailable)?
        .ok_or_else(Error::missing)?;
    let steps = codeyard_db_actions::action_jobs::steps(&state.pool, repo.id, job.id)
        .await
        .map_err(Error::unavailable)?;
    Ok(codeyard_app::webhooks::payloads::action_job(
        job, &steps, &run, owner, &repo.name,
    ))
}
async fn get_one(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, name, id)): Path<(String, String, i64)>,
) -> Result<Json<Value>> {
    let (repo, _) = repository(&state, &headers, &owner, &name, false).await?;
    let job = codeyard_db_actions::action_jobs::get(&state.pool, repo.id, id, false)
        .await
        .map_err(Error::unavailable)?
        .ok_or_else(Error::missing)?;
    Ok(Json(document(&state, &repo, &owner, &job).await?))
}
#[derive(Deserialize)]
struct Filter {
    filter: Option<String>,
}
async fn list(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(params): Path<HashMap<String, String>>,
    Query(page): Query<Pagination>,
    Query(filter): Query<Filter>,
    OriginalUri(uri): OriginalUri,
) -> Result<Response> {
    let owner = params.get("owner").ok_or_else(Error::missing)?;
    let name = params.get("repo").ok_or_else(Error::missing)?;
    let (repo, _) = repository(&state, &headers, owner, name, false).await?;
    let attempt = if params.contains_key("attempt_number") {
        Some(i32::try_from(id(&params, "attempt_number")?).map_err(|_| Error::missing())?)
    } else {
        None
    };
    let run = codeyard_db_actions::action_runs::get(
        &state.pool,
        repo.id,
        id(&params, "run_id")?,
        attempt,
    )
    .await
    .map_err(Error::unavailable)?
    .ok_or_else(Error::missing)?;
    let all = match filter.filter.as_deref().unwrap_or("latest") {
        "latest" => false,
        "all" if attempt.is_none() => true,
        "all" => false,
        _ => return Err(Error::invalid("Invalid job filter")),
    };
    if !all
        && run.started_at.is_some()
        && !codeyard_db_actions::action_jobs::observed(&state.pool, repo.id, run.id)
            .await
            .map_err(Error::unavailable)?
    {
        return Err(Error::new(
            StatusCode::NOT_FOUND,
            "Job metadata is unavailable for this engine or historical execution",
        ));
    }
    let (offset, limit) = page.window()?;
    let result = codeyard_db_actions::action_jobs::list(
        &state.pool,
        repo.id,
        if all {
            run.workflow_run_id.unwrap_or(run.id)
        } else {
            run.id
        },
        all,
        offset as i64,
        limit as i64,
    )
    .await
    .map_err(Error::unavailable)?;
    let mut documents = Vec::with_capacity(result.jobs.len());
    for job in &result.jobs {
        documents.push(document(&state, &repo, owner, job).await?);
    }
    Ok((
        page.headers(&uri, ((offset + documents.len()) as i64) < result.total)?,
        Json(json!({"total_count":result.total,"jobs":documents})),
    )
        .into_response())
}

/// Annotations of an Actions job, which GitHub serves as a check run.
pub(crate) async fn annotations(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, name, id)): Path<(String, String, i64)>,
    Query(page): Query<Pagination>,
    OriginalUri(uri): OriginalUri,
) -> Result<Response> {
    let (repo, _) = repository(&state, &headers, &owner, &name, false).await?;
    codeyard_db_actions::action_jobs::get(&state.pool, repo.id, id, false)
        .await
        .map_err(Error::unavailable)?
        .ok_or_else(Error::missing)?;
    let (offset, limit) = page.window()?;
    let (total, annotations) = codeyard_db_actions::action_jobs::annotations(
        &state.pool,
        repo.id,
        id,
        offset as i64,
        limit as i64,
    )
    .await
    .map_err(Error::unavailable)?;
    let values: Vec<_> = annotations
        .into_iter()
        .map(|row| {
            let mut value = json!(row.0);
            value["start_column"] = Value::Null;
            value["end_column"] = Value::Null;
            value["raw_details"] = Value::Null;
            value["blob_href"] = json!("");
            value
        })
        .collect();
    Ok((
        page.headers(&uri, (offset.saturating_add(limit) as i64) < total)?,
        Json(values),
    )
        .into_response())
}

pub(super) fn handlers() -> Vec<(&'static str, MethodRouter<Arc<AppState>>)> {
    vec![
        ("actions/get-job-for-workflow-run", get(get_one)),
        ("actions/list-jobs-for-workflow-run", get(list)),
        ("actions/list-jobs-for-workflow-run-attempt", get(list)),
    ]
}
