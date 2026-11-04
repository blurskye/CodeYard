//! Repository labels and their assignments use the same authorized transactions as the forms.

use crate::backend::api::github::pagination::Pagination;
use crate::backend::api::github::repository;
use crate::backend::api::github::Error;
use crate::backend::api::github::Result;
use crate::backend::app::AppState;

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
    use codeyard_db_repos::labels::LabelWriteRefusal;
    match error.downcast_ref::<LabelWriteRefusal>() {
        Some(LabelWriteRefusal::Denied) => {
            Error::new(StatusCode::FORBIDDEN, "Label mutation denied")
        }
        // gh falls back to an update only on this error item.
        Some(LabelWriteRefusal::DuplicateName) => {
            Error::validation("Label", "name", "already_exists")
        }
        Some(LabelWriteRefusal::InvalidLabels) => Error::invalid("Invalid labels"),
        Some(LabelWriteRefusal::Conflict) => Error::new(StatusCode::CONFLICT, "Label changed"),
        None => Error::unavailable(error),
    }
}

async fn find(
    pool: &codeyard_db_core::DbPool,
    repo_id: i64,
    name: &str,
) -> Result<codeyard_db_core::models::Label> {
    codeyard_db_repos::labels::list_repo_labels(pool, repo_id)
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
    let actor = auth
        .ok_or_else(crate::backend::api::github::auth::required)?
        .database_actor();
    codeyard_model::issue_input::validate_label(&input.name, input.description.as_deref())
        .map_err(Error::invalid)?;
    let color = color(input.color.as_deref().unwrap_or("ededed"))?;
    let label = codeyard_db_repos::labels::create_label(
        &state.pool,
        actor,
        repo.id,
        &input.name,
        &color,
        input.description.as_deref(),
    )
    .await
    .map_err(write_error)?;
    Ok((
        StatusCode::CREATED,
        Json(crate::backend::webhooks::payloads::label(
            &label,
            &p["owner"],
            &repo.name,
        )),
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
    let labels = codeyard_db_repos::labels::list_repo_labels_page(
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
            .map(|label| crate::backend::webhooks::payloads::label(label, &p["owner"], &repo.name))
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
    Ok(Json(crate::backend::webhooks::payloads::label(
        &label,
        &p["owner"],
        &repo.name,
    )))
}

#[derive(Deserialize)]
struct Update {
    new_name: Option<String>,
    color: Option<String>,
    /// Left out keeps the description; `null` clears it.
    #[serde(
        default,
        deserialize_with = "codeyard_db_repos::repositories::records::nullable_change"
    )]
    description: Option<Option<String>>,
}

async fn update(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(p): Path<Params>,
    Json(input): Json<Update>,
) -> Result<Json<Value>> {
    let (repo, auth) = repository(&state, &headers, &p["owner"], &p["repo"], true).await?;
    let actor = auth
        .ok_or_else(crate::backend::api::github::auth::required)?
        .database_actor();
    let existing = find(&state.pool, repo.id, &p["name"]).await?;
    codeyard_model::issue_input::validate_label(
        input.new_name.as_deref().unwrap_or(&existing.name),
        input.description.as_ref().and_then(Option::as_deref),
    )
    .map_err(Error::invalid)?;
    let color = input.color.as_deref().map(color).transpose()?;
    let label = codeyard_db_repos::labels::update_label(
        &state.pool,
        actor,
        repo.id,
        existing.id,
        input.new_name.as_deref(),
        color.as_deref(),
        input.description.as_ref().map(Option::as_deref),
    )
    .await
    .map_err(write_error)?;
    Ok(Json(crate::backend::webhooks::payloads::label(
        &label,
        &p["owner"],
        &repo.name,
    )))
}

async fn remove(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(p): Path<Params>,
) -> Result<StatusCode> {
    let (repo, auth) = repository(&state, &headers, &p["owner"], &p["repo"], true).await?;
    let actor = auth
        .ok_or_else(crate::backend::api::github::auth::required)?
        .database_actor();
    let label = find(&state.pool, repo.id, &p["name"]).await?;
    codeyard_db_repos::labels::delete_label(&state.pool, actor, repo.id, label.id)
        .await
        .map_err(write_error)?;
    Ok(StatusCode::NO_CONTENT)
}

pub(super) async fn item(
    pool: &codeyard_db_core::DbPool,
    repo_id: i64,
    number: &str,
) -> Result<codeyard_db_repos::assignees::AssignedItem> {
    let number = number.parse().map_err(|_| Error::missing())?;
    if let Some(issue) = codeyard_db_repos::issues::get_issue(pool, repo_id, number)
        .await
        .map_err(Error::unavailable)?
    {
        return Ok(codeyard_db_repos::assignees::AssignedItem::Issue(issue.id));
    }
    let pull = codeyard_db_repos::pull_requests::records::get_pull_request(pool, repo_id, number)
        .await
        .map_err(Error::unavailable)?
        .ok_or_else(Error::missing)?;
    Ok(codeyard_db_repos::assignees::AssignedItem::PullRequest(
        pull.id,
    ))
}

async fn assigned(
    pool: &codeyard_db_core::DbPool,
    repo_id: i64,
    item: codeyard_db_repos::assignees::AssignedItem,
) -> Result<Vec<codeyard_db_core::models::Label>> {
    match item {
        codeyard_db_repos::assignees::AssignedItem::Issue(id) => {
            codeyard_db_repos::labels::get_issue_labels(pool, repo_id, id).await
        }
        codeyard_db_repos::assignees::AssignedItem::PullRequest(id) => {
            codeyard_db_repos::labels::get_pull_request_labels(pool, repo_id, id).await
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
    let labels = assigned(&state.pool, repo.id, item).await?;
    let (offset, limit) = page.window()?;
    let next = labels.len() > offset + limit;
    page.response(
        &uri,
        labels
            .iter()
            .skip(offset)
            .take(limit)
            .map(|label| crate::backend::webhooks::payloads::label(label, &p["owner"], &repo.name))
            .collect(),
        next,
    )
}

#[derive(Deserialize)]
#[serde(untagged)]
enum Names {
    Object { labels: Vec<LabelName> },
    Array(Vec<LabelName>),
}

#[derive(Deserialize)]
#[serde(untagged)]
enum LabelName {
    String(String),
    Object { name: String },
}

async fn change(
    state: Arc<AppState>,
    headers: HeaderMap,
    p: Params,
    names: Vec<LabelName>,
    replace: bool,
) -> Result<Json<Vec<Value>>> {
    let (repo, auth) = repository(&state, &headers, &p["owner"], &p["repo"], true).await?;
    let actor = auth
        .ok_or_else(crate::backend::api::github::auth::required)?
        .database_actor();
    let item = item(&state.pool, repo.id, &p["issue_number"]).await?;
    if names.len() > codeyard_db_repos::labels::MAX_ISSUE_LABELS {
        return Err(Error::invalid("Too many labels"));
    }
    let catalog = codeyard_db_repos::labels::list_repo_labels(&state.pool, repo.id)
        .await
        .map_err(Error::unavailable)?;
    let ids = names
        .into_iter()
        .map(|entry| {
            let (LabelName::String(name) | LabelName::Object { name }) = entry;
            catalog
                .iter()
                .find(|label| label.name.eq_ignore_ascii_case(&name))
                .map(|label| label.id)
                .ok_or_else(|| Error::invalid("Label does not exist"))
        })
        .collect::<Result<Vec<_>>>()?;
    let labels = save_labels(&state.pool, actor, repo.id, item, &ids, replace).await?;
    Ok(Json(
        labels
            .iter()
            .map(|label| crate::backend::webhooks::payloads::label(label, &p["owner"], &repo.name))
            .collect(),
    ))
}

pub(in crate::backend::api) async fn save_labels(
    pool: &codeyard_db_core::DbPool,
    actor: impl Into<codeyard_db_core::queries::credentials::Actor>,
    repo_id: i64,
    item: codeyard_db_repos::assignees::AssignedItem,
    ids: &[i64],
    replace: bool,
) -> Result<Vec<codeyard_db_core::models::Label>> {
    match (item, replace) {
        (codeyard_db_repos::assignees::AssignedItem::Issue(id), true) => {
            codeyard_db_repos::labels::set_issue_labels(pool, actor, repo_id, id, ids).await
        }
        (codeyard_db_repos::assignees::AssignedItem::PullRequest(id), true) => {
            codeyard_db_repos::labels::set_pull_request_labels(pool, actor, repo_id, id, ids).await
        }
        (codeyard_db_repos::assignees::AssignedItem::Issue(id), false) => {
            codeyard_db_repos::labels::add_issue_labels(pool, actor, repo_id, id, ids)
                .await
                .map(|_| ())
        }
        (codeyard_db_repos::assignees::AssignedItem::PullRequest(id), false) => {
            codeyard_db_repos::labels::add_pull_request_labels(pool, actor, repo_id, id, ids)
                .await
                .map(|_| ())
        }
    }
    .map_err(write_error)?;
    assigned(pool, repo_id, item).await
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
    let actor = auth
        .ok_or_else(crate::backend::api::github::auth::required)?
        .database_actor();
    let item = item(&state.pool, repo.id, &p["issue_number"]).await?;
    let label = find(&state.pool, repo.id, &p["name"]).await?;
    let removed = match item {
        codeyard_db_repos::assignees::AssignedItem::Issue(id) => {
            codeyard_db_repos::labels::remove_issue_label(&state.pool, actor, repo.id, id, label.id)
                .await
        }
        codeyard_db_repos::assignees::AssignedItem::PullRequest(id) => {
            codeyard_db_repos::labels::remove_pull_request_label(
                &state.pool,
                actor,
                repo.id,
                id,
                label.id,
            )
            .await
        }
    }
    .map_err(write_error)?;
    if !removed {
        return Err(Error::missing());
    }
    Ok(Json(
        assigned(&state.pool, repo.id, item)
            .await?
            .iter()
            .map(|label| crate::backend::webhooks::payloads::label(label, &p["owner"], &repo.name))
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
