//! External status reporters share the status rows used by branch protection.
use super::{contents::store_error, pagination::Pagination, repo_url, repository, Error, Result};
use crate::backend::{
    app::AppState,
    db::{self, models::CommitStatus, repositories::protection},
    repositories::store::repo_store,
    webhooks::{self, payloads},
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

async fn documents(
    state: &AppState,
    owner: &str,
    name: &str,
    rows: Vec<CommitStatus>,
) -> Result<Vec<Value>> {
    let mut ids: Vec<_> = rows.iter().filter_map(|row| row.creator_id).collect();
    ids.sort_unstable();
    ids.dedup();
    let users = db::users::accounts::get_users_by_ids(&state.pool, &ids)
        .await
        .map_err(Error::unavailable)?;
    let users: std::collections::HashMap<_, _> = users
        .into_iter()
        .map(|user| (user.id, payloads::person(&payloads::Person::of(&user))))
        .collect();
    Ok(rows
        .into_iter()
        .map(|status| {
            let creator = status.creator_id.and_then(|id| users.get(&id));
            json!({
            "id": status.id,
            "node_id": super::node_id::encode("CS", status.id),
            "avatar_url": creator.as_ref().map(|value| &value["avatar_url"]),
            "url": format!("{}/statuses/{}", repo_url(owner, name), status.sha),
            "state": status.state, "context": status.context, "description": status.description,
            "target_url": status.target_url, "creator": creator,
            "created_at": status.created_at, "updated_at": status.updated_at,
            })
        })
        .collect())
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
    let auth = auth.ok_or_else(super::auth::required)?;
    let actor = auth.user.id;
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
    let store = repo_store(&repo, state.cluster.as_ref());
    let commit = store.commit(&sha).await.map_err(store_error)?;
    if commit.sha != sha {
        return Err(Error::invalid("sha must name an existing commit object"));
    }
    let saved = protection::upsert_commit_status_with_token(
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
        Some(auth.token.id),
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
    webhooks::statuses::emit_status(&state.pool, &store, &repo, &saved, &commit, actor).await;
    Ok((
        StatusCode::CREATED,
        Json(
            documents(&state, &owner, &name, vec![saved])
                .await?
                .remove(0),
        ),
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
    let (repo, auth) = repository(&state, &headers, &owner, &name, false).await?;
    let sha = repo_store(&repo, state.cluster.as_ref())
        .commit(&reference)
        .await
        .map_err(store_error)?
        .sha;
    let (offset, limit) = page.window()?;
    let history = uri.path().ends_with("/statuses");
    let page_rows = protection::commit_status_page(
        &state.pool,
        auth.as_ref().map(|auth| auth.user.id),
        repo.id,
        &sha,
        offset as i64,
        limit as i64,
        history,
    )
    .await
    .map_err(Error::unavailable)?
    .ok_or_else(Error::missing)?;
    if let Some(auth) = auth.as_ref() {
        if db::users::tokens::token_user(&state.pool, auth.token.id, false)
            .await
            .map_err(Error::unavailable)?
            .is_none_or(|user| user.id != auth.user.id)
        {
            return Err(super::auth::required());
        }
        auth.repository_scope(&state.pool, repo.id, "statuses", false)
            .await
            .map_err(super::auth::scope_error)?;
    }
    let protection::CommitStatusPage {
        total,
        counts,
        statuses,
    } = page_rows;
    let items = documents(&state, &owner, &name, statuses).await?;
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
