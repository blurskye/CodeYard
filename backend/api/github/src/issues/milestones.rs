use axum::extract::OriginalUri;
use axum::extract::Path;
use axum::extract::Query;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::http::StatusCode;
use axum::response::Response;
use axum::routing::delete;
use axum::routing::get;
use axum::routing::patch;
use axum::routing::post;
use axum::routing::MethodRouter;
use axum::Json;
use chrono::DateTime;
use chrono::Utc;
use codeyard_api_http::shared::github::pagination::Pagination;
use codeyard_app::api_shared::github::repository;
use codeyard_app::app::AppState;
use codeyard_database::models::MilestoneSummary;
use codeyard_types::github_error::Error;
use codeyard_types::github_error::Result;
use serde::Deserialize;
use serde_json::Value;
use std::collections::HashMap;
use std::sync::Arc;

type Params = HashMap<String, String>;

fn write_error(error: anyhow::Error) -> Error {
    use codeyard_db_repos::milestones::MilestoneWriteRefusal;
    match error.downcast_ref::<MilestoneWriteRefusal>() {
        Some(MilestoneWriteRefusal::Denied) => {
            Error::new(StatusCode::FORBIDDEN, "Milestone mutation denied")
        }
        Some(MilestoneWriteRefusal::NotFound) => Error::missing(),
        Some(MilestoneWriteRefusal::Conflict) => {
            Error::new(StatusCode::CONFLICT, "Milestone changed")
        }
        Some(MilestoneWriteRefusal::DuplicateTitle) => {
            Error::invalid("A milestone with this title already exists")
        }
        None => Error::unavailable(error),
    }
}

pub(crate) async fn document(
    pool: &codeyard_database::DbPool,
    summary: &MilestoneSummary,
    owner: &str,
    repo: &str,
) -> Result<Value> {
    let creator = match summary.milestone.creator_id {
        Some(id) => codeyard_db_identity::users::accounts::get_user_by_id(pool, id)
            .await
            .map_err(Error::unavailable)?,
        None => None,
    };
    Ok(codeyard_webhooks::payloads::milestone(
        summary,
        creator.as_ref(),
        owner,
        repo,
    ))
}

async fn find(
    pool: &codeyard_database::DbPool,
    repo_id: i64,
    number: &str,
) -> Result<MilestoneSummary> {
    codeyard_db_repos::milestones::get_milestone(
        pool,
        repo_id,
        number.parse().map_err(|_| Error::missing())?,
    )
    .await
    .map_err(Error::unavailable)?
    .ok_or_else(Error::missing)
}

