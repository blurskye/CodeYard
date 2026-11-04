use super::{contents::store_error, pagination::Pagination, repo_url, repository, Error, Result};
use crate::backend::{
    app::AppState,
    repositories::{refs, store::repo_store},
};
use axum::{
    extract::{OriginalUri, Path, Query, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{delete, get, patch, post, MethodRouter},
    Json, Router,
};
use serde::Deserialize;
use serde_json::{json, Value};
use std::{collections::HashMap, sync::Arc};

fn reference(
    repo_id: i64,
    owner: &str,
    name: &str,
    reference: &str,
    sha: &str,
    kind: &str,
) -> Value {
    json!({
        "ref": reference,
        "node_id": super::node_id::reference(repo_id, reference),
        "url": format!("{}/git/refs/{}", repo_url(owner, name), super::contents::path_url(reference.strip_prefix("refs/").unwrap_or(reference))),
        "object": {"type": kind, "sha": sha, "url": format!("{}/git/{}s/{sha}", repo_url(owner, name), kind)},
    })
}

async fn read(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(params): Path<HashMap<String, String>>,
) -> Result<Json<Value>> {
    let (owner, name) = (&params["owner"], &params["repo"]);
    let (repo, _) = repository(&state, &headers, owner, name, false).await?;
    let full = format!("refs/{}", params["ref"]);
    let store = repo_store(&repo, state.cluster.as_ref());
    let refs = store.refs().await.map_err(store_error)?;
    let sha = refs.get(&full).ok_or_else(Error::missing)?;
    let kind = object_kind(&store, &full, sha).await?;
    Ok(Json(reference(repo.id, owner, name, &full, sha, &kind)))
}

async fn object_kind(
    store: &crate::backend::repositories::store::RepoStore,
    full: &str,
    sha: &str,
) -> Result<String> {
    if full.starts_with("refs/heads/") {
        return Ok("commit".into());
    }
    store.git_object_kind(sha).await.map_err(store_error)
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
    let (offset, limit) = page.window()?;
    let prefix = format!(
        "refs/{}",
        params.get("ref").map(String::as_str).unwrap_or("")
    );
    let store = repo_store(&repo, state.cluster.as_ref());
    let mut refs: Vec<_> = store
        .matching_refs(&prefix)
        .await
        .map_err(store_error)?
        .into_iter()
        .filter(|(name, _)| name.starts_with(&prefix))
        .collect();
    refs.sort_by(|a, b| a.0.cmp(&b.0));
    let has_next = refs.len() > offset + limit;
    let mut items = Vec::new();
    for (full, sha) in refs.into_iter().skip(offset).take(limit) {
        let kind = object_kind(&store, &full, &sha).await?;
        items.push(reference(repo.id, owner, name, &full, &sha, &kind));
    }
    page.response(&uri, items, has_next)
}

#[derive(Deserialize)]
struct Create {
    #[serde(rename = "ref")]
    reference: String,
    sha: String,
}

pub(super) fn ref_error(error: refs::RefError) -> Error {
    use refs::RefError;
    match error {
        RefError::Git(error)
            if matches!(
                error.kind,
                codeyard_git::error::ErrorKind::Conflict | codeyard_git::error::ErrorKind::NotFound
            ) =>
        {
            Error::invalid("Invalid reference target or reference already exists")
        }
        RefError::Git(error) => store_error(error),
        RefError::Access(crate::backend::repositories::access::AccessError::Unavailable)
        | RefError::Unavailable
        | RefError::AppliedFollowupFailed => Error::unavailable(()),
        RefError::Access(_) | RefError::Protected(_) => {
            Error::new(StatusCode::FORBIDDEN, "Reference write denied")
        }
        RefError::Invalid(message) => Error::invalid(message),
    }
}

async fn create(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, name)): Path<(String, String)>,
    Json(input): Json<Create>,
) -> Result<Response> {
    let (repo, auth) = repository(&state, &headers, &owner, &name, true).await?;
    let auth = auth.ok_or_else(super::auth::required)?;
    let actor = auth.database_actor();
    if !codeyard_git::is_full_commit_id(&input.sha) {
        return Err(Error::invalid("sha must be a full object ID"));
    }
    let sha = if let Some(branch) = input.reference.strip_prefix("refs/heads/") {
        refs::create_branch(
            &state.pool,
            state.cluster.as_ref(),
            &state.ci_runs,
            actor,
            &owner,
            repo.id,
            branch,
            &input.sha,
        )
        .await
        .map_err(ref_error)?
    } else if let Some(tag) = input.reference.strip_prefix("refs/tags/") {
        refs::create_tag(
            &state.pool,
            state.cluster.as_ref(),
            &state.ci_runs,
            actor,
            &owner,
            repo.id,
            tag,
            &input.sha,
            None,
        )
        .await
        .map_err(ref_error)?
        .sha
    } else {
        return Err(Error::invalid("Only heads and tags may be created"));
    };
    let kind = object_kind(
        &repo_store(&repo, state.cluster.as_ref()),
        &input.reference,
        &sha,
    )
    .await?;
    Ok((
        StatusCode::CREATED,
        Json(reference(
            repo.id,
            &owner,
            &name,
            &input.reference,
            &sha,
            &kind,
        )),
    )
        .into_response())
}

