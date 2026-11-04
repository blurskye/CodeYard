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
use codeyard_api_http::shared::github::pagination::Pagination;
use codeyard_app::api_shared::github::repository;
use codeyard_app::app::AppState;
use codeyard_db_repos::assignees::AssignedItem;
use codeyard_model::github_error::Error;
use codeyard_model::github_error::Result;
use serde_json::Value;
use std::collections::HashMap;
use std::sync::Arc;

/// The documents of items in several repositories, in the order given. Items of one repository
/// are read together.
pub(super) async fn documents_across(
    pool: &codeyard_db_core::DbPool,
    reader: Reader<'_>,
    items: &[(&codeyard_db_core::models::Repository, &str, AssignedItem)],
) -> Result<Vec<Value>> {
    let mut groups: Vec<(i64, Vec<usize>)> = Vec::new();
    for (index, (repo, _, _)) in items.iter().enumerate() {
        match groups.iter_mut().find(|(id, _)| *id == repo.id) {
            Some((_, indexes)) => indexes.push(index),
            None => groups.push((repo.id, vec![index])),
        }
    }
    let mut values = vec![Value::Null; items.len()];
    for (_, indexes) in groups {
        let (repo, owner, _) = items[indexes[0]];
        let group: Vec<AssignedItem> = indexes.iter().map(|&index| items[index].2).collect();
        for (indexes, group) in indexes.chunks(100).zip(group.chunks(100)) {
            for (&index, value) in indexes
                .iter()
                .zip(documents_page(pool, reader, repo, owner, group).await?)
            {
                values[index] = value;
            }
        }
    }
    Ok(values)
}

pub(super) async fn required_issue(
    pool: &codeyard_db_core::DbPool,
    repo: i64,
    number: &str,
) -> Result<i64> {
    let number = number.parse().map_err(|_| Error::missing())?;
    codeyard_db_repos::issues::get_issue(pool, repo, number)
        .await
        .map_err(Error::unavailable)?
        .map(|i| i.id)
        .ok_or_else(Error::missing)
}