async fn get_milestone(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(p): Path<Params>,
) -> Result<Json<Value>> {
    let (repo, auth) = repository(&state, &headers, &p["owner"], &p["repo"], false).await?;
    let summary = codeyard_db_repos::milestones::read_milestones(
        &state.pool,
        auth.as_ref().map(|auth| auth.user.id),
        repo.id,
        Some(
            p["milestone_number"]
                .parse()
                .map_err(|_| Error::missing())?,
        ),
        None,
        "number-asc",
        1,
        0,
    )
    .await
    .map_err(Error::unavailable)?
    .pop()
    .ok_or_else(Error::missing)?;
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
    let actor = auth.ok_or_else(codeyard_identity::github_auth::required)?;
    let input: Input = serde_json::from_value(value.clone())
        .map_err(|_| Error::invalid("Invalid milestone fields"))?;
    let number = if creating {
        None
    } else {
        Some(
            p["milestone_number"]
                .parse::<i32>()
                .map_err(|_| Error::missing())?,
        )
    };
    if creating && input.title.is_none() {
        return Err(Error::invalid("Title is required"));
    }
    if let Some(title) = input.title.as_deref() {
        codeyard_types::issue_input::validate_milestone(title, "").map_err(Error::invalid)?;
    }
    if let Some(description) = input.description.as_deref() {
        codeyard_types::issue_input::validate_milestone("title", description)
            .map_err(Error::invalid)?;
    }
    if input
        .state
        .as_deref()
        .is_some_and(|state| !matches!(state, "open" | "closed"))
    {
        return Err(Error::invalid("State must be open or closed"));
    }
    let milestone = codeyard_db_repos::milestones::save_milestone(
        &state.pool,
        actor.database_actor(),
        repo.id,
        number,
        codeyard_db_repos::milestones::MilestoneInput {
            title: input.title.as_deref(),
            description: value
                .get("description")
                .map(|_| input.description.as_deref()),
            due_on: value.get("due_on").map(|_| input.due_on),
            state: input.state.as_deref(),
        },
    )
    .await
    .map_err(write_error)?;
    let summary =
        codeyard_db_repos::milestones::get_milestone(&state.pool, repo.id, milestone.number)
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
    let actor = auth.ok_or_else(codeyard_identity::github_auth::required)?;
    let summary = find(&state.pool, repo.id, &p["milestone_number"]).await?;
    codeyard_db_repos::milestones::delete_milestone(
        &state.pool,
        actor.database_actor(),
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
    let (repo, auth) = repository(&state, &headers, &p["owner"], &p["repo"], false).await?;
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
    let rows = codeyard_db_repos::milestones::read_milestones(
        &state.pool,
        auth.as_ref().map(|auth| auth.user.id),
        repo.id,
        None,
        filter_state,
        sort,
        (limit + 1) as i64,
        offset as i64,
    )
    .await
    .map_err(Error::unavailable)?;
    let next = rows.len() > limit;
    let rows: Vec<_> = rows.into_iter().take(limit).collect();
    let ids: Vec<_> = rows
        .iter()
        .filter_map(|row| row.milestone.creator_id)
        .collect::<std::collections::HashSet<_>>()
        .into_iter()
        .collect();
    let creators: HashMap<_, _> =
        codeyard_db_identity::users::accounts::get_users_by_ids(&state.pool, &ids)
            .await
            .map_err(Error::unavailable)?
            .into_iter()
            .map(|user| (user.id, user))
            .collect();
    let items = rows
        .iter()
        .map(|row| {
            codeyard_webhooks::payloads::milestone(
                row,
                row.milestone.creator_id.and_then(|id| creators.get(&id)),
                &p["owner"],
                &repo.name,
            )
        })
        .collect();
    page.response(&uri, items, next)
}

async fn labels(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(p): Path<Params>,
    Query(page): Query<Pagination>,
    OriginalUri(uri): OriginalUri,
) -> Result<Response> {
    let (repo, auth) = repository(&state, &headers, &p["owner"], &p["repo"], false).await?;
    let milestone = codeyard_db_repos::milestones::read_milestones(
        &state.pool,
        auth.as_ref().map(|auth| auth.user.id),
        repo.id,
        Some(
            p["milestone_number"]
                .parse()
                .map_err(|_| Error::missing())?,
        ),
        None,
        "number-asc",
        1,
        0,
    )
    .await
    .map_err(Error::unavailable)?
    .pop()
    .ok_or_else(Error::missing)?;
    let (offset, limit) = page.window()?;
    let labels = codeyard_db_repos::labels::list_milestone_labels(
        &state.pool,
        auth.as_ref().map(|auth| auth.user.id),
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
            .map(|label| codeyard_webhooks::payloads::label(label, &p["owner"], &repo.name))
            .collect(),
        next,
    )
}

pub(crate) fn handlers() -> Vec<(&'static str, MethodRouter<Arc<AppState>>)> {
    vec![
        ("issues/create-milestone", post(create)),
        ("issues/update-milestone", patch(update)),
        ("issues/delete-milestone", delete(remove)),
        ("issues/get-milestone", get(get_milestone)),
        ("issues/list-milestones", get(list)),
        ("issues/list-labels-for-milestone", get(labels)),
    ]
}
