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
use codeyard_db_repos::assignees::AssignedItem;
use codeyard_repos_pulls::issues::comments::CommentTarget;
use codeyard_repos_pulls::issues::comments::CreateCommentRequest;
use codeyard_repos_pulls::repositories::pull_requests::comments::CreatePullRequestCommentRequest;
use codeyard_types::github_error::Error;
use codeyard_types::github_error::Result;
use serde::Deserialize;
use serde_json::Value;
use std::collections::HashMap;
use std::sync::Arc;

type Params = HashMap<String, String>;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Representation {
    Raw,
    Text,
    Html,
    Full,
}

fn representation(headers: &HeaderMap) -> Representation {
    let mut selected = (0.0_f32, Representation::Raw);
    for header in headers.get_all(axum::http::header::ACCEPT).iter() {
        let Ok(header) = header.to_str() else {
            continue;
        };
        for range in header.split(',') {
            let mut parts = range.split(';').map(str::trim);
            let media = parts.next().unwrap_or("").to_ascii_lowercase();
            let profile = match media.as_str() {
                "application/vnd.github.text+json" | "application/vnd.github.v3.text+json" => {
                    Representation::Text
                }
                "application/vnd.github.html+json" | "application/vnd.github.v3.html+json" => {
                    Representation::Html
                }
                "application/vnd.github.full+json" | "application/vnd.github.v3.full+json" => {
                    Representation::Full
                }
                "application/vnd.github.raw+json"
                | "application/vnd.github.v3.raw+json"
                | "application/vnd.github+json"
                | "application/json"
                | "*/*" => Representation::Raw,
                _ => continue,
            };
            let weight = parts
                .find_map(|part| part.strip_prefix("q="))
                .map_or(1.0, |weight| weight.parse::<f32>().unwrap_or(0.0));
            if weight > selected.0 && weight <= 1.0 {
                selected = (weight, profile);
            }
        }
    }
    selected.1
}

async fn represent(
    state: &AppState,
    headers: &HeaderMap,
    repo: &codeyard_database::models::Repository,
    owner: &str,
    mut values: Vec<Value>,
) -> Result<Vec<Value>> {
    let profile = representation(headers);
    if profile == Representation::Raw {
        return Ok(values);
    }
    let html = matches!(profile, Representation::Html | Representation::Full);
    let text = matches!(profile, Representation::Text | Representation::Full);
    let references = if html {
        let authentication =
            codeyard_identity::github_auth::authenticate(state, headers, false).await?;
        let texts: Vec<_> = values
            .iter()
            .filter_map(|value| value["body"].as_str())
            .collect();
        crate::meta::resolve_references(
            state,
            authentication.as_ref(),
            repo,
            owner,
            &repo.name,
            &texts,
            false,
        )
        .await?
    } else {
        Default::default()
    };
    tokio::task::spawn_blocking(move || {
        let mut size = 0_usize;
        for value in &mut values {
            let body = value["body"].as_str().unwrap_or_default();
            let rendered_html =
                html.then(|| codeyard_webui::components::parse_markdown(body, &references));
            let rendered_text = text.then(|| codeyard_webui::components::markdown_text(body));
            size = size
                .saturating_add(if profile == Representation::Full {
                    body.len()
                } else {
                    0
                })
                .saturating_add(rendered_html.as_ref().map_or(0, String::len))
                .saturating_add(rendered_text.as_ref().map_or(0, String::len));
            if size > 16 * 1024 * 1024 {
                return Err(Error::new(
                    StatusCode::PAYLOAD_TOO_LARGE,
                    "Rendered comment page is too large",
                ));
            }
            if profile != Representation::Full {
                value
                    .as_object_mut()
                    .ok_or_else(|| Error::unavailable(()))?
                    .remove("body");
            }
            if let Some(html) = rendered_html {
                value["body_html"] = Value::String(html);
            }
            if let Some(text) = rendered_text {
                value["body_text"] = Value::String(text);
            }
        }
        Ok(values)
    })
    .await
    .map_err(Error::unavailable)?
}