async fn list(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(p): Path<HashMap<String, String>>,
    Query(query): Query<HashMap<String, String>>,
    Query(page): Query<Pagination>,
    OriginalUri(uri): OriginalUri,
) -> Result<Response> {
    use codeyard_db_repos::discussions::ListFilter;
    use codeyard_db_repos::discussions::ListSort;
    use codeyard_db_repos::discussions::Pick;
    let (repo, auth) = repository(&state, &headers, &p["owner"], &p["repo"], false).await?;
    let state_filter = match query.get("state").map(String::as_str).unwrap_or("open") {
        "all" => None,
        state @ ("open" | "closed") => Some(state),
        _ => return Err(Error::invalid("Invalid state")),
    };
    let ascending = match query.get("direction").map(String::as_str).unwrap_or("desc") {
        "asc" => true,
        "desc" => false,
        _ => return Err(Error::invalid("Invalid direction")),
    };
    let sort = match (
        query.get("sort").map(String::as_str).unwrap_or("created"),
        ascending,
    ) {
        ("created", false) => ListSort::Newest,
        ("created", true) => ListSort::Oldest,
        ("updated", false) => ListSort::RecentlyUpdated,
        ("updated", true) => ListSort::LeastRecentlyUpdated,
        ("comments", false) => ListSort::MostCommented,
        ("comments", true) => ListSort::LeastCommented,
        _ => return Err(Error::invalid("Invalid sort")),
    };
    let issue_type = match query.get("type").map(String::as_str) {
        None | Some("*") => Pick::Any,
        Some("none") => Pick::Unset,
        Some(name) => Pick::Is(
            codeyard_db_repos::issue_attributes::catalog(
                &state.pool,
                repo.id,
                auth.as_ref().map(|a| a.user.id),
                "type",
            )
            .await
            .map_err(Error::unavailable)?
            .into_iter()
            .find(|a| a.name.to_lowercase() == name.to_lowercase())
            .ok_or_else(|| Error::invalid("Issue type does not exist"))?
            .id,
        ),
    };
    let mentioned = query
        .get("mentioned")
        .map(|name| {
            let name = if name == "@me" {
                auth.as_ref()
                    .ok_or_else(codeyard_repos_core::api_shared::github::auth::required)?
                    .user
                    .username
                    .clone()
            } else {
                name.clone()
            };
            codeyard_model::account_input::validate_namespace(&name, 3)
                .map_err(|_| Error::invalid("Invalid mentioned user"))?;
            Ok::<_, Error>(name)
        })
        .transpose()?;
    let mut labels: Vec<String> = query
        .get("labels")
        .map(|labels| {
            labels
                .split(',')
                .map(|name| name.trim().to_ascii_lowercase())
                .filter(|name| !name.is_empty())
                .collect()
        })
        .unwrap_or_default();
    labels.sort();
    labels.dedup();
    let author_id = match query.get("creator") {
        Some(name) => Some(
            codeyard_db_identity::users::accounts::get_user_by_username(&state.pool, name)
                .await
                .map_err(Error::unavailable)?
                .map_or(-1, |user| user.id),
        ),
        None => None,
    };
    let any_assignee = query.get("assignee").is_some_and(|value| value == "*");
    let assignee = match query.get("assignee").map(String::as_str) {
        None | Some("*") => Pick::Any,
        Some("none") => Pick::Unset,
        Some(name) => Pick::Is(
            codeyard_db_identity::users::accounts::get_user_by_username(&state.pool, name)
                .await
                .map_err(Error::unavailable)?
                .map_or(-1, |user| user.id),
        ),
    };
    let any_milestone = query.get("milestone").is_some_and(|value| value == "*");
    let milestone = match query.get("milestone").map(String::as_str) {
        None | Some("*") => Pick::Any,
        Some("none") => Pick::Unset,
        Some(number) => {
            let number = number
                .parse()
                .map_err(|_| Error::invalid("Invalid milestone"))?;
            Pick::Is(
                codeyard_db_repos::milestones::get_milestone(&state.pool, repo.id, number)
                    .await
                    .map_err(Error::unavailable)?
                    .map_or(-1, |summary| summary.milestone.id),
            )
        }
    };
    let since = query
        .get("since")
        .map(|since| {
            since
                .parse::<chrono::DateTime<chrono::Utc>>()
                .map_err(|_| Error::invalid("Invalid since timestamp"))
        })
        .transpose()?;
    let (offset, limit) = page.window()?;
    let filter = ListFilter {
        state: state_filter,
        author_id,
        assignee,
        milestone,
        labels: &labels,
        sort,
        offset: offset as i64,
        ..Default::default()
    };
    let rows = codeyard_db_repos::issues::list_repository_items(
        &state.pool,
        repo.id,
        &filter,
        &codeyard_db_repos::issues::RepositoryItemOptions {
            since,
            any_assignee,
            any_milestone,
            issue_type,
            any_type: query.get("type").is_some_and(|v| v == "*"),
            mentioned: mentioned.as_deref(),
        },
        (limit + 1) as i64,
    )
    .await
    .map_err(Error::unavailable)?;
    let next = rows.len() > limit;
    let selected: Vec<_> = rows
        .into_iter()
        .take(limit)
        .map(|(id, pull)| {
            if pull {
                AssignedItem::PullRequest(id)
            } else {
                AssignedItem::Issue(id)
            }
        })
        .collect();
    let items = documents(
        &state.pool,
        Reader::Request(&state, auth.as_ref()),
        &repo,
        &p["owner"],
        &selected,
    )
    .await?;
    page.response(&uri, items, next)
}

pub(super) fn operation_error(error: codeyard_repos_pulls::issues::OperationError) -> Error {
    use codeyard_repos_pulls::issues::OperationError;
    match error {
        OperationError::Invalid { message, .. } => Error::invalid(message),
        OperationError::Missing { .. } => Error::missing(),
        OperationError::Denied { message, .. } => Error::new(StatusCode::FORBIDDEN, message),
        OperationError::Unauthenticated { .. } => {
            codeyard_repos_core::api_shared::github::auth::required()
        }
        OperationError::Conflict { message, .. } => Error::new(StatusCode::CONFLICT, message),
        OperationError::Unavailable { .. } => Error::unavailable(()),
    }
}

pub(super) fn pull_error(
    error: codeyard_repos_core::repositories::pull_requests::OperationError,
) -> Error {
    crate::pulls::mutations::operation_error(error)
}

fn title(value: Option<&Value>) -> Result<Option<String>> {
    match value {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(title)) => Ok(Some(title.clone())),
        Some(Value::Number(title)) if title.is_i64() || title.is_u64() => {
            Ok(Some(title.to_string()))
        }
        _ => Err(Error::invalid("Title must be a string or integer")),
    }
}

