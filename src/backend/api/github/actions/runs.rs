//! Run documents project durable executions, not invented check suites or billing records.
use super::id;
use crate::backend::{
    api::github::{auth, pagination, repository, Error, Result},
    app::AppState,
    db::{
        self,
        action_runs::{self, RunQuery},
    },
    repositories::store::repo_store,
    webhooks::payloads,
};
use axum::{
    extract::{OriginalUri, Path, Query, State},
    http::{HeaderMap, StatusCode},
    routing::{get, MethodRouter},
    Json,
};
use chrono::{DateTime, Days, NaiveDate, Utc};
use serde::Deserialize;
use serde_json::{json, Value};
use std::{collections::HashMap, sync::Arc};

#[derive(Deserialize)]
pub(super) struct Parameters {
    actor: Option<String>,
    branch: Option<String>,
    event: Option<String>,
    status: Option<String>,
    created: Option<String>,
    head_sha: Option<String>,
    check_suite_id: Option<i64>,
    #[serde(default)]
    exclude_pull_requests: bool,
}

fn date(value: &str) -> Result<(DateTime<Utc>, bool)> {
    if let Ok(date) = NaiveDate::parse_from_str(value, "%Y-%m-%d") {
        return Ok((
            date.and_hms_opt(0, 0, 0)
                .ok_or_else(|| Error::invalid("Invalid created date"))?
                .and_utc(),
            true,
        ));
    }
    DateTime::parse_from_rfc3339(value)
        .map(|date| (date.with_timezone(&Utc), false))
        .map_err(|_| Error::invalid("Invalid created date"))
}
fn next_day(date: DateTime<Utc>) -> Result<DateTime<Utc>> {
    date.checked_add_days(Days::new(1))
        .ok_or_else(|| Error::invalid("Invalid created date"))
}
fn created(value: &str, q: &mut RunQuery) -> Result<()> {
    if let Some((start, end)) = value.split_once("..") {
        if !matches!(start, "" | "*") {
            q.since = Some(date(start)?.0);
        }
        if !matches!(end, "" | "*") {
            let (end, day) = date(end)?;
            q.until = Some(if day { next_day(end)? } else { end });
            q.until_inclusive = !day;
        }
        if q.since.is_none() && q.until.is_none() {
            return Err(Error::invalid("Invalid created range"));
        }
    } else if let Some(value) = value.strip_prefix(">=") {
        q.since = Some(date(value)?.0);
    } else if let Some(value) = value.strip_prefix('>') {
        let (value, day) = date(value)?;
        q.since = Some(if day { next_day(value)? } else { value });
        q.since_exclusive = !day;
    } else if let Some(value) = value.strip_prefix("<=") {
        let (value, day) = date(value)?;
        q.until = Some(if day { next_day(value)? } else { value });
        q.until_inclusive = !day;
    } else if let Some(value) = value.strip_prefix('<') {
        q.until = Some(date(value)?.0);
    } else {
        let (value, day) = date(value)?;
        q.since = Some(value);
        q.until = Some(if day { next_day(value)? } else { value });
        q.until_inclusive = !day;
    }
    if q.since.zip(q.until).is_some_and(|(start, end)| start > end) {
        return Err(Error::invalid("Invalid created range"));
    }
    Ok(())
}

struct Documents {
    metadata: HashMap<i64, action_runs::RunMetadata>,
    actors: HashMap<i64, Value>,
    commits: HashMap<String, Value>,
    workflows: HashMap<String, HashMap<String, String>>,
}

impl Documents {
    async fn load(state: &AppState, repo: i64, runs: &[db::models::CiRun]) -> Result<Self> {
        let ids: Vec<_> = runs.iter().map(|run| run.id).collect();
        let metadata = action_runs::metadata(&state.pool, repo, &ids).await.map_err(Error::unavailable)?;
        let mut actors: Vec<_> = metadata.iter().filter_map(|row| row.actor_id)
            .chain(runs.iter().filter_map(|run| run.triggered_by_id)).collect();
        actors.sort_unstable();
        actors.dedup();
        let actors = db::users::accounts::get_users_by_ids(&state.pool, &actors).await.map_err(Error::unavailable)?
            .into_iter().map(|user| (user.id, payloads::user(&user))).collect();
        Ok(Self { metadata: metadata.into_iter().map(|row| (row.run_id, row)).collect(), actors, commits: HashMap::new(), workflows: HashMap::new() })
    }
}

async fn referenced_document(state: &AppState, auth: Option<&auth::Authentication>, id: i64) -> Result<Value> {
    let Some(head) = auth::referenced_repository(state, auth, id).await? else { return Ok(Value::Null) };
    let owner = db::repositories::records::repository_owner_name(&state.pool, head.id)
        .await.map_err(Error::unavailable)?;
    Ok(owner.map(|owner| payloads::repository(&head, &owner)).unwrap_or(Value::Null))
}