async fn response_document(
    state: &AppState,
    headers: &HeaderMap,
    repo: &codeyard_database::models::Repository,
    owner: &str,
    comment: &Comment,
) -> Result<Value> {
    let value = document(&state.pool, repo, owner, comment).await?;
    represent(state, headers, repo, owner, vec![value])
        .await?
        .pop()
        .ok_or_else(Error::missing)
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
    let auth = auth.ok_or_else(codeyard_identity::github_auth::required)?;
    let (actor, credential) = (&auth.user, auth.credential());
    let item =
        codeyard_app::api_shared::github::labels::item(&state.pool, repo.id, &p["issue_number"])
            .await?;
    let number = p["issue_number"].parse().map_err(|_| Error::missing())?;
    let (id, pull) = match item {
        AssignedItem::Issue(_) => {
            let comment = codeyard_repos_pulls::issues::comments::create_issue_comment(
                &state.pool,
                &state.notification_broadcaster,
                actor.id,
                &credential,
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
            .map_err(crate::issues::operation_error)?;
            (comment.id, false)
        }
        AssignedItem::PullRequest(_) => {
            let comment =
                codeyard_repos_pulls::repositories::pull_requests::comments::create_pull_request_comment(
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
                .map_err(crate::issues::pull_error)?;
            (comment.id, true)
        }
    };
    let comment = find(
        &state.pool,
        repo.id,
        codeyard_types::webhook_ids::conversation_comment_id(id, pull),
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
    let auth = auth.ok_or_else(codeyard_identity::github_auth::required)?;
    let (actor, credential) = (&auth.user, auth.credential());
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
            codeyard_repos_pulls::issues::comments::edit_issue_comment(
                &state.pool,
                &state.notification_broadcaster,
                actor.id,
                &credential,
                target,
                CreateCommentRequest {
                    body: input.body,
                    repository_id: Some(repo.id),
                    issue_id: None,
                },
                None,
            )
            .await
            .map_err(crate::issues::operation_error)?;
        }
        AssignedItem::PullRequest(_) => {
            codeyard_repos_pulls::repositories::pull_requests::comments::edit_pull_request_comment(
                &state.pool,
                &state.notification_broadcaster,
                actor.id,
                target,
                input.body,
                None,
            )
            .await
            .map_err(crate::issues::pull_error)?;
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
    let auth = auth.ok_or_else(codeyard_identity::github_auth::required)?;
    let (actor, credential) = (&auth.user, auth.credential());
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
        AssignedItem::Issue(_) => codeyard_repos_pulls::issues::comments::delete_issue_comment(
            &state.pool,
            actor.id,
            &credential,
            target,
        )
        .await
        .map_err(crate::issues::operation_error)?,
        AssignedItem::PullRequest(_) => {
            codeyard_repos_pulls::repositories::pull_requests::comments::delete_pull_request_comment(
                &state.pool,
                actor.id,
                target,
            )
            .await
            .map_err(crate::issues::pull_error)?
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
    use codeyard_db_repos::discussions::ListSort;
    let (repo, _) = repository(&state, &headers, &p["owner"], &p["repo"], false).await?;
    let number = match p.get("issue_number") {
        Some(number) => {
            codeyard_app::api_shared::github::labels::item(&state.pool, repo.id, number).await?;
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
    let rows = codeyard_db_repos::issues::list_repository_comments(
        &state.pool,
        repo.id,
        &codeyard_db_repos::issues::CommentFilter {
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
    let comments: Vec<Comment> = rows.into_iter().take(limit).map(Comment::from).collect();
    let items = documents(&state.pool, &repo, &p["owner"], &comments).await?;
    let items = represent(&state, &headers, &repo, &p["owner"], items).await?;
    page.response(&uri, items, next)
}

pub(crate) fn handlers() -> Vec<(&'static str, MethodRouter<Arc<AppState>>)> {
    vec![
        ("issues/get-comment", get(read)),
        ("issues/create-comment", post(create)),
        ("issues/update-comment", patch(update)),
        ("issues/delete-comment", delete(remove)),
        ("issues/list-comments", get(list)),
        ("issues/list-comments-for-repo", get(list)),
    ]
}

use codeyard_webhooks::comment_documents::Comment;

use codeyard_webhooks::comment_documents::document;

use codeyard_webhooks::comment_documents::documents;

use codeyard_webhooks::comment_documents::find;
