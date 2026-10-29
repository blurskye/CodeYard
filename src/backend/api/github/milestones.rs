use super::{auth, pagination::Pagination, repository, Error, Result};
use crate::backend::{
    app::AppState,
    db,
    db::models::{Milestone, MilestoneSummary},
    webhooks::payloads,
};
use axum::{
    extract::{OriginalUri, Path, Query, State},
    http::{HeaderMap, StatusCode},
    response::Response,
    routing::{delete, get, patch, post, MethodRouter},
    Json,
};
use chrono::{DateTime, Utc};
use serde::Deserialize;
use serde_json::Value;
use std::{collections::HashMap, sync::Arc};

type Params = HashMap<String, String>;

fn write_error(error: anyhow::Error) -> Error {
    use db::milestones::MilestoneWriteRefusal;
    match error.downcast_ref::<MilestoneWriteRefusal>() {
        Some(MilestoneWriteRefusal::Denied) => {
            Error::new(StatusCode::FORBIDDEN, "Milestone mutation denied")
        }
        Some(MilestoneWriteRefusal::NotFound) => Error::missing(),
        Some(MilestoneWriteRefusal::Conflict) => Error::new(StatusCode::CONFLICT, "Milestone changed"),
        Some(MilestoneWriteRefusal::DuplicateTitle) => {
            Error::invalid("A milestone with this title already exists")
        }
        None => Error::unavailable(error),
    }
}

pub(super) async fn document(
    pool: &db::DbPool,
    summary: &MilestoneSummary,
    owner: &str,
    repo: &str,
) -> Result<Value> {
    let creator = match summary.milestone.creator_id {
        Some(id) => db::users::accounts::get_user_by_id(pool, id)
            .await
            .map_err(Error::unavailable)?,
        None => None,
    };
    Ok(payloads::milestone(summary, creator.as_ref(), owner, repo))
}

async fn find(pool: &db::DbPool, repo_id: i64, number: &str) -> Result<MilestoneSummary> {
    db::milestones::get_milestone(pool, repo_id, number.parse().map_err(|_| Error::missing())?)
        .await
        .map_err(Error::unavailable)?
        .ok_or_else(Error::missing)
}

async fn get_milestone(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(p): Path<Params>,
) -> Result<Json<Value>> {
    let (repo, _) = repository(&state, &headers, &p["owner"], &p["repo"], false).await?;
    let summary = find(&state.pool, repo.id, &p["milestone_number"]).await?;
    Ok(Json(
        document(&state.pool, &summary, &p["owner"], &repo.name).await?,
    ))
}

#[derive(Deserialize)]
struct Input {
    title: Option<String>,
    description: Option<String>,
    due_on: Option<DateTime<Utc>>,
    state: Option<String>,
}

async fn save(
    state: Arc<AppState>,
    headers: HeaderMap,
    p: Params,
    value: Value,
    creating: bool,
) -> Result<(StatusCode, Json<Value>)> {
    let (repo, auth) = repository(&state, &headers, &p["owner"], &p["repo"], true).await?;
    let actor = auth.ok_or_else(auth::required)?.user;
    let input: Input = serde_json::from_value(value.clone())
        .map_err(|_| Error::invalid("Invalid milestone fields"))?;
    let existing: Option<Milestone> = if creating {
        None
    } else {
        Some(
            find(&state.pool, repo.id, &p["milestone_number"])
                .await?
                .milestone,
        )
    };
    let title = input
        .title
        .as_deref()
        .or_else(|| existing.as_ref().map(|milestone| milestone.title.as_str()))
        .ok_or_else(|| Error::invalid("Title is required"))?;
    let description = if value.get("description").is_some() {
        input.description.as_deref()
    } else {
        existing
            .as_ref()
            .and_then(|milestone| milestone.description.as_deref())
    };
    let due_on = if value.get("due_on").is_some() {
        input.due_on
    } else {
        existing.as_ref().and_then(|milestone| milestone.due_on)
    };
    crate::backend::issues::validate_milestone(title, description.unwrap_or(""))
        .map_err(Error::invalid)?;
    if input
        .state
        .as_deref()
        .is_some_and(|state| !matches!(state, "open" | "closed"))
    {
        return Err(Error::invalid("State must be open or closed"));
    }
    let milestone = db::milestones::save_milestone(
        &state.pool,
        actor.id,
        repo.id,
        existing.as_ref().map(|milestone| milestone.number),
        db::milestones::MilestoneInput {
            title,
            description,
            due_on,
            state: input.state.as_deref(),
        },
    )
    .await
    .map_err(write_error)?;
    let summary = db::milestones::get_milestone(&state.pool, repo.id, milestone.number)
        .await
        .map_err(Error::unavailable)?
        .ok_or_else(Error::missing)?;
    Ok((
        if creating {
            StatusCode::CREATED
        } else {
            StatusCode::OK
        },
        Json(document(&state.pool, &summary, &p["owner"], &repo.name).await?),
    ))
}

