//! Branch renames and upstream synchronization use the owning repository store.
use super::{contents::store_error, repository, Error, Result};
use crate::backend::{
    app::AppState,
    db,
    repositories::{access, pull_requests, store::repo_store},
};
use axum::{
    extract::{FromRequest, Path, Request, State},
    http::{HeaderMap, StatusCode},
    routing::{post, MethodRouter},
    Json,
};
use serde::Deserialize;
use serde_json::{json, Value};
use std::sync::Arc;

#[derive(Deserialize)]
struct Rename {
    new_name: String,
}

async fn rename(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, name, branch)): Path<(String, String, String)>,
    Json(input): Json<Rename>,
) -> Result<(StatusCode, Json<Value>)> {
    let (repo, auth) = repository(&state, &headers, &owner, &name, true).await?;
    let auth = auth.ok_or_else(super::auth::required)?;
    let actor = auth.user.id;
    let authority = auth.database_actor();
    repo_store(&repo, state.cluster.as_ref())
        .rename_branch_as(&state.pool, repo.id, authority, &branch, &input.new_name)
        .await
        .map_err(store_error)?;
    pull_requests::refresh_open_pull_requests(
        &state.pool,
        &repo,
        Some(&[&branch, &input.new_name]),
        &state.ci_runs,
        actor,
        db::models::AccessLevel::Owner,
    )
    .await
    .map_err(|_| {
        Error::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Branch renamed but pull request refresh failed",
        )
    })?;
    Ok((
        StatusCode::CREATED,
        Json(super::branches::document(&state, &repo, &owner, &name, &input.new_name).await?),
    ))
}

#[derive(Deserialize)]
struct Upstream {
    branch: String,
}

async fn upstream(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, name)): Path<(String, String)>,
    request: Request,
) -> Result<Json<Value>> {
    let (repo, auth) = repository(&state, &headers, &owner, &name, true).await?;
    let auth = auth.ok_or_else(super::auth::required)?;
    let actor = &auth.user;
    let Json(input) = Json::<Upstream>::from_request(request, &state)
        .await
        .map_err(|_| Error::invalid("A branch name is required"))?;
    if crate::backend::git::branch_ref(&input.branch).is_none() {
        return Err(Error::invalid("Invalid branch name"));
    }
    let parent = repo
        .forked_from_id
        .filter(|_| repo.is_fork)
        .ok_or_else(|| Error::invalid("Repository is not a fork"))?;
    let parent = access::check_read(&state.pool, parent, Some(actor.id))
        .await
        .map_err(|error| match error {
            access::AccessError::Unavailable => Error::unavailable(()),
            _ => Error::missing(),
        })?;
    // The upstream's commits become readable through the fork, so the credential must reach
    // the upstream too, not only the fork the request names.
    auth.repository_scope(&state.pool, parent.id, "contents", false)
        .await
        .map_err(super::auth::scope_error)?;
    let parent_owner = db::repositories::records::repository_owner_name(&state.pool, parent.id)
        .await
        .map_err(Error::unavailable)?
        .ok_or_else(Error::missing)?;
    let base_branch = format!("{parent_owner}:{}", input.branch);
    let tasks = state.git_tasks.clone();
    let (send, receive) = tokio::sync::oneshot::channel();
    if !tasks.spawn(async move {
        let result = complete_upstream(state, repo, parent, input, owner, base_branch, auth).await;
        if result.is_err() { tracing::warn!("upstream synchronization or its follow-up failed"); }
        let _ = send.send(result);
    }) {
        return Err(Error::new(StatusCode::SERVICE_UNAVAILABLE, "Repository writes are busy"));
    }
    receive.await.map_err(Error::unavailable)?
}

async fn complete_upstream(
    state: Arc<AppState>,
    repo: db::models::Repository,
    parent: db::models::Repository,
    input: Upstream,
    owner: String,
    base_branch: String,
    auth: super::auth::Authentication,
) -> Result<Json<Value>> {
    let actor = &auth.user;
    let source = repo_store(&parent, state.cluster.as_ref());
    let merged = repo_store(&repo, state.cluster.as_ref())
        .merge_upstream_as(
            &state.pool,
            repo.id,
            actor.id,
            (parent.id, &source),
            &input.branch,
            Some(auth.token.id),
        )
        .await
        .map_err(store_error)?;
    if merged.merge_type != crate::backend::repositories::store::UpstreamMergeType::None {
        // The branch moved, so its push workflows run as they do after any other push.
        state
            .ci_runs
            .queue_push_runs(
                &repo,
                &owner,
                actor.id,
                crate::backend::ci::RefUpdate {
                    reference: format!("refs/heads/{}", input.branch),
                    old: Some(merged.old.clone()),
                    new: merged.sha.clone(),
                    changed_paths: None,
                },
            )
            .await;
    }
    pull_requests::refresh_open_pull_requests(
        &state.pool,
        &repo,
        Some(&[&input.branch]),
        &state.ci_runs,
        actor.id,
        db::models::AccessLevel::Owner,
    )
    .await
    .map_err(|_| {
        Error::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Branch synchronized but pull request refresh failed",
        )
    })?;
    use crate::backend::repositories::store::UpstreamMergeType;
    let message = match merged.merge_type {
        UpstreamMergeType::None => format!("Already up to date with upstream {base_branch}"),
        UpstreamMergeType::FastForward => {
            format!("Successfully fetched and fast-forwarded from upstream {base_branch}")
        }
        UpstreamMergeType::Merge => {
            format!("Successfully fetched and merged from upstream {base_branch}")
        }
    };
    Ok(Json(
        json!({"message":message,"merge_type":merged.merge_type,"base_branch":base_branch}),
    ))
}

pub(super) fn handlers() -> Vec<(&'static str, MethodRouter<Arc<AppState>>)> {
    vec![
        ("repos/rename-branch", post(rename)),
        ("repos/merge-upstream", post(upstream)),
    ]
}
