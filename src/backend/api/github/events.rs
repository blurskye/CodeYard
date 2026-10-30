//! Issue events and discussion entries share one chronological, paginated timeline.
use super::{comments, issues, labels, pagination::Pagination, pulls, repository, Error, Result};
use crate::backend::{app::AppState, db, db::assignees::AssignedItem, webhooks::payloads};
use axum::{
    extract::{OriginalUri, Path, Query, State},
    http::HeaderMap,
    response::Response,
    routing::{get, MethodRouter},
    Json,
};
use serde_json::{json, Value};
use std::{collections::HashMap, sync::Arc};

type Params = HashMap<String, String>;

async fn event_document(
    state: &AppState,
    auth: Option<&super::auth::Authentication>,
    repo: &db::models::Repository,
    owner: &str,
    id: i64,
    include_issue: bool,
) -> Result<Value> {
    let event = db::timeline::get_event(&state.pool, repo.id, id)
        .await
        .map_err(Error::unavailable)?
        .ok_or_else(Error::missing)?;
    let item = match event.subject_kind.as_str() {
        "issue" => AssignedItem::Issue(event.subject_id),
        "pull_request" => AssignedItem::PullRequest(event.subject_id),
        _ => return Err(Error::missing()),
    };
    let actor = match event.actor_id {
        Some(id) => db::users::accounts::get_user_by_id(&state.pool, id)
            .await
            .map_err(Error::unavailable)?,
        None => None,
    };
    let mut value = payloads::issue_event(&event, actor.as_ref(), repo, owner);
    match event.kind.as_str() {
        "renamed" => value["rename"] = json!({"from":event.previous,"to":event.detail}),
        "labeled" | "unlabeled" => {
            let label = db::labels::list_repo_labels(&state.pool, repo.id)
                .await
                .map_err(Error::unavailable)?
                .into_iter()
                .find(|label| label.name.eq_ignore_ascii_case(&event.detail));
            value["label"] = json!({"name":event.detail,"color":label.map(|label|label.color.trim_start_matches('#').to_owned())});
        }
        "milestoned" | "demilestoned" => value["milestone"] = json!({"title":event.detail}),
        "assigned" | "unassigned" => {
            let user = db::users::accounts::get_user_by_username(&state.pool, &event.detail)
                .await
                .map_err(Error::unavailable)?;
            value["assignee"] = user.as_ref().map_or(Value::Null, payloads::user);
            value["assigner"] = value["actor"].clone();
        }
        _ => {}
    }
    if include_issue {
        value["issue"] = issues::document(&state.pool, issues::Reader::Request(state, auth), repo, owner, item).await?;
    }
    Ok(value)
}

async fn read(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(p): Path<Params>,
) -> Result<Json<Value>> {
    let (repo, auth) = repository(&state, &headers, &p["owner"], &p["repo"], false).await?;
    let id = p["event_id"].parse().map_err(|_| Error::missing())?;
    Ok(Json(
        event_document(&state, auth.as_ref(), &repo, &p["owner"], id, true).await?,
    ))
}

async fn review_document(
    state: &AppState,
    repo: &db::models::Repository,
    owner: &str,
    id: i64,
) -> Result<Value> {
    let review = db::pull_requests::reviews::get_pull_request_review_by_id(&state.pool, id)
        .await
        .map_err(Error::unavailable)?
        .filter(|review| review.state != "pending")
        .ok_or_else(Error::missing)?;
    let pull =
        db::pull_requests::records::get_pull_request_by_id(&state.pool, review.pull_request_id)
            .await
            .map_err(Error::unavailable)?
            .filter(|pull| pull.repo_id == repo.id)
            .ok_or_else(Error::missing)?;
    let user = db::users::accounts::get_user_by_id(&state.pool, review.author_id)
        .await
        .map_err(Error::unavailable)?
        .ok_or_else(Error::missing)?;
    let mut value = payloads::pull_request_review_json(
        &review,
        &payloads::Person::of(&user),
        &pull,
        repo,
        owner,
    );
    value["user"] = payloads::user(&user);
    value["event"] = json!("reviewed");
    value["author_association"] = json!(pulls::association(&state.pool, repo, user.id).await?);
    Ok(value)
}

async fn list(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(p): Path<Params>,
    Query(page): Query<Pagination>,
    OriginalUri(uri): OriginalUri,
) -> Result<Response> {
    let (repo, auth) = repository(&state, &headers, &p["owner"], &p["repo"], false).await?;
    let item = match p.get("issue_number") {
        Some(number) => Some(labels::item(&state.pool, repo.id, number).await?),
        None => None,
    };
    let timeline = uri.path().ends_with("/timeline");
    let (offset, limit) = page.window()?;
    let rows = db::timeline::list_entries(
        &state.pool,
        repo.id,
        item,
        timeline,
        item.is_none(),
        (limit + 1) as i64,
        offset as i64,
    )
    .await
    .map_err(Error::unavailable)?;
    let next = rows.len() > limit;
    let mut items = Vec::new();
    for (kind, id) in rows.into_iter().take(limit) {
        let value = match kind.as_str() {
            "event" => event_document(&state, auth.as_ref(), &repo, &p["owner"], id, item.is_none()).await?,
            "review" => review_document(&state, &repo, &p["owner"], id).await?,
            "issue_comment" | "pr_comment" => {
                let comment = comments::find(
                    &state.pool,
                    repo.id,
                    payloads::conversation_comment_id(id, kind == "pr_comment"),
                )
                .await?;
                let mut value =
                    comments::document(&state.pool, &repo, &p["owner"], &comment).await?;
                value["event"] = json!("commented");
                value["actor"] = value["user"].clone();
                value["author_association"] = json!(
                    pulls::association(
                        &state.pool,
                        &repo,
                        value["user"]["id"].as_i64().ok_or_else(Error::missing)?
                    )
                    .await?
                );
                value
            }
            _ => return Err(Error::unavailable(())),
        };
        items.push(value);
    }
    page.response(&uri, items, next)
}

pub(super) fn handlers() -> Vec<(&'static str, MethodRouter<Arc<AppState>>)> {
    vec![
        ("issues/get-event", get(read)),
        ("issues/list-events", get(list)),
        ("issues/list-events-for-repo", get(list)),
        ("issues/list-events-for-timeline", get(list)),
    ]
}