fn text(value: &Value, field: &str) -> Result<Option<String>> {
    match value.get(field) {
        None => Ok(None),
        Some(Value::Null) => Ok(Some(String::new())),
        Some(Value::String(text)) => Ok(Some(text.clone())),
        _ => Err(Error::invalid(format!("Invalid {field}"))),
    }
}

pub fn assignment_error(error: anyhow::Error) -> Error {
    use codeyard_db_repos::assignees::AssignmentRefusal;
    match error.downcast_ref::<AssignmentRefusal>() {
        Some(AssignmentRefusal::Denied) => Error::new(StatusCode::FORBIDDEN, "Assignment denied"),
        Some(AssignmentRefusal::NotFound) => Error::missing(),
        Some(_) => Error::invalid("User cannot be assigned"),
        None => Error::unavailable(error),
    }
}

pub async fn apply_metadata(
    state: &AppState,
    authentication: &codeyard_repos_core::api_shared::github::auth::Authentication,
    repo: &codeyard_db_core::models::Repository,
    item: AssignedItem,
    detail: (&str, i32, &str),
    metadata: Metadata,
) -> Result<()> {
    let actor = &authentication.user;
    if let Some(kind) = metadata.issue_type {
        let AssignedItem::Issue(id) = item else {
            return Err(Error::invalid("Pull requests do not support issue types"));
        };
        codeyard_db_repos::issue_attributes::set_type(
            &state.pool,
            repo.id,
            authentication.database_actor(),
            id,
            kind,
        )
        .await
        .map_err(crate::issues::issue_attributes::error)?;
    }
    if let Some(ids) = metadata.labels {
        crate::issues::labels::save_labels(
            &state.pool,
            authentication.database_actor(),
            repo.id,
            item,
            &ids,
            true,
        )
        .await?;
    }
    if let Some(number) = metadata.milestone {
        match item {
            AssignedItem::Issue(id) => {
                codeyard_db_repos::milestones::set_issue_milestone(
                    &state.pool,
                    authentication.database_actor(),
                    repo.id,
                    id,
                    number,
                )
                .await
            }
            AssignedItem::PullRequest(id) => {
                codeyard_db_repos::milestones::set_pull_request_milestone(
                    &state.pool,
                    authentication.database_actor(),
                    repo.id,
                    id,
                    number,
                )
                .await
            }
        }
        .map_err(Error::unavailable)?;
    }
    if let Some(users) = metadata.assignees {
        let (owner, number, title) = detail;
        let target = codeyard_repos_pulls::issues::assignments::Target {
            actor,
            repository: repo,
            owner,
            item,
            number,
            title,
        };
        let removed: Vec<_> = codeyard_db_repos::assignees::list_assignees(&state.pool, item)
            .await
            .map_err(Error::unavailable)?
            .into_iter()
            .filter(|name| !users.iter().any(|user| user.username == *name))
            .collect();
        codeyard_repos_pulls::issues::assignments::unassign(
            &state.pool,
            &target,
            authentication.database_actor(),
            &removed,
        )
        .await
        .map_err(assignment_error)?;
        codeyard_repos_pulls::issues::assignments::assign(
            &state.pool,
            &state.notification_broadcaster,
            &target,
            authentication.database_actor(),
            &users,
        )
        .await
        .map_err(assignment_error)?;
    }
    Ok(())
}

