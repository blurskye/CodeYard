use super::{
    auth, contents::store_error, pagination::Pagination, repo_url, repository, Error, Result,
};
use crate::backend::{
    app::AppState,
    db::{models::Repository, repositories::commit_comments as records},
    repositories::{
        commit_comments::{self as comments, CommentDetails, CommentError},
        store::repo_store,
    },
    webhooks::payloads,
};
use axum::{
    extract::{FromRequest, OriginalUri, Path, Query, Request, State},
    http::{header, HeaderMap, Method, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, MethodRouter},
    Json,
};
use serde::Deserialize;
use serde_json::Value;
use std::{collections::HashMap, sync::Arc};

fn operation_error(error: CommentError) -> Error {
    match error {
        CommentError::Missing => Error::missing(),
        CommentError::Denied => Error::new(StatusCode::FORBIDDEN, "Commit comment write denied"),
        CommentError::Conflict => Error::new(
            StatusCode::CONFLICT,
            "The comment changed; reload before saving",
        ),
        CommentError::Invalid(message) => Error::invalid(message),
        CommentError::Unavailable => Error::unavailable(()),
    }
}

fn document(
    entry: &CommentDetails,
    repo: &Repository,
    owner: &str,
    counts: &[crate::backend::db::reactions::ReactionCount],
) -> Value {
    let counts = counts
        .iter()
        .filter(|count| count.subject_id == entry.comment.id)
        .cloned()
        .collect::<Vec<_>>();
    payloads::commit_comment(
        &entry.comment,
        entry.author.as_ref(),
        &entry.association,
        repo,
        owner,
        &counts,
    )
}
async fn document_one(
    pool: &crate::backend::db::DbPool,
    entry: &CommentDetails,
    repo: &Repository,
    owner: &str,
) -> Result<Value> {
    let counts = crate::backend::db::reactions::reaction_counts(
        pool,
        "commit_comment",
        &[entry.comment.id],
        0,
    )
    .await
    .map_err(Error::unavailable)?;
    Ok(document(entry, repo, owner, &counts))
}

async fn writer(
    state: &AppState,
    headers: &HeaderMap,
    owner: &str,
    name: &str,
) -> Result<(Repository, crate::backend::db::credentials::Actor)> {
    // Comment participation needs read access, not permission to push to the repository.
    let (repo, auth) = repository(state, headers, owner, name, false).await?;
    let auth = auth.ok_or_else(super::auth::required)?;
    auth.require_write()?;
    Ok((repo, auth.database_actor()))
}

#[derive(Deserialize)]
struct Create {
    body: String,
    path: Option<String>,
    position: Option<i32>,
    line: Option<i32>,
}

async fn create(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, name, reference)): Path<(String, String, String)>,
    request: Request,
) -> Result<Response> {
    let (repo, actor) = writer(&state, &headers, &owner, &name).await?;
    let Json(input) = Json::<Create>::from_request(request, &state)
        .await
        .map_err(|error| Error::new(error.status(), "Invalid commit comment"))?;
    let entry = comments::create(
        &state.pool,
        state.cluster.as_ref(),
        &state.notification_broadcaster,
        actor,
        repo.id,
        &reference,
        comments::CreateComment {
            body: input.body,
            path: input.path,
            position: input.position,
            line: input.line,
            repository_id: None,
        },
    )
    .await
    .map_err(operation_error)?;
    Ok((
        StatusCode::CREATED,
        [(
            header::LOCATION,
            format!("{}/comments/{}", repo_url(&owner, &name), entry.comment.id),
        )],
        Json(document_one(&state.pool, &entry, &repo, &owner).await?),
    )
        .into_response())
}

#[derive(Deserialize)]
struct Update {
    body: String,
}

