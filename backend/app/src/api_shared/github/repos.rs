use crate::api_shared::github::repository;
use crate::app::AppState;
use axum::extract::Path;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::http::StatusCode;
use codeyard_db_core::models::Repository;
use codeyard_model::github_error::Error;
use codeyard_model::github_error::Result;
use serde_json::json;
use serde_json::Value;
use std::sync::Arc;
pub fn lifecycle_error(error: crate::repositories::lifecycle::StorageError) -> Error {
    use crate::repositories::lifecycle::LifecycleFailure::*;
    let status = match error.kind {
        Invalid | OwnRepositoryFork | Conflict => StatusCode::UNPROCESSABLE_ENTITY,
        Denied => StatusCode::FORBIDDEN,
        Unavailable | InitializationCleanupFailed | DeletedStorageRemains => {
            StatusCode::SERVICE_UNAVAILABLE
        }
    };
    Error::new(status, error.message)
}

pub async fn public_fork_count(state: &AppState, repo: i64) -> Result<i32> {
    // The source token does not necessarily select any of its private forks.
    Ok(
        codeyard_db_repos::repositories::discovery::public_fork_counts(&state.pool, &[repo])
            .await
            .map_err(Error::unavailable)?
            .remove(&repo)
            .unwrap_or_default(),
    )
}

/// A repository's REST document for `viewer`. Its fork parent and source appear only when the
/// viewer's credential may read them too.
pub async fn document(
    state: &AppState,
    repo: &Repository,
    viewer: Option<&crate::api_shared::github::auth::Authentication>,
) -> Result<Value> {
    documents(state, std::slice::from_ref(repo), viewer)
        .await?
        .pop()
        .ok_or_else(Error::missing)
}

pub async fn documents(
    state: &AppState,
    repos: &[Repository],
    viewer: Option<&crate::api_shared::github::auth::Authentication>,
) -> Result<Vec<Value>> {
    let ids: Vec<_> = repos.iter().map(|repo| repo.id).collect();
    let owners =
        codeyard_db_repos::repositories::records::repository_owner_names(&state.pool, &ids)
            .await
            .map_err(Error::unavailable)?;
    let topics = codeyard_db_repos::repositories::topics::repository_topics(&state.pool, &ids)
        .await
        .map_err(Error::unavailable)?;
    // Counts only public forks: the viewer's credential does not necessarily select its private ones.
    let forks = codeyard_db_repos::repositories::discovery::public_fork_counts(&state.pool, &ids)
        .await
        .map_err(Error::unavailable)?;
    let mut metadata = std::collections::HashMap::new();
    for ids in ids.chunks(100) {
        metadata.extend(
            codeyard_db_repos::repository_stats::repository_document_metadata(&state.pool, ids)
                .await
                .map_err(Error::unavailable)?
                .into_iter()
                .map(|row| (row.repo_id, row)),
        );
    }
    let mut values = Vec::with_capacity(repos.len());
    for repo in repos {
        let (Some(owner), Some(facts)) = (owners.get(&repo.id), metadata.get(&repo.id)) else {
            continue;
        };
        values.push(
            document_with(
                state,
                repo,
                viewer,
                owner,
                facts,
                topics.get(&repo.id).map(Vec::as_slice).unwrap_or_default(),
                forks.get(&repo.id).copied().unwrap_or_default(),
            )
            .await?,
        );
    }
    if let Some(viewer) = viewer {
        let readable = crate::api_shared::github::auth::readable_repositories(
            state,
            Some(viewer),
            &ids,
            "metadata",
        )
        .await?;
        values.retain(|value| {
            value["id"]
                .as_i64()
                .is_some_and(|id| readable.contains(&id))
        });
    }
    Ok(values)
}