async fn document(
    state: &Arc<AppState>,
    auth: Option<&auth::Authentication>,
    repo: &db::models::Repository,
    owner: &str,
    run: &db::models::CiRun,
    exclude: bool,
    documents: &mut Documents,
) -> Result<Value> {
    let metadata = documents.metadata.get(&run.id)
        .ok_or_else(|| Error::new(StatusCode::GONE, "Workflow run metadata is unavailable"))?;
    if !run.workflow_run_number.is_some_and(|number| number > 0) {
        return Err(Error::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "Workflow run number is unavailable",
        ));
    }
    let repository = payloads::repository(repo, owner);
    let pr = if let Some(id) = run.pull_request_id {
        db::pull_requests::records::get_pull_request_by_id(&state.pool, id)
            .await
            .map_err(Error::unavailable)?
            .filter(|pr| pr.repo_id == repo.id)
    } else {
        None
    };
    let head_repository = if !run.is_fork {
        repository.clone()
    } else {
        let snapshot = metadata.head_repository.as_ref().map(|value| &value.0);
        let head_id = snapshot
            .map(|head| head.id)
            .or_else(|| pr.as_ref().and_then(|pr| pr.head_repo_id));
        match head_id {
            Some(id) => referenced_document(state, auth, id).await?,
            None => Value::Null,
        }
    };
    let mut head_commit = Value::Null;
    if let Some(sha) = run.head_sha.as_deref() {
        if let Some(commit) = documents.commits.get(sha) {
            head_commit = commit.clone();
        } else {
        match repo_store(repo, state.cluster.as_ref()).commit(sha).await {
            Ok(commit) => {
                let git = payloads::git_commit(&commit, owner, &repo.name);
                let signature = |name: &str| {
                    if git[name]["email"]
                        .as_str()
                        .is_some_and(|email| !email.is_empty())
                    {
                        git[name].clone()
                    } else {
                        Value::Null
                    }
                };
                head_commit = json!({"id":git["sha"],"tree_id":git["tree"]["sha"],"message":git["message"],"timestamp":git["committer"]["date"],"author":signature("author"),"committer":signature("committer")});
            }
            Err(error)
                if matches!(
                    error.kind,
                    crate::backend::repositories::store::ErrorKind::NotFound
                        | crate::backend::repositories::store::ErrorKind::MissingPath
                        | crate::backend::repositories::store::ErrorKind::Unavailable
                ) => {}
            Err(error) => return Err(crate::backend::api::github::contents::store_error(error)),
        }
        documents.commits.insert(sha.to_owned(), head_commit.clone());
        }
    }
    let pull_requests = if exclude {
        json!([])
    } else if let Some(pr) = pr {
        let snapshot = run.pull_request_event.as_ref();
        let event_head = run.event_payload.as_ref().map(|event| &event["pull_request"]["head"]);
        let head_sha = snapshot.and_then(|event| event["head_sha"].as_str())
            .or_else(|| event_head.and_then(|head| head["sha"].as_str()))
            .unwrap_or(if run.event == "pull_request_target" { &pr.head_sha } else { run.head_sha.as_deref().unwrap_or(&pr.head_sha) });
        let mut pr_head_repository = head_repository.clone();
        if run.event == "pull_request_target" {
            pr_head_repository = Value::Null;
            if let Some(id) = snapshot.and_then(|event| event["head_repo_id"].as_i64()).or(pr.head_repo_id) {
                pr_head_repository = referenced_document(state, auth, id).await?;
            }
        }
        let side = |reference: &str, sha: &str, repository: &Value| json!({"ref":reference,"sha":sha,"repo":{"id":repository["id"],"url":repository["url"],"name":repository["name"]}});
        json!([{"id":pr.id,"number":pr.number,"url":format!("{}/pulls/{}",repository["url"].as_str().unwrap_or(""),pr.number),
            "head":side(snapshot.and_then(|event|event["head_ref"].as_str()).unwrap_or(&pr.head_branch),head_sha,&pr_head_repository),
            "base":side(snapshot.and_then(|event|event["base_ref"].as_str()).unwrap_or(&pr.base_branch),snapshot.and_then(|event|event["base_sha"].as_str()).unwrap_or(&pr.base_sha),&repository)}])
    } else if run.event == "pull_request" {
        // The association was deleted; an empty list would falsely claim there never was one.
        Value::Null
    } else {
        json!([])
    };
    let mut result = payloads::action_run(
        run,
        owner,
        repository,
        head_repository,
        head_commit,
        &metadata,
        pull_requests,
    );
    let mut workflow_name = crate::backend::ci::workflow_name_from_path(&run.workflow_path);
    if let Some(sha) = &run.head_sha {
        if !documents.workflows.contains_key(sha) {
            // Discovery owns bounded background parsing and its immutable cross-request cache.
            // Git enrichment is optional; durable execution history remains readable offline.
            let definitions = crate::backend::ci::list_workflows_for_ref(&repo_store(repo, state.cluster.as_ref()), sha).await.unwrap_or_default();
            documents.workflows.insert(sha.clone(), definitions.into_iter().map(|definition| (definition.path, definition.name)).collect());
        }
        if let Some(name) = documents.workflows[sha].get(&run.workflow_path) {
            workflow_name = name.clone();
        }
    }
    result["name"] = json!(workflow_name);
    if let Some(created_at) = metadata.origin_created_at {
        result["created_at"] = json!(created_at);
    }
    if let Some(actor) = metadata.actor_id.and_then(|id| documents.actors.get(&id)) {
        result["actor"] = actor.clone();
    }
    if let Some(actor) = run.triggered_by_id.and_then(|id| documents.actors.get(&id)) {
        result["triggering_actor"] = actor.clone();
    }
    Ok(result)
}

