use super::{auth, labels, milestones, pagination::Pagination, repository, Error, Result};
use crate::backend::{app::AppState, db, db::assignees::AssignedItem, webhooks::payloads};
use axum::{
    extract::{OriginalUri, Path, Query, State},
    http::{HeaderMap, StatusCode},
    response::Response,
    routing::{delete, get, patch, post, MethodRouter},
    Json,
};
use serde_json::{json, Value};
use std::{collections::HashMap, sync::Arc};

async fn user(pool: &db::DbPool, id: i64) -> Result<Option<db::models::User>> {
    db::users::accounts::get_user_by_id(pool, id)
        .await
        .map_err(Error::unavailable)
}

/// The REST issue document that webhook payloads send. `None` when the item is gone.
pub(crate) async fn webhook_document(
    pool: &db::DbPool,
    repo: &db::models::Repository,
    owner: &str,
    item: AssignedItem,
) -> Option<Value> {
    document(pool, repo, owner, item).await.ok()
}

pub(super) async fn document(
    pool: &db::DbPool,
    repo: &db::models::Repository,
    owner: &str,
    item: AssignedItem,
) -> Result<Value> {
    let (mut value, author_id, milestone_id, closed_by_id, labels, comments, subject, id) =
        match item {
            AssignedItem::Issue(id) => {
                let issue = db::issues::get_issue_by_id(pool, id)
                    .await
                    .map_err(Error::unavailable)?
                    .filter(|issue| issue.repo_id == repo.id)
                    .ok_or_else(Error::missing)?;
                let author = user(pool, issue.author_id)
                    .await?
                    .ok_or_else(|| Error::unavailable(()))?;
                let value =
                    payloads::issue_json(&issue, &payloads::Person::of(&author), repo, owner);
                let labels = db::labels::get_issue_labels(pool, id)
                    .await
                    .map_err(Error::unavailable)?;
                let comments = db::issues::count_issue_comments(pool, &[id])
                    .await
                    .map_err(Error::unavailable)?
                    .first()
                    .map_or(0, |(_, count)| *count);
                (
                    value,
                    issue.author_id,
                    issue.milestone_id,
                    issue.closed_by_id,
                    labels,
                    comments,
                    db::reactions::Subject::Issue,
                    id,
                )
            }
            AssignedItem::PullRequest(id) => {
                let pull = db::pull_requests::records::get_pull_request_by_id(pool, id)
                    .await
                    .map_err(Error::unavailable)?
                    .filter(|pull| pull.repo_id == repo.id)
                    .ok_or_else(Error::missing)?;
                let author = user(pool, pull.author_id)
                    .await?
                    .ok_or_else(|| Error::unavailable(()))?;
                let value = payloads::pull_request_issue(
                    &pull,
                    &payloads::Person::of(&author),
                    repo,
                    owner,
                );
                let labels = db::labels::get_pull_request_labels(pool, id)
                    .await
                    .map_err(Error::unavailable)?;
                let comments = db::pull_requests::reviews::pull_request_comment_counts(pool, id)
                    .await
                    .map_err(Error::unavailable)?
                    .0;
                (
                    value,
                    pull.author_id,
                    pull.milestone_id,
                    pull.merged_by_id,
                    labels,
                    comments,
                    db::reactions::Subject::PullRequest,
                    id,
                )
            }
        };
    value["user"] = user(pool, author_id)
        .await?
        .as_ref()
        .map_or(Value::Null, payloads::user);
    value["author_association"] = json!(super::pulls::association(pool, repo, author_id).await?);
    value["closed_by"] = match closed_by_id {
        Some(id) => user(pool, id)
            .await?
            .as_ref()
            .map_or(Value::Null, payloads::user),
        None => Value::Null,
    };
    value["labels"] = json!(labels
        .iter()
        .map(|label| payloads::label(label, owner, &repo.name))
        .collect::<Vec<_>>());
    value["comments"] = json!(comments);
    value["milestone"] = match milestone_id {
        Some(id) => match db::milestones::get_milestone_by_id(pool, repo.id, id)
            .await
            .map_err(Error::unavailable)?
        {
            Some(summary) => milestones::document(pool, &summary, owner, &repo.name).await?,
            None => Value::Null,
        },
        None => Value::Null,
    };
    let mut assignees = Vec::new();
    for name in db::assignees::list_assignees(pool, item)
        .await
        .map_err(Error::unavailable)?
    {
        let user = db::users::accounts::get_user_by_username(pool, &name)
            .await
            .map_err(Error::unavailable)?
            .ok_or_else(|| Error::unavailable(()))?;
        assignees.push(payloads::user(&user));
    }
    value["assignee"] = assignees.first().cloned().unwrap_or(Value::Null);
    value["assignees"] = json!(assignees);
    let url = value["url"]
        .as_str()
        .ok_or_else(|| Error::unavailable(()))?;
    let counts = db::reactions::reaction_counts(pool, subject.name(), &[id], 0)
        .await
        .map_err(Error::unavailable)?;
    value["reactions"] = payloads::reaction_rollup(&format!("{url}/reactions"), &counts);
    if matches!(item, AssignedItem::Issue(_)) {
        value["type"] = db::issue_attributes::assigned_type(pool, repo.id, id)
            .await
            .map_err(Error::unavailable)?
            .as_ref()
            .map(payloads::issue_type)
            .unwrap_or(Value::Null);
        let summary = db::relationships::summary(pool, id)
            .await
            .map_err(Error::unavailable)?;
        value["sub_issues_summary"] = json!({"total":summary.total,"completed":summary.completed,"percent_completed":if summary.total==0{0}else{summary.completed*100/summary.total}});
        value["issue_dependencies_summary"] = json!({"blocked_by":summary.blocked_by,"blocking":summary.blocking,"total_blocked_by":summary.total_blocked_by,"total_blocking":summary.total_blocking});
    }
    value["pinned_comment"] = match db::pins::get_pin(pool, item)
        .await
        .map_err(Error::unavailable)?
    {
        Some(pin) => {
            let id = payloads::conversation_comment_id(
                pin.comment_id,
                matches!(item, AssignedItem::PullRequest(_)),
            );
            let comment = super::comments::find(pool, repo.id, id).await?;
            super::comments::document(pool, repo, owner, &comment).await?
        }
        None => Value::Null,
    };
    Ok(value)
}

