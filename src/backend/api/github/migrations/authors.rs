//! Author mappings retain original identities and never alter imported commits.
use super::{administrator, repo_url, AppState, Error, Result};
use crate::backend::{
    db::{self, import_authors::Author, source_imports::ImportRefusal},
    repositories::{reads, store::repo_store},
    webhooks::payloads,
};
use axum::{
    body::to_bytes,
    extract::{Path, Query, Request, State},
    http::{HeaderMap, StatusCode},
    routing::{get, patch, MethodRouter},
    Json,
};
use serde::Deserialize;
use serde_json::{json, Value};
use std::{
    collections::{BTreeSet, HashSet},
    sync::Arc,
    time::Instant,
};

pub(super) fn handlers() -> Vec<(&'static str, MethodRouter<Arc<AppState>>)> {
    vec![
        ("migrations/get-commit-authors", get(list)),
        ("migrations/map-commit-author", patch(map)),
    ]
}
fn error(error: anyhow::Error) -> Error {
    if matches!(
        error.downcast_ref::<ImportRefusal>(),
        Some(ImportRefusal::Denied)
    ) {
        Error::new(StatusCode::FORBIDDEN, "Repository administration denied")
    } else {
        Error::unavailable(error)
    }
}
async fn documents(
    state: &AppState,
    owner: &str,
    name: &str,
    rows: Vec<Author>,
) -> Result<Vec<Value>> {
    let mut values = Vec::new();
    for chunk in rows.chunks(512) {
        let emails = chunk
            .iter()
            .map(|row| {
                row.mapped_email
                    .as_ref()
                    .unwrap_or(&row.author_email)
                    .clone()
            })
            .collect::<Vec<_>>();
        let users = reads::commit_authors(&state.pool, &emails)
            .await
            .map_err(Error::unavailable)?;
        for row in chunk {
            let email = row.mapped_email.as_ref().unwrap_or(&row.author_email);
            let user = users.get(email);
            let display_name = row
                .mapped_name
                .as_deref()
                .or_else(|| user.map(|user| user.display_name.as_deref().unwrap_or(&user.username)))
                .unwrap_or(&row.author_name);
            let base = format!("{}/import", repo_url(owner, name));
            values.push(json!({"id":row.id,"remote_id":row.author_email,"remote_name":row.author_name,
                "email":email,"name":display_name,"url":format!("{base}/authors/{}",row.id),"import_url":base,
                "user":user.map(payloads::user)}));
        }
    }
    Ok(values)
}

#[derive(Deserialize)]
struct Since {
    since: Option<i64>,
}
async fn list(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, name)): Path<(String, String)>,
    Query(query): Query<Since>,
) -> Result<Json<Value>> {
    let (repo, actor) = administrator(&state, &headers, &owner, &name, false).await?;
    db::source_imports::get(&state.pool, repo.id)
        .await
        .map_err(Error::unavailable)?
        .ok_or_else(Error::missing)?;
    let store = repo_store(&repo, state.cluster.as_ref());
    let (refs, _) = reads::refs_and_default(&repo, state.cluster.as_ref())
        .await
        .map_err(super::store_error)?;
    let mut authors = BTreeSet::new();
    let deadline = Instant::now() + crate::backend::git::COMMIT_WALK_TIMEOUT;
    // Every branch tip is walked, so an author who committed only on a feature branch is recorded.
    let tips: BTreeSet<&str> = refs
        .iter()
        .filter(|(name, _)| name.starts_with("refs/heads/"))
        .map(|(_, sha)| sha.as_str())
        .collect();
    let mut seen = HashSet::new();
    for sha in tips {
        let mut offset = 0;
        loop {
            let commits = store
                .commits_until(sha, offset, 100, &Default::default(), deadline)
                .await
                .map_err(super::store_error)?;
            let count = commits.len();
            let mut new = false;
            for commit in commits {
                if seen.insert(commit.sha) {
                    new = true;
                    authors.insert((commit.author_name, commit.author_email));
                }
            }
            // A page walked entirely from an earlier tip means this branch has joined known
            // history, so its walk stops there.
            if count < 100 || !new {
                break;
            }
            offset += count;
        }
    }
    let authors = authors.into_iter().collect::<Vec<_>>();
    let mut rows = db::import_authors::record(&state.pool, actor.user.id, repo.id, &authors)
        .await
        .map_err(error)?;
    rows.retain(|row| row.id > query.since.unwrap_or(0));
    rows.sort_by_key(|row| row.id);
    Ok(Json(json!(documents(&state, &owner, &name, rows).await?)))
}

#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct Mapping {
    name: Option<String>,
    email: Option<String>,
}
async fn map(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, name, id)): Path<(String, String, i64)>,
    request: Request,
) -> Result<Json<Value>> {
    let (repo, actor) = administrator(&state, &headers, &owner, &name, true).await?;
    actor.require_write()?;
    let bytes = to_bytes(request.into_body(), 16 * 1024)
        .await
        .map_err(super::super::json_bytes_error)?;
    let input = if bytes.is_empty() {
        Mapping::default()
    } else {
        serde_json::from_slice::<Option<Mapping>>(&bytes)
            .map_err(super::super::json_parse_error)?
            .unwrap_or_default()
    };
    if input.name.as_ref().is_some_and(|value| {
        value.is_empty() || value.len() > 255 || value.chars().any(char::is_control)
    }) {
        return Err(Error::validation("PorterAuthor", "name", "invalid"));
    }
    if input
        .email
        .as_ref()
        .is_some_and(|value| value.len() > 254 || value.parse::<lettre::Address>().is_err())
    {
        return Err(Error::validation("PorterAuthor", "email", "invalid"));
    }
    let row = db::import_authors::map(
        &state.pool,
        actor.user.id,
        repo.id,
        id,
        input.name.as_deref(),
        input.email.as_deref(),
    )
    .await
    .map_err(error)?
    .ok_or_else(Error::missing)?;
    Ok(Json(
        documents(&state, &owner, &name, vec![row]).await?.remove(0),
    ))
}
