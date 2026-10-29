//! The Git database API does not move refs when it creates objects.
use super::{contents::store_error, repo_url, repository, Error, Result};
use crate::backend::{
    app::AppState,
    git::objects::{
        ObjectWrite, Signature, TagObject, TreeChange, TreeObject, MAX_BLOB_WRITE_BYTES,
    },
    repositories::store::repo_store,
};
use axum::{
    extract::{DefaultBodyLimit, FromRequest, Path, Query, Request, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post, MethodRouter},
    Json,
};
use base64::{engine::general_purpose::STANDARD, Engine};
use serde::Deserialize;
use serde_json::{json, Value};
use std::{collections::HashMap, sync::Arc};

pub(crate) const BLOB_ROUTE: &str = "/api/v3/repos/:owner/:repo/git/blobs";
// JSON can escape each input byte as six ASCII characters.
const BLOB_BODY_LIMIT: usize = MAX_BLOB_WRITE_BYTES * 6 + 64 * 1024;

fn tree_document(tree: TreeObject, owner: &str, name: &str) -> Value {
    let url = repo_url(owner, name);
    json!({
        "sha": tree.sha, "url": format!("{url}/git/trees/{}", tree.sha), "truncated": tree.truncated,
        "tree": tree.tree.into_iter().map(|entry| {
            let mut value = json!({"path": entry.path, "mode": entry.mode, "type": entry.kind, "sha": entry.sha,
                "url": format!("{url}/git/{}s/{}", entry.kind, entry.sha)});
            if let Some(size) = entry.size { value["size"] = json!(size); }
            value
        }).collect::<Vec<_>>(),
    })
}

async fn tag_document(state: &AppState, tag: TagObject, owner: &str, name: &str) -> Result<Value> {
    let url = repo_url(owner, name);
    let verification = crate::backend::identity::signatures::verification(
        &state.pool,
        crate::backend::identity::signatures::Signed {
            signature: tag.signature.as_deref(),
            payload: tag.signature_payload.as_deref(),
            email: tag.tagger.as_ref().map(|tagger| tagger.email.as_str()),
        },
    )
    .await
    .map_err(Error::unavailable)?;
    Ok(json!({
        "sha": tag.sha, "node_id": super::node_id::git("TAG", &tag.sha), "url": format!("{url}/git/tags/{}", tag.sha),
        "tag": tag.tag, "message": tag.message, "tagger": tag.tagger,
        "object": {"sha": tag.object, "type": tag.kind, "url": format!("{url}/git/{}s/{}", tag.kind, tag.object)},
        "verification": verification,
    }))
}

async fn tree(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, name, sha)): Path<(String, String, String)>,
    Query(query): Query<HashMap<String, String>>,
) -> Result<Json<Value>> {
    let (repo, auth) = repository(&state, &headers, &owner, &name, false).await?;
    let tree = repo_store(&repo, state.cluster.as_ref())
        .git_tree(&sha, query.contains_key("recursive"))
        .await
        .map_err(store_error)?;
    super::auth::revalidate_repository(&state, auth.as_ref(), repo.id, "contents").await?;
    Ok(Json(tree_document(tree, &owner, &name)))
}

async fn tag(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, name, sha)): Path<(String, String, String)>,
) -> Result<Json<Value>> {
    let (repo, _) = repository(&state, &headers, &owner, &name, false).await?;
    let tag = repo_store(&repo, state.cluster.as_ref())
        .git_tag(&sha)
        .await
        .map_err(store_error)?;
    Ok(Json(tag_document(&state, tag, &owner, &name).await?))
}

#[derive(Deserialize)]
struct Blob {
    content: String,
    encoding: Option<String>,
}

async fn create_blob(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, name)): Path<(String, String)>,
    request: Request,
) -> Result<Response> {
    let (repo, auth) = repository(&state, &headers, &owner, &name, true).await?;
    let actor = auth.ok_or_else(super::auth::required)?.user.id;
    crate::backend::repositories::access::check_write(&state.pool, repo.id, actor)
        .await
        .map_err(|error| match error {
            crate::backend::repositories::access::AccessError::Unavailable => {
                Error::unavailable(())
            }
            _ => Error::new(StatusCode::FORBIDDEN, "Repository write denied"),
        })?;
    let _permit = state
        .upload_reads
        .clone()
        .try_acquire_owned()
        .map_err(|_| Error::unavailable(()))?;
    let Json(input) = tokio::time::timeout(
        super::super::repos::code::upload::READ_TIMEOUT,
        Json::<Blob>::from_request(request, &state),
    )
    .await
    .map_err(|_| Error::new(StatusCode::REQUEST_TIMEOUT, "Upload timed out"))?
    .map_err(|error| Error::new(error.status(), "Invalid blob request"))?;
    let bytes = match input.encoding.as_deref().unwrap_or("utf-8") {
        "utf-8" => input.content.into_bytes(),
        "base64" => STANDARD
            .decode(input.content.split_whitespace().collect::<String>())
            .map_err(|_| Error::invalid("Invalid base64 content"))?,
        _ => return Err(Error::invalid("encoding must be utf-8 or base64")),
    };
    if bytes.len() > MAX_BLOB_WRITE_BYTES {
        return Err(Error::invalid("Blobs must not exceed 100 MiB"));
    }
    let store = repo_store(&repo, state.cluster.as_ref());
    let sha = store
        .write_git_object_as(&state.pool, repo.id, actor, ObjectWrite::Blob, &bytes)
        .await
        .map_err(write_error)?;
    Ok((
        StatusCode::CREATED,
        Json(json!({"sha": sha, "url": format!("{}/git/blobs/{sha}", repo_url(&owner, &name))})),
    )
        .into_response())
}