async fn create(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(p): Path<Params>,
    Json(input): Json<Value>,
) -> Result<(StatusCode, Json<Value>)> {
    save(state, headers, p, input, true).await
}
async fn update(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(p): Path<Params>,
    Json(input): Json<Value>,
) -> Result<(StatusCode, Json<Value>)> {
    save(state, headers, p, input, false).await
}

async fn remove(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(p): Path<Params>,
) -> Result<StatusCode> {
    let (repo, auth) = repository(&state, &headers, &p["owner"], &p["repo"], true).await?;
    let actor = auth.ok_or_else(auth::required)?.user;
    let summary = find(&state.pool, repo.id, &p["milestone_number"]).await?;
    db::milestones::delete_milestone(
        &state.pool,
        actor.id,
        repo.id,
        summary.milestone.number,
        summary.milestone.id,
    )
    .await
    .map_err(write_error)?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
struct List {
    state: Option<String>,
    sort: Option<String>,
    direction: Option<String>,
}

async fn list(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(p): Path<Params>,
    Query(input): Query<List>,
    Query(page): Query<Pagination>,
    OriginalUri(uri): OriginalUri,
) -> Result<Response> {
    let (repo, _) = repository(&state, &headers, &p["owner"], &p["repo"], false).await?;
    let filter_state = match input.state.as_deref().unwrap_or("open") {
        "all" => None,
        state @ ("open" | "closed") => Some(state),
        _ => return Err(Error::invalid("Invalid state")),
    };
    let ascending = match input.direction.as_deref().unwrap_or("asc") {
        "asc" => true,
        "desc" => false,
        _ => return Err(Error::invalid("Invalid direction")),
    };
    let sort = match (input.sort.as_deref().unwrap_or("due_on"), ascending) {
        ("due_on", true) => "due-on-asc",
        ("due_on", false) => "due-on-desc",
        ("completeness", true) => "least-complete",
        ("completeness", false) => "most-complete",
        _ => return Err(Error::invalid("Invalid sort")),
    };
    let (offset, limit) = page.window()?;
    let rows = db::milestones::list_milestones_page(
        &state.pool,
        repo.id,
        filter_state,
        sort,
        (limit + 1) as i64,
        offset as i64,
    )
    .await
    .map_err(Error::unavailable)?;
    let next = rows.len() > limit;
    let mut items = Vec::new();
    for row in rows {
        items.push(document(&state.pool, &row, &p["owner"], &repo.name).await?);
    }
    page.response(&uri, items, next)
}

async fn labels(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(p): Path<Params>,
    Query(page): Query<Pagination>,
    OriginalUri(uri): OriginalUri,
) -> Result<Response> {
    let (repo, _) = repository(&state, &headers, &p["owner"], &p["repo"], false).await?;
    let milestone = find(&state.pool, repo.id, &p["milestone_number"]).await?;
    let (offset, limit) = page.window()?;
    let labels = db::labels::list_milestone_labels(
        &state.pool,
        repo.id,
        milestone.milestone.id,
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

pub(super) fn handlers() -> Vec<(&'static str, MethodRouter<Arc<AppState>>)> {
    vec![
        ("issues/create-milestone", post(create)),
        ("issues/update-milestone", patch(update)),
        ("issues/delete-milestone", delete(remove)),
        ("issues/get-milestone", get(get_milestone)),
        ("issues/list-milestones", get(list)),
        ("issues/list-labels-for-milestone", get(labels)),
    ]
}
