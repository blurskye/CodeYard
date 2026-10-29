use super::{auth, issues, labels, pagination::Pagination, repository, Error, Result};
use crate::backend::{
    app::AppState,
    db,
    db::assignees::AssignedItem,
    issues::comments::{CommentTarget, CreateCommentRequest},
    repositories::pull_requests::comments::{self, CreatePullRequestCommentRequest},
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

pub(super) struct Comment {
    pub id: i64,
    pub item: AssignedItem,
    number: i32,
    author_id: i64,
    body: String,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
}

pub(super) async fn find(pool: &db::DbPool, repo_id: i64, id: i64) -> Result<Comment> {
    if id < 2 {
        return Err(Error::missing());
    }
    let native_id = id / 2;
    if id % 2 == 0 {
        let comment = db::issues::get_issue_comment(pool, native_id)
            .await
            .map_err(Error::unavailable)?
            .ok_or_else(Error::missing)?;
        let issue = db::issues::get_issue_by_id(pool, comment.issue_id)
            .await
            .map_err(Error::unavailable)?
            .filter(|issue| issue.repo_id == repo_id)
            .ok_or_else(Error::missing)?;
        Ok(Comment {
            id: native_id,
            item: AssignedItem::Issue(issue.id),
            number: issue.number,
            author_id: comment.author_id,
            body: comment.body,
            created_at: comment.created_at,
            updated_at: comment.updated_at,
        })
    } else {
        let comment = db::pull_requests::reviews::get_pull_request_comment_by_id(pool, native_id)
            .await
            .map_err(Error::unavailable)?
            .filter(|comment| comment.path.is_none() && comment.review_id.is_none())
            .ok_or_else(Error::missing)?;
        let pull =
            db::pull_requests::records::get_pull_request_by_id(pool, comment.pull_request_id)
                .await
                .map_err(Error::unavailable)?
                .filter(|pull| pull.repo_id == repo_id)
                .ok_or_else(Error::missing)?;
        Ok(Comment {
            id: native_id,
            item: AssignedItem::PullRequest(pull.id),
            number: pull.number,
            author_id: comment.author_id,
            body: comment.body,
            created_at: comment.created_at,
            updated_at: comment.updated_at,
        })
    }
}

pub(super) async fn document(
    pool: &db::DbPool,
    repo: &db::models::Repository,
    owner: &str,
    comment: &Comment,
) -> Result<Value> {
    let user = db::users::accounts::get_user_by_id(pool, comment.author_id)
        .await
        .map_err(Error::unavailable)?
        .ok_or_else(|| Error::unavailable(()))?;
    let author = payloads::Person::of(&user);
    let mut value = payloads::conversation_comment(
        comment.id,
        &comment.body,
        &author,
        (comment.created_at, comment.updated_at),
        payloads::Scope {
            repo,
            owner,
            sender: &author,
        },
        comment.number,
        matches!(comment.item, AssignedItem::PullRequest(_)),
    );
    value["user"] = payloads::user(&user);
    value["author_association"] =
        serde_json::json!(super::pulls::association(pool, repo, user.id).await?);
    value["pin"] = match db::pins::get_pin(pool, comment.item)
        .await
        .map_err(Error::unavailable)?
        .filter(|pin| pin.comment_id == comment.id)
    {
        Some(pin) => {
            let pinner = match pin.pinned_by {
                Some(id) => db::users::accounts::get_user_by_id(pool, id)
                    .await
                    .map_err(Error::unavailable)?,
                None => None,
            };
            payloads::comment_pin(&pin, pinner.as_ref())
        }
        None => Value::Null,
    };
    Ok(value)
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Representation { Raw, Text, Html, Full }

fn representation(headers: &HeaderMap) -> Representation {
    let mut selected = (0.0_f32, Representation::Raw);
    for header in headers.get_all(axum::http::header::ACCEPT).iter() {
        let Ok(header) = header.to_str() else { continue };
        for range in header.split(',') {
            let mut parts = range.split(';').map(str::trim);
            let media = parts.next().unwrap_or("").to_ascii_lowercase();
            let profile = match media.as_str() {
                "application/vnd.github.text+json" | "application/vnd.github.v3.text+json" => Representation::Text,
                "application/vnd.github.html+json" | "application/vnd.github.v3.html+json" => Representation::Html,
                "application/vnd.github.full+json" | "application/vnd.github.v3.full+json" => Representation::Full,
                "application/vnd.github.raw+json" | "application/vnd.github.v3.raw+json" | "application/vnd.github+json" | "application/json" | "*/*" => Representation::Raw,
                _ => continue,
            };
            let weight = parts.find_map(|part| part.strip_prefix("q=")).map_or(1.0, |weight| weight.parse::<f32>().unwrap_or(0.0));
            if weight > selected.0 && weight <= 1.0 { selected = (weight, profile); }
        }
    }
    selected.1
}

async fn represent(state: &AppState, headers: &HeaderMap, repo: &db::models::Repository, owner: &str, mut values: Vec<Value>) -> Result<Vec<Value>> {
    let profile = representation(headers);
    if profile == Representation::Raw { return Ok(values); }
    let html = matches!(profile, Representation::Html | Representation::Full);
    let text = matches!(profile, Representation::Text | Representation::Full);
    let references = if html {
        let authentication = auth::authenticate(state, headers, false).await?;
        let texts: Vec<_> = values.iter().filter_map(|value| value["body"].as_str()).collect();
        super::meta::resolve_references(state, authentication.as_ref(), repo, owner, &repo.name, &texts, false).await?
    } else { Default::default() };
    tokio::task::spawn_blocking(move || {
        let mut size = 0_usize;
        for value in &mut values {
            let body = value["body"].as_str().unwrap_or_default();
            let rendered_html = html.then(|| crate::webui::components::parse_markdown(body, &references));
            let rendered_text = text.then(|| crate::webui::components::markdown_text(body));
            size = size.saturating_add(if profile == Representation::Full { body.len() } else { 0 }).saturating_add(rendered_html.as_ref().map_or(0, String::len)).saturating_add(rendered_text.as_ref().map_or(0, String::len));
            if size > 16 * 1024 * 1024 { return Err(Error::new(StatusCode::PAYLOAD_TOO_LARGE, "Rendered comment page is too large")); }
            if profile != Representation::Full { value.as_object_mut().ok_or_else(|| Error::unavailable(()))?.remove("body"); }
            if let Some(html) = rendered_html { value["body_html"] = Value::String(html); }
            if let Some(text) = rendered_text { value["body_text"] = Value::String(text); }
        }
        Ok(values)
    }).await.map_err(Error::unavailable)?
}

async fn response_document(state: &AppState, headers: &HeaderMap, repo: &db::models::Repository, owner: &str, comment: &Comment) -> Result<Value> {
    let value = document(&state.pool, repo, owner, comment).await?;
    represent(state, headers, repo, owner, vec![value]).await?.pop().ok_or_else(Error::missing)
}

async fn read(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(p): Path<Params>,
) -> Result<Json<Value>> {
    let (repo, _) = repository(&state, &headers, &p["owner"], &p["repo"], false).await?;
    let comment = find(
        &state.pool,
        repo.id,
        p["comment_id"].parse().map_err(|_| Error::missing())?,
    )
    .await?;
    Ok(Json(
        response_document(&state, &headers, &repo, &p["owner"], &comment).await?,
    ))
}

#[derive(Deserialize)]
struct Body {
    body: String,
}

async fn create(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(p): Path<Params>,
    Json(input): Json<Body>,
) -> Result<(StatusCode, Json<Value>)> {
    let (repo, auth) = repository(&state, &headers, &p["owner"], &p["repo"], true).await?;
    let actor = auth.ok_or_else(auth::required)?.user;
    let item = labels::item(&state.pool, repo.id, &p["issue_number"]).await?;
    let number = p["issue_number"].parse().map_err(|_| Error::missing())?;
    let (id, pull) = match item {
        AssignedItem::Issue(_) => {
            let comment = crate::backend::issues::comments::create_issue_comment(
                &state.pool,
                &state.notification_broadcaster,
                actor.id,
                p["owner"].clone(),
                repo.name.clone(),
                number,
                CreateCommentRequest {
                    body: input.body,
                    repository_id: Some(repo.id),
                    issue_id: None,
                },
            )
            .await
            .map_err(issues::operation_error)?;
            (comment.id, false)
        }
        AssignedItem::PullRequest(_) => {
            let comment = comments::create_pull_request_comment(
                &state.pool,
                state.cluster.as_ref(),
                &state.notification_broadcaster,
                actor.id,
                p["owner"].clone(),
                repo.name.clone(),
                number,
                CreatePullRequestCommentRequest {
                    body: input.body,
                    repository_id: Some(repo.id),
                    path: None,
                    line: None,
                    side: None,
                    start_line: None,
                    start_side: None,
                    commit_sha: None,
                    base_sha: None,
                    in_reply_to_comment_id: None,
                    review_id: None,
                    pending: false,
                },
            )
            .await
            .map_err(issues::pull_error)?;
            (comment.id, true)
        }
    };
    let comment = find(
        &state.pool,
        repo.id,
        payloads::conversation_comment_id(id, pull),
    )
    .await?;
    Ok((
        StatusCode::CREATED,
        Json(response_document(&state, &headers, &repo, &p["owner"], &comment).await?),
    ))
}

async fn update(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(p): Path<Params>,
    Json(input): Json<Body>,
) -> Result<Json<Value>> {
    let (repo, auth) = repository(&state, &headers, &p["owner"], &p["repo"], true).await?;
    let actor = auth.ok_or_else(auth::required)?.user;
    let api_id = p["comment_id"].parse().map_err(|_| Error::missing())?;
    let comment = find(&state.pool, repo.id, api_id).await?;
    let target = CommentTarget {
        owner: p["owner"].clone(),
        repo: repo.name.clone(),
        number: comment.number,
        comment_id: comment.id,
    };
    match comment.item {
        AssignedItem::Issue(_) => {
            crate::backend::issues::comments::edit_issue_comment(
                &state.pool,
                &state.notification_broadcaster,
                actor.id,
                target,
                CreateCommentRequest {
                    body: input.body,
                    repository_id: Some(repo.id),
                    issue_id: None,
                },
                None,
            )
            .await
            .map_err(issues::operation_error)?;
        }
        AssignedItem::PullRequest(_) => {
            comments::edit_pull_request_comment(
                &state.pool,
                &state.notification_broadcaster,
                actor.id,
                target,
                input.body,
                None,
            )
            .await
            .map_err(issues::pull_error)?;
        }
    }
    let comment = find(&state.pool, repo.id, api_id).await?;
    Ok(Json(
        response_document(&state, &headers, &repo, &p["owner"], &comment).await?,
    ))
}

async fn remove(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(p): Path<Params>,
) -> Result<StatusCode> {
    let (repo, auth) = repository(&state, &headers, &p["owner"], &p["repo"], true).await?;
    let actor = auth.ok_or_else(auth::required)?.user;
    let comment = find(
        &state.pool,
        repo.id,
        p["comment_id"].parse().map_err(|_| Error::missing())?,
    )
    .await?;
    let target = CommentTarget {
        owner: p["owner"].clone(),
        repo: repo.name.clone(),
        number: comment.number,
        comment_id: comment.id,
    };
    match comment.item {
        AssignedItem::Issue(_) => {
            crate::backend::issues::comments::delete_issue_comment(&state.pool, actor.id, target)
                .await
                .map_err(issues::operation_error)?
        }
        AssignedItem::PullRequest(_) => {
            comments::delete_pull_request_comment(&state.pool, actor.id, target)
                .await
                .map_err(issues::pull_error)?
        }
    }
    Ok(StatusCode::NO_CONTENT)
}

async fn list(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(p): Path<Params>,
    Query(query): Query<HashMap<String, String>>,
    Query(page): Query<Pagination>,
    OriginalUri(uri): OriginalUri,
) -> Result<Response> {
    use db::discussions::ListSort;
    let (repo, _) = repository(&state, &headers, &p["owner"], &p["repo"], false).await?;
    let number = match p.get("issue_number") {
        Some(number) => {
            labels::item(&state.pool, repo.id, number).await?;
            Some(number.parse().map_err(|_| Error::missing())?)
        }
        None => None,
    };
    let ascending = match query.get("direction").map(String::as_str).unwrap_or("asc") {
        "asc" => true,
        "desc" => false,
        _ => return Err(Error::invalid("Invalid direction")),
    };
    let sort = match (
        query.get("sort").map(String::as_str).unwrap_or("created"),
        ascending,
    ) {
        ("created", true) => ListSort::Oldest,
        ("created", false) => ListSort::Newest,
        ("updated", true) => ListSort::LeastRecentlyUpdated,
        ("updated", false) => ListSort::RecentlyUpdated,
        _ => return Err(Error::invalid("Invalid sort")),
    };
    let since = query
        .get("since")
        .map(|date| {
            date.parse::<DateTime<Utc>>()
                .map_err(|_| Error::invalid("Invalid since timestamp"))
        })
        .transpose()?;
    let (offset, limit) = page.window()?;
    let rows = db::issues::list_repository_comments(
        &state.pool,
        repo.id,
        &db::issues::CommentFilter {
            number,
            since,
            sort,
            limit: (limit + 1) as i64,
            offset: offset as i64,
        },
    )
    .await
    .map_err(Error::unavailable)?;
    let next = rows.len() > limit;
    let mut items = Vec::new();
    for (id, pull) in rows.into_iter().take(limit) {
        let comment = find(
            &state.pool,
            repo.id,
            payloads::conversation_comment_id(id, pull),
        )
        .await?;
        items.push(document(&state.pool, &repo, &p["owner"], &comment).await?);
    }
    let items = represent(&state, &headers, &repo, &p["owner"], items).await?;
    page.response(&uri, items, next)
}

pub(super) fn handlers() -> Vec<(&'static str, MethodRouter<Arc<AppState>>)> {
    vec![
        ("issues/get-comment", get(read)),
        ("issues/create-comment", post(create)),
        ("issues/update-comment", patch(update)),
        ("issues/delete-comment", delete(remove)),
        ("issues/list-comments", get(list)),
        ("issues/list-comments-for-repo", get(list)),
    ]
}