async fn remove(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, name, reference)): Path<(String, String, String)>,
) -> Result<StatusCode> {
    let (repo, auth) = repository(&state, &headers, &owner, &name, true).await?;
    let auth = auth.ok_or_else(super::auth::required)?;
    let actor = auth.database_actor();
    let store = repo_store(&repo, state.cluster.as_ref());
    if !store
        .refs_from_primary()
        .await
        .map_err(store_error)?
        .contains_key(&format!("refs/{reference}"))
    {
        return Err(Error::invalid("Reference does not exist"));
    }
    if let Some(branch) = reference.strip_prefix("heads/") {
        refs::delete_branch(
            &state.pool,
            state.cluster.as_ref(),
            &state.ci_runs,
            actor,
            repo.id,
            branch,
        )
        .await
        .map_err(ref_error)?;
    } else if let Some(tag) = reference.strip_prefix("tags/") {
        refs::delete_tag(&state.pool, state.cluster.as_ref(), actor, repo.id, tag)
            .await
            .map_err(ref_error)?;
    } else {
        return Err(Error::invalid("Only heads and tags may be deleted"));
    }
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
struct Update {
    sha: String,
    #[serde(default)]
    force: bool,
}

async fn update(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, name, reference)): Path<(String, String, String)>,
    Json(input): Json<Update>,
) -> Result<Json<Value>> {
    let (repo, auth) = repository(&state, &headers, &owner, &name, true).await?;
    let auth = auth.ok_or_else(super::auth::required)?;
    let actor = auth.database_actor();
    let full = format!("refs/{reference}");
    let sha = refs::update_reference(
        &state.pool,
        state.cluster.as_ref(),
        &state.ci_runs,
        actor,
        &owner,
        repo.id,
        &full,
        &input.sha,
        input.force,
    )
    .await
    .map_err(ref_error)?;
    let kind = object_kind(&repo_store(&repo, state.cluster.as_ref()), &full, &sha).await?;
    Ok(Json(self::reference(
        repo.id, &owner, &name, &full, &sha, &kind,
    )))
}

pub(super) fn handlers() -> Vec<(&'static str, MethodRouter<Arc<AppState>>)> {
    vec![
        ("git/update-ref", patch(update)),
        ("git/delete-ref", delete(remove)),
        ("git/get-ref", get(read)),
        ("git/create-ref", post(create)),
        ("git/list-matching-refs", get(list)),
    ]
}

/// Reads GitHub serves without documenting them.
pub(super) fn aliases() -> Router<Arc<AppState>> {
    Router::new()
        .route("/repos/:owner/:repo/git/refs", get(list))
        .route("/repos/:owner/:repo/git/refs/*ref", get(read))
        .route("/repos/:owner/:repo/git/matching-refs", get(list))
}
