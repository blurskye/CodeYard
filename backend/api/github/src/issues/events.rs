//! Issue events and discussion entries share one chronological, paginated timeline.

use axum::extract::OriginalUri;
use axum::extract::Path;
use axum::extract::Query;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::response::Response;
use axum::routing::get;
use axum::routing::MethodRouter;
use axum::Json;
use codeyard_api_http::shared::github::pagination::Pagination;
use codeyard_app::api_shared::github::repository;
use codeyard_app::app::AppState;
use codeyard_db_repos::assignees::AssignedItem;
use codeyard_model::github_error::Error;
use codeyard_model::github_error::Result;
use serde_json::json;
use serde_json::Value;
use std::collections::HashMap;
use std::sync::Arc;

type Params = HashMap<String, String>;

async fn event_document(
    state: &AppState,
    auth: Option<&codeyard_app::api_shared::github::auth::Authentication>,
    repo: &codeyard_db_core::models::Repository,
    owner: &str,
    id: i64,
    include_issue: bool,
    issues: &mut HashMap<(String, i64), Value>,
) -> Result<Value> {
    let event = codeyard_db_repos::timeline::get_event(&state.pool, repo.id, id)
        .await
        .map_err(Error::unavailable)?
        .ok_or_else(Error::missing)?;
    let item = match event.subject_kind.as_str() {
        "issue" => AssignedItem::Issue(event.subject_id),
        "pull_request" => AssignedItem::PullRequest(event.subject_id),
        _ => return Err(Error::missing()),
    };
    let actor = match event.actor_id {
        Some(id) => codeyard_db_identity::users::accounts::get_user_by_id(&state.pool, id)
            .await
            .map_err(Error::unavailable)?,
        None => None,
    };
    let mut value =
        codeyard_app::webhooks::payloads::issue_event(&event, actor.as_ref(), repo, owner);
    match event.kind.as_str() {
        "renamed" => value["rename"] = json!({"from":event.previous,"to":event.detail}),
        "labeled" | "unlabeled" => {
            // Older entries have no retained color; never substitute today's mutable label.
            let color =
                (!event.previous.is_empty()).then(|| event.previous.trim_start_matches('#'));
            value["label"] = json!({"name":event.detail,"color":color});
        }
        "milestoned" | "demilestoned" => value["milestone"] = json!({"title":event.detail}),
        "assigned" | "unassigned" => {
            let user = codeyard_db_identity::users::accounts::get_user_by_username(
                &state.pool,
                &event.detail,
            )
            .await
            .map_err(Error::unavailable)?;
            value["assignee"] = user
                .as_ref()
                .map_or(Value::Null, codeyard_app::webhooks::payloads::user);
            value["assigner"] = value["actor"].clone();
        }
        _ => {}
    }
    if include_issue {
        let key = (event.subject_kind.clone(), event.subject_id);
        if !issues.contains_key(&key) {
            issues.insert(
                key.clone(),
                codeyard_app::api_shared::github::issues::document(
                    &state.pool,
                    codeyard_app::api_shared::github::issues::Reader::Request(state, auth),
                    repo,
                    owner,
                    item,
                )
                .await?,
            );
        }
        value["issue"] = issues[&key].clone();
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
        event_document(
            &state,
            auth.as_ref(),
            &repo,
            &p["owner"],
            id,
            true,
            &mut HashMap::new(),
        )
        .await?,
    ))
}

async fn review_document(
    state: &AppState,
    repo: &codeyard_db_core::models::Repository,
    owner: &str,
    id: i64,
) -> Result<Value> {
    let review =
        codeyard_db_repos::pull_requests::reviews::get_pull_request_review_by_id(&state.pool, id)
            .await
            .map_err(Error::unavailable)?
            .filter(|review| review.state != "pending")
            .ok_or_else(Error::missing)?;
    let pull = codeyard_db_repos::pull_requests::records::get_pull_request_by_id(
        &state.pool,
        review.pull_request_id,
    )
    .await
    .map_err(Error::unavailable)?
    .filter(|pull| pull.repo_id == repo.id)
    .ok_or_else(Error::missing)?;
    let user = codeyard_db_identity::users::accounts::get_user_by_id(&state.pool, review.author_id)
        .await
        .map_err(Error::unavailable)?
        .ok_or_else(Error::missing)?;
    let mut value = codeyard_app::webhooks::payloads::pull_request_review_json(
        &review,
        &codeyard_app::webhooks::payloads::Person::of(&user),
        &pull,
        repo,
        owner,
    );
    value["user"] = codeyard_app::webhooks::payloads::user(&user);
    value["event"] = json!("reviewed");
    value["author_association"] =
        json!(crate::pulls::association(&state.pool, repo, user.id).await?);
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
        Some(number) => Some(
            codeyard_app::api_shared::github::labels::item(&state.pool, repo.id, number).await?,
        ),
        None => None,
    };
    let timeline = uri.path().ends_with("/timeline");
    let (offset, limit) = page.window()?;
    let rows = codeyard_db_repos::timeline::list_entries(
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
    let mut issues = HashMap::new();
    for (kind, id) in rows.into_iter().take(limit) {
        let value = match kind.as_str() {
            "event" => {
                event_document(
                    &state,
                    auth.as_ref(),
                    &repo,
                    &p["owner"],
                    id,
                    item.is_none(),
                    &mut issues,
                )
                .await?
            }
            "review" => review_document(&state, &repo, &p["owner"], id).await?,
            "issue_comment" | "pr_comment" => {
                let comment = codeyard_app::api_shared::github::comments::find(
                    &state.pool,
                    repo.id,
                    codeyard_model::webhook_ids::conversation_comment_id(id, kind == "pr_comment"),
                )
                .await?;
                let mut value = codeyard_app::api_shared::github::comments::document(
                    &state.pool,
                    &repo,
                    &p["owner"],
                    &comment,
                )
                .await?;
                value["event"] = json!("commented");
                value["actor"] = value["user"].clone();
                value["author_association"] = json!(
                    crate::pulls::association(
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
