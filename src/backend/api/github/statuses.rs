//! External status reporters share the status rows used by branch protection.
use super::{contents::store_error, pagination::Pagination, repo_url, repository, Error, Result};
use crate::backend::{
    app::AppState,
    db::{self, models::CommitStatus, repositories::protection},
    repositories::store::repo_store,
    webhooks::payloads,
};
use axum::{
    extract::{OriginalUri, Path, Query, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post, MethodRouter},
    Json,
};
use serde::Deserialize;
use serde_json::{json, Value};
use std::sync::Arc;

async fn document(
    state: &AppState,
    owner: &str,
    name: &str,
    status: CommitStatus,
) -> Result<Value> {
    let creator = match status.creator_id {
        Some(id) => db::users::accounts::get_user_by_id(&state.pool, id)
            .await
            .map_err(Error::unavailable)?
            .map(|user| payloads::person(&payloads::Person::of(&user))),
        None => None,
    };
    Ok(json!({
        "id": status.id,
        "node_id": super::node_id::encode("CS", status.id),
        "avatar_url": creator.as_ref().map(|value| &value["avatar_url"]),
        "url": format!("{}/statuses/{}", repo_url(owner, name), status.sha),
        "state": status.state, "context": status.context, "description": status.description,
        "target_url": status.target_url, "creator": creator,
        "created_at": status.created_at, "updated_at": status.updated_at,
    }))
}

#[derive(Deserialize)]
struct Create {
    state: String,
    context: Option<String>,
    description: Option<String>,
    target_url: Option<String>,
}

async fn create(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, name, sha)): Path<(String, String, String)>,
    Json(input): Json<Create>,
) -> Result<Response> {
    let (repo, auth) = repository(&state, &headers, &owner, &name, true).await?;
    let actor = auth.ok_or_else(super::auth::required)?.user.id;
    if !crate::backend::git::is_full_commit_id(&sha) {
        return Err(Error::invalid("sha must be a full commit ID"));
    }
    let context = input.context.as_deref().unwrap_or("default");
    if protection::server_owned_status_context(context) {
        return Err(Error::new(
            StatusCode::FORBIDDEN,
            "This status context is managed by Actions",
        ));
    }
    let sha = sha.to_ascii_lowercase();
    repo_store(&repo, state.cluster.as_ref())
        .commit(&sha)
        .await
        .map_err(store_error)?;
    let saved = protection::upsert_commit_status(
        &state.pool,
        protection::CommitStatusUpsert {
            repo_id: repo.id,
            sha: &sha,
            context,
            state: &input.state,
            description: input.description.as_deref(),
            target_url: input.target_url.as_deref(),
            creator_id: Some(actor),
        },
    )
    .await
    .map_err(
        |error| match error.downcast_ref::<protection::ProtectionWriteRefusal>() {
            Some(protection::ProtectionWriteRefusal::Denied) => {
                Error::new(StatusCode::FORBIDDEN, "Status write denied")
            }
            Some(_) => Error::invalid("Invalid commit status"),
            None => Error::unavailable(()),
        },
    )?;
    Ok((
        StatusCode::CREATED,
        Json(document(&state, &owner, &name, saved).await?),
    )
        .into_response())
}

async fn read(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, name, reference)): Path<(String, String, String)>,
    Query(page): Query<Pagination>,
    OriginalUri(uri): OriginalUri,
) -> Result<Response> {
    let (repo, _) = repository(&state, &headers, &owner, &name, false).await?;
    let sha = repo_store(&repo, state.cluster.as_ref())
        .commit(&reference)
        .await
        .map_err(store_error)?
        .sha;
    let (offset, limit) = page.window()?;
    let history = uri.path().ends_with("/statuses");
    let counts = if history {
        Vec::new()
    } else {
        protection::commit_status_counts(&state.pool, repo.id, &sha)
            .await
            .map_err(Error::unavailable)?
    };
    let total: i64 = if history {
        protection::commit_status_history_count(&state.pool, repo.id, &sha)
            .await
            .map_err(Error::unavailable)?
    } else {
        counts.iter().map(|(_, count)| count).sum()
    };
    let rows = if history {
        protection::list_commit_status_history_page(
            &state.pool,
            repo.id,
            &sha,
            offset as i64,
            limit as i64,
        )
        .await
    } else {
        protection::list_commit_statuses_page(
            &state.pool,
            repo.id,
            &sha,
            offset as i64,
            limit as i64,
            true,
        )
        .await
    }
    .map_err(Error::unavailable)?;
    let mut items = Vec::with_capacity(rows.len());
    for status in rows {
        items.push(document(&state, &owner, &name, status).await?);
    }
    let has_next = total > (offset + limit) as i64;
    if history {
        return page.response(&uri, items, has_next);
    }
    let combined = if counts
        .iter()
        .any(|(state, count)| *count > 0 && matches!(state.as_str(), "error" | "failure"))
    {
        "failure"
    } else if total == 0
        || counts
            .iter()
            .any(|(state, count)| state == "pending" && *count > 0)
    {
        "pending"
    } else {
        "success"
    };
    Ok((
        page.headers(&uri, has_next)?,
        Json(json!({
            "state": combined, "sha": sha, "total_count": total, "statuses": items,
            "repository": payloads::repository(&repo, &owner),
            "commit_url": format!("{}/commits/{sha}", repo_url(&owner, &name)),
            "url": format!("{}/commits/{reference}/status", repo_url(&owner, &name)),
        })),
    )
        .into_response())
}

pub(super) fn handlers() -> Vec<(&'static str, MethodRouter<Arc<AppState>>)> {
    vec![
        ("repos/create-commit-status", post(create)),
        ("repos/get-combined-status-for-ref", get(read)),
        ("repos/list-commit-statuses-for-ref", get(read)),
    ]
}