async fn create(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(p): Path<HashMap<String, String>>,
    Json(input): Json<Value>,
) -> Result<(StatusCode, Json<Value>)> {
    let (repo, auth) = repository(&state, &headers, &p["owner"], &p["repo"], true).await?;
    let authentication = auth.ok_or_else(codeyard_repos_core::api_shared::github::auth::required)?;
    let actor = &authentication.user;
    let title = title(input.get("title"))?.ok_or_else(|| Error::invalid("Title is required"))?;
    let parent_issue_id = input
        .get("parent_issue_id")
        .map(|value| {
            value
                .as_i64()
                .filter(|id| *id > 0)
                .ok_or_else(|| Error::invalid("parent_issue_id must be a positive integer"))
        })
        .transpose()?;
    let parent_repository_id = if let Some(parent) = parent_issue_id {
        let parent = codeyard_db_repos::issues::get_issue_by_id(&state.pool, parent)
            .await
            .map_err(Error::unavailable)?
            .ok_or_else(Error::missing)?;
        authentication
            .repository_scope(&state.pool, parent.repo_id, "issues", true)
            .await
            .map_err(crate::auth::scope_error)?;
        Some(parent.repo_id)
    } else {
        None
    };
    let body = text(&input, "body")?;
    let metadata = metadata(&state.pool, &repo, actor, &input).await?;
    let issue = codeyard_repos_pulls::issues::mutations::create_issue_with_metadata(
        &state.pool,
        &state.notification_broadcaster,
        authentication.database_actor(),
        p["owner"].clone(),
        repo.name.clone(),
        codeyard_repos_pulls::issues::mutations::CreateIssueRequest {
            title,
            body,
            repository_id: Some(repo.id),
            parent_issue_id,
            parent_repository_id,
        },
        metadata.creation(),
    )
    .await
    .map_err(operation_error)?;
    let item = AssignedItem::Issue(issue.id);
    Ok((
        StatusCode::CREATED,
        Json(
            document(
                &state.pool,
                Reader::Request(&state, Some(&authentication)),
                &repo,
                &p["owner"],
                item,
            )
            .await?,
        ),
    ))
}

async fn update(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(p): Path<HashMap<String, String>>,
    Json(input): Json<Value>,
) -> Result<Json<Value>> {
    let (repo, auth) = repository(&state, &headers, &p["owner"], &p["repo"], true).await?;
    let authentication = auth.ok_or_else(codeyard_repos_core::api_shared::github::auth::required)?;
    let actor = &authentication.user;
    let item =
        codeyard_app::api_shared::github::labels::item(&state.pool, repo.id, &p["issue_number"])
            .await?;
    let title = title(input.get("title"))?;
    let body = text(&input, "body")?;
    let requested_state = text(&input, "state")?;
    let reason = text(&input, "state_reason")?.filter(|reason| !reason.is_empty());
    let metadata = metadata(&state.pool, &repo, actor, &input).await?;
    if matches!(item, AssignedItem::PullRequest(_)) && metadata.issue_type.is_some() {
        return Err(Error::invalid("Pull requests do not support issue types"));
    }
    let metadata_only = title.is_none()
        && body.is_none()
        && requested_state.is_none()
        && reason.is_none()
        && (metadata.issue_type.is_some()
            || metadata.labels.is_some()
            || metadata.assignees.is_some()
            || metadata.milestone.is_some());
    let (number, title) = match item {
        AssignedItem::Issue(id) if metadata_only => {
            let issue = codeyard_db_repos::issues::get_issue_by_id(&state.pool, id)
                .await
                .map_err(Error::unavailable)?
                .ok_or_else(Error::missing)?;
            (issue.number, issue.title)
        }
        AssignedItem::PullRequest(id) if metadata_only => {
            let pull =
                codeyard_db_repos::pull_requests::records::get_pull_request_by_id(&state.pool, id)
                    .await
                    .map_err(Error::unavailable)?
                    .ok_or_else(Error::missing)?;
            (pull.number, pull.title)
        }
        AssignedItem::Issue(id) => {
            let issue = codeyard_db_repos::issues::get_issue_by_id(&state.pool, id)
                .await
                .map_err(Error::unavailable)?
                .ok_or_else(Error::missing)?;
            let saved = codeyard_repos_pulls::issues::mutations::update_issue_latest(
                &state.pool,
                &state.notification_broadcaster,
                actor.id,
                repo.id,
                issue.number,
                codeyard_repos_pulls::issues::mutations::UpdateIssueRequest {
                    expected_state: issue.state,
                    expected_updated: issue.updated_at.to_rfc3339(),
                    title,
                    body,
                    state: requested_state,
                    state_reason: reason,
                },
            )
            .await
            .map_err(operation_error)?;
            (saved.number, saved.title)
        }
        AssignedItem::PullRequest(id) => {
            let pull =
                codeyard_db_repos::pull_requests::records::get_pull_request_by_id(&state.pool, id)
                    .await
                    .map_err(Error::unavailable)?
                    .ok_or_else(Error::missing)?;
            let saved = codeyard_repos_pulls::repositories::pull_requests::mutations::update_pull_request_latest(&state.pool, &state.ci_runs, &state.notification_broadcaster, actor.id, (repo.id, pull.number), codeyard_repos_pulls::repositories::pull_requests::mutations::UpdatePullRequestRequest { expected_state: pull.state, expected_updated: pull.updated_at.to_rfc3339(), title, body, state: requested_state, base: None, maintainer_can_modify: None }).await.map_err(pull_error)?;
            (saved.number, saved.title)
        }
    };
    apply_metadata(
        &state,
        &authentication,
        &repo,
        item,
        (&p["owner"], number, &title),
        metadata,
    )
    .await?;
    Ok(Json(
        document(
            &state.pool,
            Reader::Request(&state, Some(&authentication)),
            &repo,
            &p["owner"],
            item,
        )
        .await?,
    ))
}

