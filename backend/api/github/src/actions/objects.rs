//! Cache and artifact metadata, deletion, and short-lived artifact downloads.
use crate::actions::id;
use crate::actions::run;
use axum::extract::OriginalUri;
use axum::extract::Path;
use axum::extract::Query;
use axum::extract::State;
use axum::http::header;
use axum::http::HeaderMap;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::response::Response;
use axum::routing::delete;
use axum::routing::get;
use axum::routing::MethodRouter;
use axum::Json;
use codeyard_api_http::shared::github::pagination::Pagination;
use codeyard_app::api_shared::github::repository;
use codeyard_app::app::AppState;
use codeyard_db_actions::action_objects as objects;
use codeyard_db_actions::action_objects::ActionObject;
use codeyard_db_actions::action_objects::ObjectOrder;
use codeyard_db_actions::action_objects::ObjectQuery;
use codeyard_model::github_error::Error;
use codeyard_model::github_error::Result;
use serde::Deserialize;
use serde_json::json;
use serde_json::Value;
use std::collections::HashMap;
use std::sync::Arc;

#[derive(Deserialize)]
struct Listing {
    page: Option<usize>,
    per_page: Option<usize>,
    name: Option<String>,
    key: Option<String>,
    #[serde(rename = "ref")]
    reference: Option<String>,
    sort: Option<String>,
    direction: Option<String>,
}

impl Listing {
    fn page(&self) -> Pagination {
        Pagination {
            page: self.page,
            per_page: self.per_page,
        }
    }
    fn validate(&self) -> Result<()> {
        if self
            .name
            .iter()
            .chain(&self.key)
            .any(|s| s.len() > 512 || s.chars().any(char::is_control))
            || self
                .reference
                .as_ref()
                .is_some_and(|s| s.len() > 1024 || s.chars().any(char::is_control))
        {
            return Err(Error::invalid("Invalid cache key, artifact name, or ref"));
        }
        Ok(())
    }
}

fn cache_document(object: &ActionObject) -> Value {
    let time = |seconds| chrono::DateTime::from_timestamp(seconds, 0).map(|t| t.to_rfc3339());
    let reference = if object.scope.starts_with("refs/") {
        object.scope.clone()
    } else {
        format!("refs/heads/{}", object.scope)
    };
    json!({"id":object.id,"ref":reference,"key":object.name,"version":object.version,"last_accessed_at":time(object.accessed_at),"created_at":time(object.created_at),"size_in_bytes":object.size})
}

async fn artifact_document(
    state: &AppState,
    owner: &str,
    name: &str,
    object: &ActionObject,
) -> Result<Value> {
    let run = match object.run_id {
        Some(id) => Some(
            codeyard_db_actions::ci::get_ci_run_by_id(&state.pool, object.repo_id, id)
                .await
                .map_err(Error::unavailable)?
                .ok_or_else(Error::missing)?,
        ),
        None => None,
    };
    Ok(codeyard_webhooks::payloads::action_artifact(
        object,
        run.as_ref(),
        owner,
        name,
    ))
}

async fn listing(
    state: &AppState,
    headers: &HeaderMap,
    params: &HashMap<String, String>,
    input: &Listing,
    uri: &axum::http::Uri,
    cache: bool,
) -> Result<Response> {
    input.validate()?;
    let (owner, name) = (&params["owner"], &params["repo"]);
    let (repo, _) = repository(state, headers, owner, name, false).await?;
    let run_id = if params.contains_key("run_id") {
        Some(
            run(state, repo.id, id(params, "run_id")?)
                .await?
                .workflow_run_id
                .unwrap_or(id(params, "run_id")?),
        )
    } else {
        None
    };
    let order = match input.sort.as_deref() {
        None if !cache => ObjectOrder::Created,
        None | Some("last_accessed_at") => ObjectOrder::Accessed,
        Some("created_at") => ObjectOrder::Created,
        Some("size_in_bytes") => ObjectOrder::Size,
        _ => return Err(Error::invalid("Invalid sort")),
    };
    let ascending = match input.direction.as_deref() {
        None | Some("desc") => false,
        Some("asc") => true,
        _ => return Err(Error::invalid("Invalid direction")),
    };
    let page = input.page();
    let (offset, limit) = page.window()?;
    let rows = objects::query_objects(
        &state.pool,
        &ObjectQuery {
            repo_id: repo.id,
            run_id,
            kind: if cache { "cache" } else { "artifact" },
            name: if cache {
                input.key.as_deref()
            } else {
                input.name.as_deref()
            },
            scope: if cache {
                input.reference.as_deref()
            } else {
                None
            },
            order,
            ascending,
            offset: offset as i64,
            limit: limit as i64,
        },
    )
    .await
    .map_err(Error::unavailable)?;
    let mut values = Vec::new();
    for object in rows.objects {
        values.push(if cache {
            cache_document(&object)
        } else {
            artifact_document(state, owner, name, &object).await?
        });
    }
    let body = if cache {
        json!({"total_count":rows.total,"actions_caches":values})
    } else {
        json!({"total_count":rows.total,"artifacts":values})
    };
    Ok((
        page.headers(uri, ((offset + limit) as i64) < rows.total)?,
        Json(body),
    )
        .into_response())
}

async fn artifacts(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(params): Path<HashMap<String, String>>,
    Query(input): Query<Listing>,
    OriginalUri(uri): OriginalUri,
) -> Result<Response> {
    listing(&state, &headers, &params, &input, &uri, false).await
}

