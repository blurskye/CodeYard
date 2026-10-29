//! Branch merges are direct native writes, never synthetic pull-request merges.
use super::{commits, repository, Error, Result};
use crate::backend::{
    app::AppState,
    repositories::{
        access::AccessError,
        merges::{self, BranchMerge},
        refs::RefError,
        store::ErrorKind,
    },
};
use axum::{
    extract::{FromRequest, Path, Request, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{post, MethodRouter},
    Json,
};
use serde::Deserialize;
use std::sync::Arc;

#[derive(Deserialize)]
struct Input {
    base: String,
    head: String,
    commit_message: Option<String>,
}

pub(super) fn failure(error: RefError) -> Error {
    match error {
        RefError::Access(AccessError::NotFound) => Error::missing(),
        RefError::Access(AccessError::Unavailable) | RefError::Unavailable => Error::unavailable(()),
        RefError::Access(_) | RefError::Protected(_) => Error::new(StatusCode::FORBIDDEN, "Branch merge is denied"),
        RefError::Invalid(message) => Error::invalid(message),
        RefError::AppliedFollowupFailed => Error::new(StatusCode::SERVICE_UNAVAILABLE, "The merge was applied, but repository follow-up failed. Check the branch before retrying."),
        RefError::Git(error) => match error.kind {
            ErrorKind::Conflict => Error::new(StatusCode::CONFLICT, "Merge conflict or branch changed"),
            ErrorKind::NotFound | ErrorKind::MissingPath => Error::missing(),
            _ => super::contents::store_error(error),
        },
    }
}

async fn merge(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, name)): Path<(String, String)>,
    request: Request,
) -> Result<Response> {
    let (repo, auth) = repository(&state, &headers, &owner, &name, true).await?;
    let auth = auth.ok_or_else(super::auth::required)?;
    let Json(input) = Json::<Input>::from_request(request, &state)
        .await
        .map_err(|error| Error::new(error.status(), "Invalid merge request"))?;
    let result = merges::merge_branch(
        &state.pool,
        state.cluster.as_ref(),
        &state.ci_runs,
        auth.user.id,
        repo.id,
        &owner,
        BranchMerge {
            base: &input.base,
            head: &input.head,
            message: input.commit_message.as_deref(),
            expected_base: None,
            expected_head: None,
            token: Some(auth.token.id),
        },
    )
    .await
    .map_err(failure)?;
    match result {
        None => Ok(StatusCode::NO_CONTENT.into_response()),
        Some(commit) => Ok((
            StatusCode::CREATED,
            Json(commits::read_document(&state, repo.id, &commit, &owner, &name).await?),
        )
            .into_response()),
    }
}

pub(super) fn handlers() -> Vec<(&'static str, MethodRouter<Arc<AppState>>)> {
    vec![("repos/merge", post(merge))]
}