#[derive(serde::Deserialize)]
struct Assignees {
    assignees: Vec<String>,
}

async fn change_assignees(
    state: Arc<AppState>,
    headers: HeaderMap,
    p: HashMap<String, String>,
    input: Assignees,
    remove: bool,
) -> Result<Json<Value>> {
    let (repo, auth) = repository(&state, &headers, &p["owner"], &p["repo"], true).await?;
    let authentication = auth.ok_or_else(codeyard_repos_core::api_shared::github::auth::required)?;
    let actor = &authentication.user;
    let item =
        codeyard_app::api_shared::github::labels::item(&state.pool, repo.id, &p["issue_number"])
            .await?;
    let names = assignee_names(input.assignees)?;
    let current = document(
        &state.pool,
        Reader::Request(&state, Some(&authentication)),
        &repo,
        &p["owner"],
        item,
    )
    .await?;
    let number = current["number"]
        .as_i64()
        .and_then(|number| i32::try_from(number).ok())
        .ok_or_else(|| Error::unavailable(()))?;
    let title = current["title"]
        .as_str()
        .ok_or_else(|| Error::unavailable(()))?;

    let target = codeyard_repos_pulls::issues::assignments::Target {
        actor,
        repository: &repo,
        owner: &p["owner"],
        item,
        number,
        title,
    };
    if remove {
        codeyard_repos_pulls::issues::assignments::unassign(
            &state.pool,
            &target,
            authentication.database_actor(),
            &names,
        )
        .await
        .map_err(assignment_error)?;
    } else {
        let users = resolve_assignees(&state.pool, &names).await?;
        codeyard_repos_pulls::issues::assignments::assign(
            &state.pool,
            &state.notification_broadcaster,
            &target,
            authentication.database_actor(),
            &users,
        )
        .await
        .map_err(assignment_error)?;
    }
    Ok(Json(
        document(
            &state.pool,
            Reader::Request(&state, Some(&authentication)),
            &repo,
            &p["owner"],
            item,
        )
        .await?,
    ))
}

async fn add_assignees(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(p): Path<HashMap<String, String>>,
    Json(input): Json<Assignees>,
) -> Result<(StatusCode, Json<Value>)> {
    Ok((
        StatusCode::CREATED,
        change_assignees(state, headers, p, input, false).await?,
    ))
}

async fn remove_assignees(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(p): Path<HashMap<String, String>>,
    Json(input): Json<Assignees>,
) -> Result<Json<Value>> {
    change_assignees(state, headers, p, input, true).await
}

pub(super) fn handlers() -> Vec<(&'static str, MethodRouter<Arc<AppState>>)> {
    vec![
        ("issues/get", get(read)),
        ("issues/list-for-repo", get(list)),
        ("issues/create", post(create)),
        ("issues/update", patch(update)),
        ("issues/add-assignees", post(add_assignees)),
        ("issues/remove-assignees", delete(remove_assignees)),
    ]
}

use codeyard_repos_core::api_shared::github::issues::Metadata;

use codeyard_repos_core::api_shared::github::issues::Reader;

use codeyard_repos_core::api_shared::github::issues::assignee_names;

use codeyard_repos_core::api_shared::github::issues::document;

use codeyard_repos_core::api_shared::github::issues::documents;

use codeyard_repos_core::api_shared::github::issues::documents_page;

use codeyard_repos_core::api_shared::github::issues::metadata;

use codeyard_app::api_shared::github::issues::read;

use codeyard_repos_core::api_shared::github::issues::resolve_assignees;

pub mod relationships;

pub mod issue_attributes;

pub mod suggestions;

pub mod pins;

pub mod labels;

pub mod milestones;

pub mod assignees;

pub mod locking;

pub mod events;

pub mod global_issues;

pub mod comments;