pub(super) async fn required_issue(pool: &db::DbPool, repo: i64, number: &str) -> Result<i64> {
    let number = number.parse().map_err(|_| Error::missing())?;
    db::issues::get_issue(pool, repo, number)
        .await
        .map_err(Error::unavailable)?
        .map(|i| i.id)
        .ok_or_else(Error::missing)
}

async fn read(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(p): Path<HashMap<String, String>>,
) -> Result<Json<Value>> {
    let (repo, _) = repository(&state, &headers, &p["owner"], &p["repo"], false).await?;
    let item = labels::item(&state.pool, repo.id, &p["issue_number"]).await?;
    Ok(Json(document(&state.pool, &repo, &p["owner"], item).await?))
}

async fn list(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(p): Path<HashMap<String, String>>,
    Query(query): Query<HashMap<String, String>>,
    Query(page): Query<Pagination>,
    OriginalUri(uri): OriginalUri,
) -> Result<Response> {
    use db::discussions::{ListFilter, ListSort, Pick};
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
            db::issue_attributes::catalog(
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
                    .ok_or_else(auth::required)?
                    .user
                    .username
                    .clone()
            } else {
                name.clone()
            };
            crate::backend::identity::credentials::validate_username(&name)
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
            db::users::accounts::get_user_by_username(&state.pool, name)
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
            db::users::accounts::get_user_by_username(&state.pool, name)
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
                db::milestones::get_milestone(&state.pool, repo.id, number)
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
    let rows = db::issues::list_repository_items(
        &state.pool,
        repo.id,
        &filter,
        &db::issues::RepositoryItemOptions {
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
    let mut items = Vec::new();
    for (id, pull) in rows {
        items.push(
            document(
                &state.pool,
                &repo,
                &p["owner"],
                if pull {
                    AssignedItem::PullRequest(id)
                } else {
                    AssignedItem::Issue(id)
                },
            )
            .await?,
        );
    }
    page.response(&uri, items, next)
}

pub(super) fn operation_error(error: crate::backend::issues::OperationError) -> Error {
    use crate::backend::issues::OperationError;
    match error {
        OperationError::Invalid { message, .. } => Error::invalid(message),
        OperationError::Missing { .. } => Error::missing(),
        OperationError::Denied { message, .. } => Error::new(StatusCode::FORBIDDEN, message),
        OperationError::Unauthenticated { .. } => auth::required(),
        OperationError::Conflict { message, .. } => Error::new(StatusCode::CONFLICT, message),
        OperationError::Unavailable { .. } => Error::unavailable(()),
    }
}

pub(super) fn pull_error(
    error: crate::backend::repositories::pull_requests::OperationError,
) -> Error {
    use crate::backend::repositories::pull_requests::OperationError;
    match error {
        OperationError::Invalid { message, .. } => Error::invalid(message),
        OperationError::Missing { .. } => Error::missing(),
        OperationError::Denied { message, .. } => Error::new(StatusCode::FORBIDDEN, message),
        OperationError::Unauthenticated { .. } => auth::required(),
        OperationError::Conflict { message, .. } => Error::new(StatusCode::CONFLICT, message),
        OperationError::TooLarge { message, .. } => {
            Error::new(StatusCode::PAYLOAD_TOO_LARGE, message)
        }
        _ => Error::unavailable(()),
    }
}

fn title(value: Option<&Value>) -> Result<Option<String>> {
    match value {
        None => Ok(None),
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

#[derive(Default)]
pub(in crate::backend::api) struct Metadata {
    issue_type: Option<Option<i64>>,
    labels: Option<Vec<i64>>,
    assignees: Option<Vec<db::models::User>>,
    milestone: Option<Option<i32>>,
}

pub(in crate::backend::api) async fn metadata(
    pool: &db::DbPool,
    repo: &db::models::Repository,
    actor: &db::models::User,
    input: &Value,
) -> Result<Metadata> {
    if !crate::backend::repositories::access::can_triage(pool, actor, repo)
        .await
        .map_err(Error::unavailable)?
    {
        return Ok(Metadata::default());
    }
    let mut result = Metadata::default();
    if let Some(value) = input.get("type") {
        result.issue_type = Some(if value.is_null() {
            None
        } else {
            let name = value
                .as_str()
                .ok_or_else(|| Error::invalid("Invalid issue type"))?;
            Some(
                db::issue_attributes::catalog(pool, repo.id, Some(actor.id), "type")
                    .await
                    .map_err(Error::unavailable)?
                    .into_iter()
                    .find(|a| a.is_enabled && a.name.to_lowercase() == name.to_lowercase())
                    .ok_or_else(|| Error::invalid("Issue type does not exist"))?
                    .id,
            )
        });
    }
    if let Some(labels) = input.get("labels") {
        let labels = labels
            .as_array()
            .ok_or_else(|| Error::invalid("Invalid labels"))?;
        if labels.len() > db::labels::MAX_ISSUE_LABELS {
            return Err(Error::invalid("Too many labels"));
        }
        let catalogue = db::labels::list_repo_labels(pool, repo.id)
            .await
            .map_err(Error::unavailable)?;
        let mut ids = Vec::new();
        for label in labels {
            let found = catalogue.iter().find(|candidate| {
                label
                    .as_str()
                    .or_else(|| label["name"].as_str())
                    .is_some_and(|name| candidate.name.eq_ignore_ascii_case(name))
                    || label["id"].as_i64() == Some(candidate.id)
            });
            ids.push(
                found
                    .ok_or_else(|| Error::invalid("Label does not exist"))?
                    .id,
            );
        }
        result.labels = Some(ids);
    }
    if input.get("assignees").is_some() || input.get("assignee").is_some() {
        let names: Vec<String> = if let Some(names) = input.get("assignees") {
            serde_json::from_value(names.clone())
                .map_err(|_| Error::invalid("Invalid assignees"))?
        } else {
            match input.get("assignee").and_then(Value::as_str) {
                Some(name) => vec![name.to_owned()],
                None => Vec::new(),
            }
        };
        let assignable = db::assignees::list_assignable_usernames(pool, repo.id)
            .await
            .map_err(Error::unavailable)?;
        let mut users = Vec::new();
        for name in names {
            if !assignable
                .iter()
                .any(|candidate| candidate.eq_ignore_ascii_case(&name))
            {
                return Err(Error::invalid("User cannot be assigned"));
            }
            let user = db::users::accounts::get_user_by_username(pool, &name)
                .await
                .map_err(Error::unavailable)?
                .ok_or_else(|| Error::invalid("User cannot be assigned"))?;
            if !users
                .iter()
                .any(|candidate: &db::models::User| candidate.id == user.id)
            {
                users.push(user);
            }
        }
        if users.len() > db::assignees::MAX_ASSIGNEES as usize {
            return Err(Error::invalid("Too many assignees"));
        }
        result.assignees = Some(users);
    }
    if let Some(number) = input.get("milestone") {
        result.milestone = Some(if number.is_null() {
            None
        } else {
            let number = number
                .as_i64()
                .or_else(|| number.as_str().and_then(|number| number.parse().ok()))
                .and_then(|number| i32::try_from(number).ok())
                .ok_or_else(|| Error::invalid("Invalid milestone"))?;
            db::milestones::get_milestone(pool, repo.id, number)
                .await
                .map_err(Error::unavailable)?
                .ok_or_else(|| Error::invalid("Milestone does not exist"))?;
            Some(number)
        });
    }
    Ok(result)
}

pub(in crate::backend::api) fn assignment_error(error: anyhow::Error) -> Error {
    use db::assignees::AssignmentRefusal;
    match error.downcast_ref::<AssignmentRefusal>() {
        Some(AssignmentRefusal::Denied) => Error::new(StatusCode::FORBIDDEN, "Assignment denied"),
        Some(AssignmentRefusal::NotFound) => Error::missing(),
        Some(_) => Error::invalid("User cannot be assigned"),
        None => Error::unavailable(error),
    }
}

pub(in crate::backend::api) async fn apply_metadata(
    state: &AppState,
    actor: &db::models::User,
    repo: &db::models::Repository,
    item: AssignedItem,
    detail: (&str, i32, &str),
    metadata: Metadata,
) -> Result<()> {
    if let Some(kind) = metadata.issue_type {
        let AssignedItem::Issue(id) = item else {
            return Err(Error::invalid("Pull requests do not support issue types"));
        };
        db::issue_attributes::set_type(&state.pool, repo.id, actor.id, id, kind)
            .await
            .map_err(super::issue_attributes::error)?;
    }
    if let Some(ids) = metadata.labels {
        labels::save_labels(&state.pool, actor.id, repo.id, item, &ids, true).await?;
    }
    if let Some(number) = metadata.milestone {
        let old_id = match item {
            AssignedItem::Issue(id) => db::issues::get_issue_by_id(&state.pool, id)
                .await
                .map_err(Error::unavailable)?
                .and_then(|issue| issue.milestone_id),
            AssignedItem::PullRequest(id) => {
                db::pull_requests::records::get_pull_request_by_id(&state.pool, id)
                    .await
                    .map_err(Error::unavailable)?
                    .and_then(|pull| pull.milestone_id)
            }
        };
        let before = match old_id {
            Some(id) => db::milestones::get_milestone_by_id(&state.pool, repo.id, id)
                .await
                .map_err(Error::unavailable)?,
            None => None,
        };
        let after = match number {
            Some(number) => db::milestones::get_milestone(&state.pool, repo.id, number)
                .await
                .map_err(Error::unavailable)?,
            None => None,
        };
        match item {
            AssignedItem::Issue(id) => {
                db::milestones::set_issue_milestone(&state.pool, actor.id, repo.id, id, number)
                    .await
            }
            AssignedItem::PullRequest(id) => {
                db::milestones::set_pull_request_milestone(
                    &state.pool,
                    actor.id,
                    repo.id,
                    id,
                    number,
                )
                .await
            }
        }
        .map_err(Error::unavailable)?;
        if before.as_ref().map(|summary| summary.milestone.id)
            != after.as_ref().map(|summary| summary.milestone.id)
        {
            if let Some(summary) = before {
                db::timeline::record_for(
                    &state.pool,
                    repo.id,
                    item,
                    actor.id,
                    "demilestoned",
                    &summary.milestone.title,
                )
                .await;
            }
            if let Some(summary) = after {
                db::timeline::record_for(
                    &state.pool,
                    repo.id,
                    item,
                    actor.id,
                    "milestoned",
                    &summary.milestone.title,
                )
                .await;
            }
        }
    }
    if let Some(users) = metadata.assignees {
        let mut added = Vec::new();
        for name in db::assignees::list_assignees(&state.pool, item)
            .await
            .map_err(Error::unavailable)?
        {
            if !users
                .iter()
                .any(|user| user.username.eq_ignore_ascii_case(&name))
                && db::assignees::unassign_user(&state.pool, actor.id, repo.id, item, &name)
                    .await
                    .map_err(assignment_error)?
            {
                db::timeline::record_for(&state.pool, repo.id, item, actor.id, "unassigned", &name)
                    .await;
            }
        }
        for user in users {
            if db::assignees::assign_user(&state.pool, actor.id, repo.id, item, user.id)
                .await
                .map_err(assignment_error)?
            {
                added.push(user.id);
                db::timeline::record_for(
                    &state.pool,
                    repo.id,
                    item,
                    actor.id,
                    "assigned",
                    &user.username,
                )
                .await;
            }
        }
        let (owner, number, title) = detail;
        let pull_request = matches!(item, AssignedItem::PullRequest(_));
        let link = format!(
            "/{owner}/{}/{}/{number}",
            repo.name,
            if pull_request { "pulls" } else { "issues" }
        );
        crate::backend::notifications::assignments::notify_assignees(
            &state.pool,
            &state.notification_broadcaster,
            &crate::backend::notifications::assignments::Assignment {
                actor,
                repository: repo,
                owner,
                repo: &repo.name,
                number,
                pull_request,
                title,
                link: &link,
            },
            &added,
        )
        .await;
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
    let actor = auth.ok_or_else(auth::required)?.user;
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
    let body = text(&input, "body")?;
    let metadata = metadata(&state.pool, &repo, &actor, &input).await?;
    let issue = crate::backend::issues::mutations::create_issue(
        &state.pool,
        &state.notification_broadcaster,
        actor.id,
        p["owner"].clone(),
        repo.name.clone(),
        crate::backend::issues::mutations::CreateIssueRequest {
            title,
            body,
            repository_id: Some(repo.id),
            parent_issue_id,
        },
    )
    .await
    .map_err(operation_error)?;
    let item = AssignedItem::Issue(issue.id);
    apply_metadata(
        &state,
        &actor,
        &repo,
        item,
        (&p["owner"], issue.number, &issue.title),
        metadata,
    )
    .await?;
    Ok((
        StatusCode::CREATED,
        Json(document(&state.pool, &repo, &p["owner"], item).await?),
    ))
}

async fn update(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(p): Path<HashMap<String, String>>,
    Json(input): Json<Value>,
) -> Result<Json<Value>> {
    let (repo, auth) = repository(&state, &headers, &p["owner"], &p["repo"], true).await?;
    let actor = auth.ok_or_else(auth::required)?.user;
    let item = labels::item(&state.pool, repo.id, &p["issue_number"]).await?;
    let title = title(input.get("title"))?;
    let body = text(&input, "body")?;
    let requested_state = text(&input, "state")?;
    let reason = text(&input, "state_reason")?.filter(|reason| !reason.is_empty());
    let metadata = metadata(&state.pool, &repo, &actor, &input).await?;
    if matches!(item, AssignedItem::PullRequest(_)) && metadata.issue_type.is_some() {
        return Err(Error::invalid("Pull requests do not support issue types"));
    }
    let (number, title) = match item {
        AssignedItem::Issue(id) => {
            let issue = db::issues::get_issue_by_id(&state.pool, id)
                .await
                .map_err(Error::unavailable)?
                .ok_or_else(Error::missing)?;
            let saved = crate::backend::issues::mutations::update_issue_latest(
                &state.pool,
                &state.notification_broadcaster,
                actor.id,
                p["owner"].clone(),
                repo.name.clone(),
                issue.number,
                crate::backend::issues::mutations::UpdateIssueRequest {
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
            let pull = db::pull_requests::records::get_pull_request_by_id(&state.pool, id)
                .await
                .map_err(Error::unavailable)?
                .ok_or_else(Error::missing)?;
            let saved = crate::backend::repositories::pull_requests::mutations::update_pull_request_latest(&state.pool, &state.ci_runs, &state.notification_broadcaster, actor.id, (p["owner"].clone(), repo.name.clone(), pull.number), crate::backend::repositories::pull_requests::mutations::UpdatePullRequestRequest { expected_state: pull.state, expected_updated: pull.updated_at.to_rfc3339(), title, body, state: requested_state, base: None, maintainer_can_modify: None }).await.map_err(pull_error)?;
            (saved.number, saved.title)
        }
    };
    apply_metadata(
        &state,
        &actor,
        &repo,
        item,
        (&p["owner"], number, &title),
        metadata,
    )
    .await?;
    Ok(Json(document(&state.pool, &repo, &p["owner"], item).await?))
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
    let actor = auth.ok_or_else(auth::required)?.user;
    let item = labels::item(&state.pool, repo.id, &p["issue_number"]).await?;
    let mut names = db::assignees::list_assignees(&state.pool, item)
        .await
        .map_err(Error::unavailable)?;
    if remove {
        names.retain(|name| {
            !input
                .assignees
                .iter()
                .any(|removed| removed.eq_ignore_ascii_case(name))
        });
    } else {
        names.extend(input.assignees);
    }
    let plan = metadata(&state.pool, &repo, &actor, &json!({"assignees":names})).await?;
    if plan.assignees.is_none() {
        return Err(Error::new(StatusCode::FORBIDDEN, "Assignment denied"));
    }
    let current = document(&state.pool, &repo, &p["owner"], item).await?;
    let number = current["number"]
        .as_i64()
        .and_then(|number| i32::try_from(number).ok())
        .ok_or_else(|| Error::unavailable(()))?;
    let title = current["title"]
        .as_str()
        .ok_or_else(|| Error::unavailable(()))?;
    apply_metadata(
        &state,
        &actor,
        &repo,
        item,
        (&p["owner"], number, title),
        plan,
    )
    .await?;
    Ok(Json(document(&state.pool, &repo, &p["owner"], item).await?))
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