async fn item(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, name, id)): Path<(String, String, i64)>,
    request: Request,
) -> Result<Response> {
    let method = request.method().clone();
    let reading = auth::read_method(&method);
    let (repo, authentication) = repository(&state, &headers, &owner, &name, false).await?;
    let actor = if reading {
        None
    } else {
        let auth = authentication.ok_or_else(auth::required)?;
        auth.require_write()?;
        Some(auth.database_actor())
    };
    let comment = records::get_comment(&state.pool, repo.id, id)
        .await
        .map_err(Error::unavailable)?
        .ok_or_else(Error::missing)?;
    if reading {
        let entry =
            comments::comment_details(&state.pool, state.cluster.as_ref(), &repo, vec![comment])
                .await
                .map_err(operation_error)?
                .pop()
                .ok_or_else(|| Error::unavailable(()))?;
        return Ok(Json(document_one(&state.pool, &entry, &repo, &owner).await?).into_response());
    }
    let actor = actor.ok_or_else(auth::required)?;
    if method == Method::DELETE {
        comments::delete(&state.pool, actor, repo.id, id, None)
            .await
            .map_err(operation_error)?;
        return Ok(StatusCode::NO_CONTENT.into_response());
    }
    let Json(input) = Json::<Update>::from_request(request, &state)
        .await
        .map_err(|error| Error::new(error.status(), "Invalid commit comment"))?;
    let entry = comments::edit(
        &state.pool,
        state.cluster.as_ref(),
        &state.notification_broadcaster,
        records::CommentEdit {
            actor_id: actor.user_id,
            token_id: actor.token_id(),
            repo_id: repo.id,
            id,
            body: &input.body,
            expected_revision: None,
        },
    )
    .await
    .map_err(operation_error)?;
    Ok(Json(document_one(&state.pool, &entry, &repo, &owner).await?).into_response())
}

async fn list(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(params): Path<HashMap<String, String>>,
    Query(page): Query<Pagination>,
    OriginalUri(uri): OriginalUri,
) -> Result<Response> {
    let (owner, name) = (&params["owner"], &params["repo"]);
    let (repo, _) = repository(&state, &headers, owner, name, false).await?;
    let sha = if let Some(reference) = params.get("commit_sha") {
        Some(
            repo_store(&repo, state.cluster.as_ref())
                .commit(reference)
                .await
                .map_err(store_error)?
                .sha,
        )
    } else {
        None
    };
    let (offset, limit) = page.window()?;
    let rows = records::list_comments(
        &state.pool,
        repo.id,
        sha.as_deref(),
        offset as i64,
        limit as i64,
    )
    .await
    .map_err(Error::unavailable)?;
    let has_next = rows.len() == limit
        && offset + limit <= 1_000_000
        && !records::list_comments(
            &state.pool,
            repo.id,
            sha.as_deref(),
            (offset + limit) as i64,
            1,
        )
        .await
        .map_err(Error::unavailable)?
        .is_empty();
    let entries = comments::comment_details(&state.pool, state.cluster.as_ref(), &repo, rows)
        .await
        .map_err(operation_error)?;
    let ids = entries
        .iter()
        .map(|entry| entry.comment.id)
        .collect::<Vec<_>>();
    let counts =
        crate::backend::db::reactions::reaction_counts(&state.pool, "commit_comment", &ids, 0)
            .await
            .map_err(Error::unavailable)?;
    page.response(
        &uri,
        entries
            .iter()
            .map(|entry| document(entry, &repo, owner, &counts))
            .collect(),
        has_next,
    )
}

pub(super) fn handlers() -> Vec<(&'static str, MethodRouter<Arc<AppState>>)> {
    vec![
        ("repos/list-commit-comments-for-repo", get(list)),
        ("repos/list-comments-for-commit", get(list)),
        ("repos/create-commit-comment", axum::routing::post(create)),
        ("repos/get-commit-comment", get(item)),
        ("repos/update-commit-comment", axum::routing::patch(item)),
        ("repos/delete-commit-comment", axum::routing::delete(item)),
    ]
}
