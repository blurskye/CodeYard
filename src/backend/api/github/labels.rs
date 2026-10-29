//! Repository labels and their assignments use the same authorized transactions as the forms.
use super::{auth, pagination::Pagination, repository, Error, Result};
use crate::backend::{app::AppState, db, webhooks::payloads};
use axum::{
    extract::{OriginalUri, Path, Query, State},
    http::{HeaderMap, StatusCode},
    response::Response,
    routing::{delete, get, patch, post, put, MethodRouter},
    Json,
};
use serde::Deserialize;
use serde_json::Value;
use std::{collections::HashMap, sync::Arc};

type Params = HashMap<String, String>;

pub(in crate::backend::api) fn write_error(error: anyhow::Error) -> Error {
    use db::labels::LabelWriteRefusal;
    match error.downcast_ref::<LabelWriteRefusal>() {
        Some(LabelWriteRefusal::Denied) => {
            Error::new(StatusCode::FORBIDDEN, "Label mutation denied")
        }
        Some(LabelWriteRefusal::DuplicateName) => {
            Error::invalid("A label with this name already exists")
        }
        Some(LabelWriteRefusal::InvalidLabels) => Error::invalid("Invalid labels"),
        Some(LabelWriteRefusal::Conflict) => Error::new(StatusCode::CONFLICT, "Label changed"),
        None => Error::unavailable(error),
    }
}

async fn find(pool: &db::DbPool, repo_id: i64, name: &str) -> Result<db::models::Label> {
    db::labels::list_repo_labels(pool, repo_id)
        .await
        .map_err(Error::unavailable)?
        .into_iter()
        .find(|label| label.name.eq_ignore_ascii_case(name))
        .ok_or_else(Error::missing)
}

pub(in crate::backend::api) fn color(value: &str) -> Result<String> {
    if value.len() != 6 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(Error::invalid(
            "Color must be a six-character hex code without #",
        ));
    }
    Ok(format!("#{}", value.to_ascii_lowercase()))
}

#[derive(Deserialize)]
struct Create {
    name: String,
    color: Option<String>,
    description: Option<String>,
}

async fn create(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(p): Path<Params>,
    Json(input): Json<Create>,
) -> Result<(StatusCode, Json<Value>)> {
    let (repo, auth) = repository(&state, &headers, &p["owner"], &p["repo"], true).await?;
    let actor = auth.ok_or_else(auth::required)?.user;
    crate::backend::issues::validate_label(&input.name, input.description.as_deref())
        .map_err(Error::invalid)?;
    let color = color(input.color.as_deref().unwrap_or("ededed"))?;
    let label = db::labels::create_label(
        &state.pool,
        actor.id,
        repo.id,
        &input.name,
        &color,
        input.description.as_deref(),
    )
    .await
    .map_err(write_error)?;
    Ok((
        StatusCode::CREATED,
        Json(payloads::label(&label, &p["owner"], &repo.name)),
    ))
}

async fn list(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(p): Path<Params>,
    Query(page): Query<Pagination>,
    OriginalUri(uri): OriginalUri,
) -> Result<Response> {
    let (repo, _) = repository(&state, &headers, &p["owner"], &p["repo"], false).await?;
    let (offset, limit) = page.window()?;
    let labels = db::labels::list_repo_labels_page(
        &state.pool,
        repo.id,
        "",
        "",
        (limit + 1) as i64,
        offset as i64,
    )
    .await
    .map_err(Error::unavailable)?;
    let next = labels.len() > limit;
    page.response(
        &uri,
        labels
            .iter()
            .map(|label| payloads::label(label, &p["owner"], &repo.name))
            .collect(),
        next,
    )
}

async fn get_label(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(p): Path<Params>,
) -> Result<Json<Value>> {
    let (repo, _) = repository(&state, &headers, &p["owner"], &p["repo"], false).await?;
    let label = find(&state.pool, repo.id, &p["name"]).await?;
    Ok(Json(payloads::label(&label, &p["owner"], &repo.name)))
}

#[derive(Deserialize)]
struct Update {
    new_name: Option<String>,
    color: Option<String>,
    description: Option<String>,
}

