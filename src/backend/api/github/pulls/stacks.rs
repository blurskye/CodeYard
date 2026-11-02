use super::super::{auth, node_id, pagination::Pagination, repo_url, repository, Error, Result};
use crate::backend::{
    app::AppState,
    db,
    repositories::{
        access,
        pull_requests::{mutations, stacks},
    },
};
use axum::{
    body::Bytes,
    extract::{OriginalUri, Path, Query, State},
    http::{header, HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post, MethodRouter},
    Json,
};
use serde::Deserialize;
use serde_json::{json, Value};
use std::sync::Arc;

async fn metadata(
    state: &AppState,
    repo: i64,
    number: i32,
) -> Result<db::pull_requests::records::StackMetadata> {
    db::pull_requests::records::get_pull_request_stack(&state.pool, repo, number)
        .await
        .map_err(Error::unavailable)?
        .ok_or_else(Error::missing)
}
async fn document(
    state: &AppState,
    headers: &HeaderMap,
    repo: &db::models::Repository,
    owner: &str,
    meta: &db::pull_requests::records::StackMetadata,
    detail: bool,
) -> Result<Value> {
    let members = stacks::order_layers(
        db::pull_requests::records::list_stack_members(&state.pool, repo.id, meta.id)
            .await
            .map_err(Error::unavailable)?,
    );
    let mut pulls = Vec::new();
    for pull in &members {
        pulls.push(if detail{super::document(state,headers,repo,owner,pull,false).await?}else{json!({"number":pull.number,"state":if pull.state=="merged"{"closed"}else{&pull.state},"draft":pull.is_draft,"merged_at":pull.merged_at,"head":{"ref":pull.head_branch,"sha":pull.head_sha}})});
    }
    Ok(
        json!({"id":meta.id,"number":meta.number,"node_id":node_id::encode("S",meta.id),"url":format!("{}/stacks/{}",repo_url(owner,&repo.name),meta.number),"base":{"ref":meta.base_branch},"open":members.iter().any(|p|p.state=="open"),"created_at":meta.created_at,"pull_requests":pulls}),
    )
}
#[derive(Deserialize)]
struct List {
    pull_request: Option<i32>,
    page: Option<usize>,
    per_page: Option<usize>,
}
async fn list(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, name)): Path<(String, String)>,
    Query(query): Query<List>,
    OriginalUri(uri): OriginalUri,
) -> Result<Response> {
    let (repo, _) = repository(&state, &headers, &owner, &name, false).await?;
    if query.pull_request.is_some_and(|n| n <= 0) {
        return Err(Error::invalid("Invalid pull_request"));
    }
    let page = Pagination {
        page: query.page,
        per_page: query.per_page,
    };
    let (offset, limit) = page.window()?;
    let rows = db::pull_requests::records::list_pull_request_stacks(
        &state.pool,
        repo.id,
        query.pull_request,
        (limit + 1) as i64,
        offset as i64,
    )
    .await
    .map_err(Error::unavailable)?;
    let has_next = rows.len() > limit;
    let mut items = Vec::new();
    for meta in rows.iter().take(limit) {
        items.push(document(&state, &headers, &repo, &owner, meta, false).await?);
    }
    page.response(&uri, items, has_next)
}
async fn read(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, name, number)): Path<(String, String, i32)>,
) -> Result<Json<Value>> {
    let (repo, _) = repository(&state, &headers, &owner, &name, false).await?;
    let meta = metadata(&state, repo.id, number).await?;
    Ok(Json(
        document(&state, &headers, &repo, &owner, &meta, true).await?,
    ))
}
#[derive(Deserialize)]
struct Members {
    pull_requests: Vec<i32>,
}
async fn change(
    state: Arc<AppState>,
    headers: HeaderMap,
    namespace: (String, String),
    number: Option<i32>,
    body: Bytes,
) -> Result<Response> {
    let (owner, name) = namespace;
    let (repo, viewer) = repository(&state, &headers, &owner, &name, true).await?;
    let actor = viewer.ok_or_else(auth::required)?.user;
    if !access::can_write(&state.pool, &actor, &repo)
        .await
        .map_err(Error::unavailable)?
    {
        return Err(Error::new(StatusCode::FORBIDDEN, "Stack write denied"));
    }
    let input: Members =
        serde_json::from_slice(&body).map_err(|_| Error::invalid("pull_requests is required"))?;
    if input.pull_requests.len() < if number.is_none() { 2 } else { 1 }
        || input.pull_requests.len() > 100
    {
        return Err(Error::invalid("Invalid stack size"));
    }
    let mut pulls = Vec::new();
    for n in input.pull_requests {
        pulls.push(
            db::pull_requests::records::get_pull_request(&state.pool, repo.id, n)
                .await
                .map_err(Error::unavailable)?
                .ok_or_else(|| Error::invalid("Pull request not found in this repository"))?,
        );
    }
    let root = match number {
        Some(number) => metadata(&state, repo.id, number).await?.id,
        None => pulls[0].id,
    };
    let ids: Vec<i64> = pulls.iter().map(|p| p.id).collect();
    mutations::join_stack_batch(
        &state.pool,
        &state.ci_runs,
        actor.id,
        (owner.clone(), name),
        root,
        &ids,
        number.is_none(),
    )
    .await
    .map_err(super::mutations::operation_error)?;
    let root = db::pull_requests::records::get_pull_request_by_id(&state.pool, root)
        .await
        .map_err(Error::unavailable)?
        .ok_or_else(Error::missing)?;
    let meta = metadata(&state, repo.id, root.number).await?;
    let value = document(&state, &headers, &repo, &owner, &meta, true).await?;
    let location = value["url"]
        .as_str()
        .ok_or_else(|| Error::unavailable(()))?
        .to_owned();
    Ok((
        if number.is_none() {
            StatusCode::CREATED
        } else {
            StatusCode::OK
        },
        [(header::LOCATION, location)],
        Json(value),
    )
        .into_response())
}
async fn create(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(namespace): Path<(String, String)>,
    body: Bytes,
) -> Result<Response> {
    change(state, headers, namespace, None, body).await
}
async fn add(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, name, number)): Path<(String, String, i32)>,
    body: Bytes,
) -> Result<Response> {
    change(state, headers, (owner, name), Some(number), body).await
}
async fn unstack(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, name, number)): Path<(String, String, i32)>,
) -> Result<Response> {
    let (repo, viewer) = repository(&state, &headers, &owner, &name, true).await?;
    let actor = viewer.ok_or_else(auth::required)?.user;
    let meta = metadata(&state, repo.id, number).await?;
    let count = mutations::unstack_named_stack(
        &state.pool,
        &state.ci_runs,
        actor.id,
        (owner.clone(), name),
        meta.id,
    )
    .await
    .map_err(super::mutations::operation_error)?;
    if count == 0 {
        return Err(Error::invalid("Every pull request in this stack is locked"));
    }
    match db::pull_requests::records::get_pull_request_stack(&state.pool, repo.id, number)
        .await
        .map_err(Error::unavailable)?
    {
        None => Ok(StatusCode::NO_CONTENT.into_response()),
        Some(meta) => {
            Ok(Json(document(&state, &headers, &repo, &owner, &meta, true).await?).into_response())
        }
    }
}
pub(super) fn handlers() -> Vec<(&'static str, MethodRouter<Arc<AppState>>)> {
    vec![
        ("pull-request-stacks/list", get(list)),
        ("pull-request-stacks/get", get(read)),
        ("pull-request-stacks/create", post(create)),
        ("pull-request-stacks/add", post(add)),
        ("pull-request-stacks/unstack", post(unstack)),
    ]
}