async fn caches(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(params): Path<HashMap<String, String>>,
    Query(input): Query<Listing>,
    OriginalUri(uri): OriginalUri,
) -> Result<Response> {
    listing(&state, &headers, &params, &input, &uri, true).await
}

async fn artifact(
    state: &AppState,
    headers: &HeaderMap,
    owner: &str,
    name: &str,
    id: i64,
    write: bool,
) -> Result<(
    ActionObject,
    Option<codeyard_repos_core::api_shared::github::auth::Authentication>,
)> {
    let (repo, auth) = repository(state, headers, owner, name, write).await?;
    let object = objects::object_by_id(&state.pool, repo.id, id, "artifact")
        .await
        .map_err(Error::unavailable)?
        .ok_or_else(Error::missing)?;
    Ok((object, auth))
}

async fn get_artifact(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, name, id)): Path<(String, String, i64)>,
) -> Result<Json<Value>> {
    let (object, _) = artifact(&state, &headers, &owner, &name, id, false).await?;
    Ok(Json(
        artifact_document(&state, &owner, &name, &object).await?,
    ))
}

async fn delete_artifact(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, name, id)): Path<(String, String, i64)>,
) -> Result<StatusCode> {
    let (object, auth) = artifact(&state, &headers, &owner, &name, id, true).await?;
    let auth = auth.ok_or_else(codeyard_repos_core::api_shared::github::auth::required)?;
    let actor = auth.workflow_actor(&state, object.repo_id).await?;
    let credential = auth.ci_credential(&headers, "actions")?;
    if !objects::delete_artifact(
        &state.pool,
        object.id,
        object.run_id.ok_or_else(Error::missing)?,
        Some(actor),
        Some(&credential),
    )
    .await
    .map_err(Error::unavailable)?
    {
        return Err(Error::new(
            StatusCode::FORBIDDEN,
            "Artifact deletion denied",
        ));
    }
    Ok(StatusCode::NO_CONTENT)
}

async fn download_artifact(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, name, id, format)): Path<(String, String, i64, String)>,
) -> Result<Response> {
    let (object, auth) = artifact(&state, &headers, &owner, &name, id, false).await?;
    let actor = auth
        .ok_or_else(codeyard_repos_core::api_shared::github::auth::required)?
        .database_actor();
    if format != "zip" {
        return Err(Error::invalid("archive_format must be zip"));
    }
    if object.expires_at <= chrono::Utc::now().timestamp() {
        return Err(Error::new(StatusCode::GONE, "Artifact has expired"));
    }
    let token = codeyard_ci::runtime::secret();
    if !objects::grant_artifact_download(
        &state.pool,
        actor,
        object.repo_id,
        object.id,
        &codeyard_support::token_hash::token_hash(&token),
    )
    .await
    .map_err(Error::unavailable)?
    {
        return Err(Error::missing());
    }
    Ok((
        StatusCode::FOUND,
        [(
            header::LOCATION,
            format!(
                "{}/twirp/blobs/{}?sig={token}",
                codeyard_model::http_config::link_origin(),
                object.id
            ),
        )],
    )
        .into_response())
}

async fn usage(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, name)): Path<(String, String)>,
) -> Result<Json<Value>> {
    let (repo, _) = repository(&state, &headers, &owner, &name, false).await?;
    let (count, bytes) = objects::cache_usage(&state.pool, repo.id)
        .await
        .map_err(Error::unavailable)?;
    Ok(Json(
        json!({"full_name":format!("{owner}/{name}"),"active_caches_count":count,"active_caches_size_in_bytes":bytes}),
    ))
}

async fn delete_cache(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(params): Path<HashMap<String, String>>,
    Query(input): Query<Listing>,
) -> Result<Response> {
    input.validate()?;
    let (repo, auth) =
        repository(&state, &headers, &params["owner"], &params["repo"], true).await?;
    let actor = auth
        .ok_or_else(codeyard_repos_core::api_shared::github::auth::required)?
        .user
        .id;
    let cache_id = params
        .contains_key("cache_id")
        .then(|| id(&params, "cache_id"))
        .transpose()?;
    if cache_id.is_none() && input.key.as_ref().is_none_or(|s| s.is_empty()) {
        return Err(Error::validation("ActionsCache", "key", "missing_field"));
    }
    let mut rows = objects::delete_caches(
        &state.pool,
        actor,
        repo.id,
        cache_id,
        input.key.as_deref(),
        input.reference.as_deref(),
    )
    .await
    .map_err(Error::unavailable)?
    .ok_or_else(|| Error::new(StatusCode::FORBIDDEN, "Cache deletion denied"))?;
    if rows.is_empty() {
        return Err(Error::missing());
    }
    if cache_id.is_some() {
        return Ok(StatusCode::NO_CONTENT.into_response());
    }
    rows.sort_by_key(|object| object.id);
    Ok(Json(json!({"total_count":rows.len(),"actions_caches":rows.iter().map(cache_document).collect::<Vec<_>>()})).into_response())
}

pub(super) fn handlers() -> Vec<(&'static str, MethodRouter<Arc<AppState>>)> {
    vec![
        ("actions/list-artifacts-for-repo", get(artifacts)),
        ("actions/list-workflow-run-artifacts", get(artifacts)),
        ("actions/get-artifact", get(get_artifact)),
        ("actions/delete-artifact", delete(delete_artifact)),
        ("actions/download-artifact", get(download_artifact)),
        ("actions/get-actions-cache-list", get(caches)),
        ("actions/get-actions-cache-usage", get(usage)),
        ("actions/delete-actions-cache-by-key", delete(delete_cache)),
        ("actions/delete-actions-cache-by-id", delete(delete_cache)),
    ]
}