async fn update(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(p): Path<Params>,
    Json(input): Json<Update>,
) -> Result<Json<Value>> {
    let (repo, auth) = repository(&state, &headers, &p["owner"], &p["repo"], true).await?;
    let actor = auth.ok_or_else(auth::required)?.user;
    let existing = find(&state.pool, repo.id, &p["name"]).await?;
    crate::backend::issues::validate_label(
        input.new_name.as_deref().unwrap_or(&existing.name),
        input.description.as_deref(),
    )
    .map_err(Error::invalid)?;
    let color = input.color.as_deref().map(color).transpose()?;
    let label = db::labels::update_label(
        &state.pool,
        actor.id,
        repo.id,
        existing.id,
        input.new_name.as_deref(),
        color.as_deref(),
        input.description.as_deref(),
    )
    .await
    .map_err(write_error)?;
    Ok(Json(payloads::label(&label, &p["owner"], &repo.name)))
}

async fn remove(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(p): Path<Params>,
) -> Result<StatusCode> {
    let (repo, auth) = repository(&state, &headers, &p["owner"], &p["repo"], true).await?;
    let actor = auth.ok_or_else(auth::required)?.user;
    let label = find(&state.pool, repo.id, &p["name"]).await?;
    db::labels::delete_label(&state.pool, actor.id, repo.id, label.id)
        .await
        .map_err(write_error)?;
    Ok(StatusCode::NO_CONTENT)
}

pub(super) async fn item(
    pool: &db::DbPool,
    repo_id: i64,
    number: &str,
) -> Result<db::assignees::AssignedItem> {
    let number = number.parse().map_err(|_| Error::missing())?;
    if let Some(issue) = db::issues::get_issue(pool, repo_id, number)
        .await
        .map_err(Error::unavailable)?
    {
        return Ok(db::assignees::AssignedItem::Issue(issue.id));
    }
    let pull = db::pull_requests::records::get_pull_request(pool, repo_id, number)
        .await
        .map_err(Error::unavailable)?
        .ok_or_else(Error::missing)?;
    Ok(db::assignees::AssignedItem::PullRequest(pull.id))
}

async fn assigned(
    pool: &db::DbPool,
    item: db::assignees::AssignedItem,
) -> Result<Vec<db::models::Label>> {
    match item {
        db::assignees::AssignedItem::Issue(id) => db::labels::get_issue_labels(pool, id).await,
        db::assignees::AssignedItem::PullRequest(id) => {
            db::labels::get_pull_request_labels(pool, id).await
        }
    }
    .map_err(Error::unavailable)
}

async fn list_on_issue(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(p): Path<Params>,
    Query(page): Query<Pagination>,
    OriginalUri(uri): OriginalUri,
) -> Result<Response> {
    let (repo, _) = repository(&state, &headers, &p["owner"], &p["repo"], false).await?;
    let item = item(&state.pool, repo.id, &p["issue_number"]).await?;
    let labels = assigned(&state.pool, item).await?;
    let (offset, limit) = page.window()?;
    let next = labels.len() > offset + limit;
    page.response(
        &uri,
        labels
            .iter()
            .skip(offset)
            .take(limit)
            .map(|label| payloads::label(label, &p["owner"], &repo.name))
            .collect(),
        next,
    )
}

#[derive(Deserialize)]
#[serde(untagged)]
enum Names {
    Object { labels: Vec<String> },
    Array(Vec<String>),
}

async fn change(
    state: Arc<AppState>,
    headers: HeaderMap,
    p: Params,
    names: Vec<String>,
    replace: bool,
) -> Result<Json<Vec<Value>>> {
    let (repo, auth) = repository(&state, &headers, &p["owner"], &p["repo"], true).await?;
    let actor = auth.ok_or_else(auth::required)?.user;
    let item = item(&state.pool, repo.id, &p["issue_number"]).await?;
    if names.len() > db::labels::MAX_ISSUE_LABELS {
        return Err(Error::invalid("Too many labels"));
    }
    let mut ids = Vec::new();
    for name in names {
        ids.push(
            find(&state.pool, repo.id, &name)
                .await
                .map_err(|error| {
                    if error.0 == StatusCode::NOT_FOUND {
                        Error::invalid("Label does not exist")
                    } else {
                        error
                    }
                })?
                .id,
        );
    }
    let labels = save_labels(&state.pool, actor.id, repo.id, item, &ids, replace).await?;
    Ok(Json(
        labels
            .iter()
            .map(|label| payloads::label(label, &p["owner"], &repo.name))
            .collect(),
    ))
}