fn write_error(error: crate::backend::repositories::store::RepoError) -> Error {
    if !error.is_write_denied()
        && matches!(
            error.kind,
            crate::backend::repositories::store::ErrorKind::NotFound
                | crate::backend::repositories::store::ErrorKind::Invalid
        )
    {
        return Error::invalid("Invalid Git object");
    }
    store_error(error)
}

#[derive(Deserialize)]
struct Tree {
    base_tree: Option<String>,
    tree: Vec<TreeChange>,
}

async fn create_tree(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, name)): Path<(String, String)>,
    Json(input): Json<Tree>,
) -> Result<Response> {
    let (repo, auth) = repository(&state, &headers, &owner, &name, true).await?;
    let actor = auth.ok_or_else(super::auth::required)?.user.id;
    let store = repo_store(&repo, state.cluster.as_ref());
    let sha = store
        .write_git_object_as(
            &state.pool,
            repo.id,
            actor,
            ObjectWrite::Tree {
                base_tree: input.base_tree,
                tree: input.tree,
            },
            b"",
        )
        .await
        .map_err(write_error)?;
    let tree = store.git_tree(&sha, false).await.map_err(store_error)?;
    Ok((
        StatusCode::CREATED,
        Json(tree_document(tree, &owner, &name)),
    )
        .into_response())
}

#[derive(Deserialize)]
struct Commit {
    message: String,
    tree: String,
    #[serde(default)]
    parents: Vec<String>,
    author: Option<Signature>,
    committer: Option<Signature>,
    signature: Option<String>,
}

async fn create_commit(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, name)): Path<(String, String)>,
    Json(input): Json<Commit>,
) -> Result<Response> {
    let (repo, auth) = repository(&state, &headers, &owner, &name, true).await?;
    let actor = auth.ok_or_else(super::auth::required)?.user;
    let author = input.author.unwrap_or(Signature {
        name: actor.display_name.unwrap_or_else(|| actor.username.clone()),
        email: actor
            .email
            .unwrap_or_else(|| format!("{}@users.noreply", actor.username)),
        date: None,
    });
    let committer = input.committer.unwrap_or_else(|| author.clone());
    let store = repo_store(&repo, state.cluster.as_ref());
    let sha = store
        .write_git_object_as(
            &state.pool,
            repo.id,
            actor.id,
            ObjectWrite::Commit {
                message: input.message,
                tree: input.tree,
                parents: input.parents,
                author,
                committer,
                signature: input.signature,
            },
            b"",
        )
        .await
        .map_err(write_error)?;
    let commit = store.commit(&sha).await.map_err(store_error)?;
    Ok((
        StatusCode::CREATED,
        Json(super::commits::git_document(&state, &commit, &owner, &name).await?),
    )
        .into_response())
}

#[derive(Deserialize)]
struct Tag {
    tag: String,
    message: String,
    object: String,
    #[serde(rename = "type")]
    kind: String,
    tagger: Option<Signature>,
}

async fn create_tag(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, name)): Path<(String, String)>,
    Json(input): Json<Tag>,
) -> Result<Response> {
    let (repo, auth) = repository(&state, &headers, &owner, &name, true).await?;
    let actor = auth.ok_or_else(super::auth::required)?.user;
    let tagger = input.tagger.unwrap_or(Signature {
        name: actor.display_name.unwrap_or_else(|| actor.username.clone()),
        email: actor
            .email
            .unwrap_or_else(|| format!("{}@users.noreply", actor.username)),
        date: None,
    });
    let store = repo_store(&repo, state.cluster.as_ref());
    let sha = store
        .write_git_object_as(
            &state.pool,
            repo.id,
            actor.id,
            ObjectWrite::Tag {
                tag: input.tag,
                message: input.message,
                object: input.object,
                kind: input.kind,
                tagger,
            },
            b"",
        )
        .await
        .map_err(write_error)?;
    let tag = store.git_tag(&sha).await.map_err(store_error)?;
    Ok((
        StatusCode::CREATED,
        Json(tag_document(&state, tag, &owner, &name).await?),
    )
        .into_response())
}

pub(super) fn handlers() -> Vec<(&'static str, MethodRouter<Arc<AppState>>)> {
    vec![
        ("git/get-tree", get(tree)),
        ("git/get-tag", get(tag)),
        (
            "git/create-blob",
            post(create_blob).layer(DefaultBodyLimit::max(BLOB_BODY_LIMIT)),
        ),
        ("git/create-tree", post(create_tree)),
        ("git/create-commit", post(create_commit)),
        ("git/create-tag", post(create_tag)),
    ]
}