async fn document_with(
    state: &AppState,
    repo: &Repository,
    viewer: Option<&crate::api_shared::github::auth::Authentication>,
    owner: &str,
    facts: &codeyard_db_repos::repository_stats::RepositoryDocumentMetadata,
    topics: &[String],
    forks: i32,
) -> Result<Value> {
    let mut value = crate::webhooks::payloads::repository(repo, owner);
    crate::webhooks::payloads::set_fork_counts(&mut value, forks);
    value["language"] = json!(facts.facts.language);
    value["open_issues_count"] = json!(facts.facts.open_items);
    value["open_issues"] = json!(facts.facts.open_items);
    value["pushed_at"] = json!(facts.last_pushed_at);
    value["topics"] = json!(topics);
    if let Some(document) = value.as_object_mut() {
        crate::webhooks::payloads::set_features(document, &facts.features);
    }
    value["has_discussions"] = json!(facts.discussions);
    if repo.is_fork {
        let mut parent = repo.forked_from_id;
        let mut seen = std::collections::HashSet::from([repo.id]);
        while let Some(id) = parent {
            if seen.len() > 64 || !seen.insert(id) {
                return Err(Error::unavailable(anyhow::anyhow!("Invalid fork ancestry")));
            }
            let ancestor =
                codeyard_db_repos::repositories::records::get_repository_by_id(&state.pool, id)
                    .await
                    .map_err(Error::unavailable)?
                    .ok_or_else(Error::missing)?;
            if !crate::api_shared::github::auth::readable_repositories(
                state,
                viewer,
                &[id],
                "metadata",
            )
            .await?
            .contains(&id)
            {
                break;
            }
            let owner =
                codeyard_db_repos::repositories::records::repository_owner_name(&state.pool, id)
                    .await
                    .map_err(Error::unavailable)?
                    .ok_or_else(Error::missing)?;
            let mut document = crate::webhooks::payloads::repository(&ancestor, &owner);
            crate::webhooks::payloads::set_fork_counts(
                &mut document,
                public_fork_count(state, id).await?,
            );
            if repo.forked_from_id == Some(id) {
                value["parent"] = document.clone();
            }
            parent = ancestor.forked_from_id;
            if parent.is_none() {
                value["source"] = document;
            }
        }
    }
    if let Some(viewer) = viewer.map(|auth| &auth.user) {
        let level = codeyard_identity::repository_access::user_access(&state.pool, viewer, repo)
            .await
            .map_err(Error::unavailable)?;
        if level.can_admin() {
            if let Some(settings) =
                codeyard_db_security::code_security::repository_settings(&state.pool, repo.id)
                    .await
                    .map_err(Error::unavailable)?
            {
                value["security_and_analysis"] =
                    serde_json::from_str(&settings.security_and_analysis)
                        .map_err(Error::unavailable)?;
            }
            let security = codeyard_db_security::secret_scanning::settings(&state.pool, repo.id)
                .await
                .map_err(Error::unavailable)?;
            let status = |enabled| if enabled { "enabled" } else { "disabled" };
            if !value["security_and_analysis"].is_object() {
                value["security_and_analysis"] = json!({});
            }
            value["security_and_analysis"]["secret_scanning"] =
                json!({"status":status(security.enabled)});
            value["security_and_analysis"]["secret_scanning_push_protection"] =
                json!({"status":status(security.push_protection)});
        }
        value["permissions"] = json!({
            "admin": level.can_admin(), "maintain": level.can_maintain(),
            "push": level.can_write(), "triage": level >= codeyard_db_core::models::AccessLevel::Triage,
            "pull": !repo.is_private || level.can_read(),
        });
    }
    Ok(value)
}

pub async fn remove(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, name)): Path<(String, String)>,
) -> Result<StatusCode> {
    let (repo, actor) = repository(&state, &headers, &owner, &name, true).await?;
    let actor = actor.ok_or_else(crate::api_shared::github::auth::required)?;
    crate::repositories::lifecycle::delete(
        &state.pool,
        state.cluster.as_ref(),
        actor.database_actor(),
        repo.id,
    )
    .await
    .map_err(lifecycle_error)?;
    Ok(StatusCode::NO_CONTENT)
}