pub(in crate::backend::api) async fn save_labels(
    pool: &db::DbPool,
    actor_id: i64,
    repo_id: i64,
    item: db::assignees::AssignedItem,
    ids: &[i64],
    replace: bool,
) -> Result<Vec<db::models::Label>> {
    let before = assigned(pool, item).await?;
    match (item, replace) {
        (db::assignees::AssignedItem::Issue(id), true) => {
            db::labels::set_issue_labels(pool, actor_id, repo_id, id, ids).await
        }
        (db::assignees::AssignedItem::PullRequest(id), true) => {
            db::labels::set_pull_request_labels(pool, actor_id, repo_id, id, ids).await
        }
        (db::assignees::AssignedItem::Issue(id), false) => {
            db::labels::add_issue_labels(pool, actor_id, repo_id, id, ids)
                .await
                .map(|_| ())
        }
        (db::assignees::AssignedItem::PullRequest(id), false) => {
            db::labels::add_pull_request_labels(pool, actor_id, repo_id, id, ids)
                .await
                .map(|_| ())
        }
    }
    .map_err(write_error)?;
    let after = assigned(pool, item).await?;
    for label in before
        .iter()
        .filter(|label| !after.iter().any(|current| current.id == label.id))
    {
        db::timeline::record_for(pool, repo_id, item, actor_id, "unlabeled", &label.name).await;
    }
    for label in after
        .iter()
        .filter(|label| !before.iter().any(|previous| previous.id == label.id))
    {
        db::timeline::record_for(pool, repo_id, item, actor_id, "labeled", &label.name).await;
    }
    Ok(after)
}

async fn add(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(p): Path<Params>,
    Json(names): Json<Names>,
) -> Result<Json<Vec<Value>>> {
    let (Names::Object { labels } | Names::Array(labels)) = names;
    change(state, headers, p, labels, false).await
}

async fn set(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(p): Path<Params>,
    Json(names): Json<Names>,
) -> Result<Json<Vec<Value>>> {
    let (Names::Object { labels } | Names::Array(labels)) = names;
    change(state, headers, p, labels, true).await
}

async fn clear(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(p): Path<Params>,
) -> Result<StatusCode> {
    let _ = change(state, headers, p, Vec::new(), true).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn remove_from_issue(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(p): Path<Params>,
) -> Result<Json<Vec<Value>>> {
    let (repo, auth) = repository(&state, &headers, &p["owner"], &p["repo"], true).await?;
    let actor = auth.ok_or_else(auth::required)?.user;
    let item = item(&state.pool, repo.id, &p["issue_number"]).await?;
    let label = find(&state.pool, repo.id, &p["name"]).await?;
    let removed = match item {
        db::assignees::AssignedItem::Issue(id) => {
            db::labels::remove_issue_label(&state.pool, actor.id, repo.id, id, label.id).await
        }
        db::assignees::AssignedItem::PullRequest(id) => {
            db::labels::remove_pull_request_label(&state.pool, actor.id, repo.id, id, label.id)
                .await
        }
    }
    .map_err(write_error)?;
    if !removed {
        return Err(Error::missing());
    }
    db::timeline::record_for(
        &state.pool,
        repo.id,
        item,
        actor.id,
        "unlabeled",
        &label.name,
    )
    .await;
    Ok(Json(
        assigned(&state.pool, item)
            .await?
            .iter()
            .map(|label| payloads::label(label, &p["owner"], &repo.name))
            .collect(),
    ))
}

pub(super) fn handlers() -> Vec<(&'static str, MethodRouter<Arc<AppState>>)> {
    vec![
        ("issues/create-label", post(create)),
        ("issues/list-labels-for-repo", get(list)),
        ("issues/get-label", get(get_label)),
        ("issues/update-label", patch(update)),
        ("issues/delete-label", delete(remove)),
        ("issues/list-labels-on-issue", get(list_on_issue)),
        ("issues/add-labels", post(add)),
        ("issues/set-labels", put(set)),
        ("issues/remove-all-labels", delete(clear)),
        ("issues/remove-label", delete(remove_from_issue)),
    ]
}