pub(super) async fn get_one(
    State(state): State<Arc<AppState>>,
    Path(params): Path<HashMap<String, String>>,
    Query(parameters): Query<Parameters>,
    headers: HeaderMap,
) -> Result<Json<Value>> {
    let owner = params.get("owner").ok_or_else(Error::missing)?;
    let name = params.get("repo").ok_or_else(Error::missing)?;
    let (repo, auth) = repository(&state, &headers, owner, name, false).await?;
    let attempt = if params.contains_key("attempt_number") {
        Some(i32::try_from(id(&params, "attempt_number")?).map_err(|_| Error::missing())?)
    } else {
        None
    };
    let run = action_runs::get(&state.pool, repo.id, id(&params, "run_id")?, attempt)
        .await
        .map_err(Error::unavailable)?
        .ok_or_else(Error::missing)?;
    let mut documents = Documents::load(&state, repo.id, std::slice::from_ref(&run)).await?;
    Ok(Json(
        document(
            &state,
            auth.as_ref(),
            &repo,
            owner,
            &run,
            parameters.exclude_pull_requests,
            &mut documents,
        )
        .await?,
    ))
}

pub(super) async fn list(
    State(state): State<Arc<AppState>>,
    Path(params): Path<HashMap<String, String>>,
    Query(parameters): Query<Parameters>,
    Query(page_parameters): Query<pagination::Pagination>,
    OriginalUri(uri): OriginalUri,
    headers: HeaderMap,
) -> Result<axum::response::Response> {
    use axum::response::IntoResponse;
    let owner = params.get("owner").ok_or_else(Error::missing)?;
    let name = params.get("repo").ok_or_else(Error::missing)?;
    let (repo, auth) = repository(&state, &headers, owner, name, false).await?;
    let mut q = RunQuery::default();
    if let Some(key) = params.get("workflow_id") {
        q.workflow = super::workflows::lookup_read(&state, &repo, key)
            .await?
            .path;
    }
    q.actor = parameters.actor.unwrap_or_default();
    q.branch = parameters.branch.unwrap_or_default();
    q.event = parameters.event.unwrap_or_default();
    q.sha = parameters.head_sha.unwrap_or_default();
    if let Some(status) = parameters.status.as_deref() {
        if !matches!(
            status,
            "queued"
                | "in_progress"
                | "completed"
                | "success"
                | "failure"
                | "neutral"
                | "cancelled"
                | "skipped"
                | "timed_out"
                | "action_required"
                | "stale"
                | "waiting"
                | "requested"
                | "pending"
                | "startup_failure"
        ) {
            return Err(Error::invalid("Invalid workflow run status"));
        }
        q.statuses = action_runs::native_statuses(status);
        if q.statuses.is_empty() {
            q.statuses.push("__no_native_status__".into());
        }
        q.completed = status == "completed";
    }
    if let Some(created_at) = parameters.created.as_deref() {
        created(created_at, &mut q)?;
    }
    if let Some(check) = parameters.check_suite_id {
        if check < 1 {
            return Err(Error::invalid("Invalid check suite ID"));
        }
        q.check_suite = Some(check);
    }
    if !q.sha.is_empty()
        && (q.sha.len() != 40 || !q.sha.bytes().all(|byte| byte.is_ascii_hexdigit()))
    {
        return Err(Error::invalid("Invalid head SHA"));
    }
    q.sha.make_ascii_lowercase();
    let (offset, limit) = page_parameters.window()?;
    let page = action_runs::list(
        &state.pool,
        auth.as_ref().map(|auth| auth.user.id),
        repo.id,
        &q,
        offset as i64,
        limit as i64,
    )
    .await
    .map_err(Error::unavailable)?
    .ok_or_else(Error::missing)?;
    let mut documents = Documents::load(&state, repo.id, &page.runs).await?;
    let mut runs = Vec::with_capacity(page.runs.len());
    for run in &page.runs {
        runs.push(
            document(
                &state,
                auth.as_ref(),
                &repo,
                owner,
                run,
                parameters.exclude_pull_requests,
                &mut documents,
            )
            .await?,
        );
    }
    let headers = page_parameters.headers(&uri, ((offset + runs.len()) as i64) < page.total)?;
    Ok((
        headers,
        Json(json!({"total_count":page.total,"workflow_runs":runs})),
    )
        .into_response())
}

pub(super) fn handlers() -> Vec<(&'static str, MethodRouter<Arc<AppState>>)> {
    vec![
        ("actions/list-workflow-runs-for-repo", get(list)),
        ("actions/list-workflow-runs", get(list)),
        ("actions/get-workflow-run", get(get_one)),
        ("actions/get-workflow-run-attempt", get(get_one)),
    ]
}
