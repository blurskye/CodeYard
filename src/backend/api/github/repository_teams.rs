//! Repository-scoped view of the existing organization team grants.
use super::{auth, pagination::Pagination, repository, Error, Result};
use crate::backend::{app::AppState, db, webhooks::payloads};
use axum::{
    extract::{OriginalUri, Path, Query, State},
    http::{HeaderMap, StatusCode},
    response::Response,
    routing::{get, MethodRouter},
};
use std::sync::Arc;

async fn list(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, name)): Path<(String, String)>,
    Query(page): Query<Pagination>,
    OriginalUri(uri): OriginalUri,
) -> Result<Response> {
    let (repository, actor) = repository(&state, &headers, &owner, &name, false).await?;
    let actor = actor.ok_or_else(auth::required)?.user;
    let (offset, limit) = page.window()?;
    let teams = db::repositories::github::repository_teams(
        &state.pool,
        actor.id,
        repository.id,
        (limit + 1) as i64,
        offset as i64,
    )
    .await
    .map_err(Error::unavailable)?
    .ok_or_else(|| {
        Error::new(
            StatusCode::FORBIDDEN,
            "Repository read access and organization membership are required",
        )
    })?;
    let next = teams.len() > limit;
    page.response(
        &uri,
        teams
            .into_iter()
            .take(limit)
            .map(|row| {
                let mut value = payloads::team_document(&row.team, &owner, row.permission);
                if let Some(parent) = row.parent { value["parent"] = payloads::team_document(&parent, &owner, parent.default_permission); }
                value
            })
            .collect(),
        next,
    )
}

pub(super) fn handlers() -> Vec<(&'static str, MethodRouter<Arc<AppState>>)> {
    vec![("repos/list-teams", get(list))]
}
