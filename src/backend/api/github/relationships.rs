use super::{auth, issues, pagination::Pagination, repository, Error, Result};
use crate::backend::{
    app::AppState,
    db::{
        self,
        assignees::AssignedItem,
        relationships::{Kind, Mutation, Refusal},
    },
};
use axum::{
    body::Bytes,
    extract::{OriginalUri, Path, Query, State},
    http::{HeaderMap, StatusCode, Uri},
    response::{IntoResponse, Response},
    routing::{delete, get, patch, post, MethodRouter},
    Json,
};
use serde::Deserialize;
use serde_json::Value;
use std::{collections::HashMap, sync::Arc};
type Params = HashMap<String, String>;
pub(super) fn error(error: anyhow::Error) -> Error {
    match error.downcast_ref::<Refusal>() {
        Some(Refusal::Missing) => Error::missing(),
        Some(Refusal::Denied) => Error::new(StatusCode::FORBIDDEN, "Relationship mutation denied"),
        Some(reason) => Error::invalid(reason.to_string()),
        None => Error::unavailable(error),
    }
}
async fn root(pool: &db::DbPool, repo: i64, p: &Params) -> Result<i64> {
    issues::required_issue(pool, repo, &p["issue_number"]).await
}
/// A linked issue may sit in another repository, which the credential must reach as well.
async fn document(state: &AppState, auth: Option<&auth::Authentication>, id: i64) -> Result<Value> {
    let issue = db::issues::get_issue_by_id(&state.pool, id)
        .await
        .map_err(Error::unavailable)?
        .ok_or_else(Error::missing)?;
    if auth::readable_repositories(state, auth, &[issue.repo_id], "issues")
        .await?
        .is_empty()
    {
        return Err(Error::missing());
    }
    let repo = db::repositories::records::get_repository_by_id(&state.pool, issue.repo_id)
        .await
        .map_err(Error::unavailable)?
        .ok_or_else(Error::missing)?;
    let owners = db::repositories::records::repository_owner_names(&state.pool, &[repo.id])
        .await
        .map_err(Error::unavailable)?;
    issues::document(
        &state.pool,
        issues::Reader::Request(state, auth),
        &repo,
        owners.get(&repo.id).ok_or_else(Error::missing)?,
        AssignedItem::Issue(id),
    )
    .await
}
fn kind(uri: &Uri) -> Kind {
    // Only the operation and its optional numeric target follow the discussion number.
    // Namespace segments are user-controlled and must never select the operation.
    let tail: Vec<_> = uri.path().rsplit('/').take(2).collect();
    if tail.contains(&"blocked_by") {
        Kind::BlockedBy
    } else if tail[0] == "blocking" {
        Kind::Blocking
    } else if tail.contains(&"relates_to") {
        Kind::Related
    } else if tail[0] == "parent" {
        Kind::Parent
    } else {
        Kind::Children
    }
}
async fn list(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(p): Path<Params>,
    Query(page): Query<Pagination>,
    OriginalUri(uri): OriginalUri,
) -> Result<Response> {
    let (repo, auth) = repository(&state, &headers, &p["owner"], &p["repo"], false).await?;
    let viewer = auth.as_ref().map(|a| a.user.id);
    let root = root(&state.pool, repo.id, &p).await?;
    let kind = kind(&uri);
    let (offset, limit) = page.window()?;
    let mut rows = Vec::new();
    let (mut scanned, mut skipped) = (0, 0);
    loop {
        let batch = db::relationships::list(&state.pool, viewer, root, kind, scanned, 100)
            .await.map_err(Error::unavailable)?;
        let done = batch.len() < 100;
        scanned += batch.len() as i64;
        let mut ids: Vec<_> = batch.iter().map(|link| link.repo_id).collect();
        ids.sort_unstable();
        ids.dedup();
        let readable = auth::readable_repositories(&state, auth.as_ref(), &ids, "issues").await?;
        for row in batch.into_iter().filter(|link| readable.contains(&link.repo_id)) {
            if kind != Kind::Parent && skipped < offset {
                skipped += 1;
                continue;
            }
            rows.push(row);
            if rows.len() > limit || kind == Kind::Parent {
                break;
            }
        }
        if done || rows.len() > limit || kind == Kind::Parent {
            break;
        }
    }
    let next = rows.len() > limit;
    rows.truncate(limit);
    if kind == Kind::Parent {
        let link = rows.first().ok_or_else(Error::missing)?;
        return Ok(Json(document(&state, auth.as_ref(), link.id).await?).into_response());
    }
    let mut values = Vec::new();
    for link in rows {
        values.push(document(&state, auth.as_ref(), link.id).await?);
    }
    page.response(&uri, values, next)
}
#[derive(Default, Deserialize)]
struct Input {
    issue_id: Option<i64>,
    sub_issue_id: Option<i64>,
    #[serde(default)]
    replace_parent: bool,
    before_id: Option<i64>,
    after_id: Option<i64>,
}
fn input(body: &[u8]) -> Result<Input> {
    serde_json::from_slice(body).map_err(|_| Error::validation("Issue", "relationship", "invalid"))
}
async fn apply(
    state: &AppState,
    headers: &HeaderMap,
    p: &Params,
    mutation: Mutation,
) -> Result<Value> {
    let (repo, auth) = repository(state, headers, &p["owner"], &p["repo"], true).await?;
    let auth = auth.ok_or_else(auth::required)?;
    let root = root(&state.pool, repo.id, p).await?;
    let other = match mutation {
        Mutation::Add { other, .. } | Mutation::Remove { other, .. } => other,
        Mutation::Move { child, .. } => child,
    };
    // The linked issue's repository must be within the credential's reach before the change.
    document(state, Some(&auth), other).await?;
    db::relationships::change(&state.pool, auth.user.id, repo.id, root, mutation)
        .await
        .map_err(error)?;
    document(state, Some(&auth), other).await
}
async fn add(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(p): Path<Params>,
    OriginalUri(uri): OriginalUri,
    body: Bytes,
) -> Result<Response> {
    let value = input(&body)?;
    let kind = kind(&uri);
    let other = if kind == Kind::Children {
        value.sub_issue_id
    } else {
        value.issue_id
    }
    .ok_or_else(|| Error::validation("Issue", "issue_id", "missing_field"))?;
    let value = apply(
        &state,
        &headers,
        &p,
        Mutation::Add {
            kind,
            other,
            replace: value.replace_parent,
        },
    )
    .await?;
    let mut response = (StatusCode::CREATED, Json(value)).into_response();
    response.headers_mut().insert(
        axum::http::header::LOCATION,
        format!(
            "{}{}",
            crate::backend::api::http::config::link_origin(),
            uri.path()
        )
        .parse()
        .map_err(Error::unavailable)?,
    );
    Ok(response)
}
async fn remove(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(p): Path<Params>,
    OriginalUri(uri): OriginalUri,
    body: Bytes,
) -> Result<Json<Value>> {
    let kind = kind(&uri);
    let other = if kind == Kind::Children {
        input(&body)?
            .sub_issue_id
            .ok_or_else(|| Error::validation("Issue", "sub_issue_id", "missing_field"))?
    } else {
        p["issue_id"].parse().map_err(|_| Error::missing())?
    };
    Ok(Json(
        apply(&state, &headers, &p, Mutation::Remove { kind, other }).await?,
    ))
}
async fn prioritize(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(p): Path<Params>,
    body: Bytes,
) -> Result<Json<Value>> {
    let input = input(&body)?;
    let child = input
        .sub_issue_id
        .ok_or_else(|| Error::validation("Issue", "sub_issue_id", "missing_field"))?;
    Ok(Json(
        apply(
            &state,
            &headers,
            &p,
            Mutation::Move {
                child,
                before: input.before_id,
                after: input.after_id,
            },
        )
        .await?,
    ))
}
pub(super) fn handlers() -> Vec<(&'static str, MethodRouter<Arc<AppState>>)> {
    vec![
        ("issues/list-dependencies-blocked-by", get(list)),
        ("issues/list-dependencies-blocking", get(list)),
        ("issues/list-relates-to", get(list)),
        ("issues/list-sub-issues", get(list)),
        ("issues/get-parent", get(list)),
        ("issues/add-blocked-by-dependency", post(add)),
        ("issues/add-relates-to", post(add)),
        ("issues/add-sub-issue", post(add)),
        ("issues/remove-dependency-blocked-by", delete(remove)),
        ("issues/remove-relates-to", delete(remove)),
        ("issues/remove-sub-issue", delete(remove)),
        ("issues/reprioritize-sub-issue", patch(prioritize)),
    ]
}
