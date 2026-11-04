//! The Results Twirp JSON API and Azure block-blob subset used by Actions v4.
use axum::extract::DefaultBodyLimit;
use axum::extract::Path;
use axum::extract::Query;
use axum::extract::State;
use axum::http::header;
use axum::http::HeaderMap;
use axum::http::StatusCode;
use axum::response::Response;
use axum::routing::get;
use axum::routing::post;
use axum::Json;
use axum::Router;
use codeyard_app::app::AppState;
use codeyard_app::storage::Upload;
use codeyard_db_actions::action_objects as objects;
use codeyard_db_actions::action_objects::ActionObject;
use codeyard_db_actions::action_objects::BlobPart;
use codeyard_db_actions::action_objects::NewObject;
use codeyard_db_actions::action_objects::RuntimeRun;
use codeyard_support::object_limits::MAX_OBJECT_BYTES;
use serde::Deserialize;
use serde_json::json;
use serde_json::Value;
use sha2::Digest;
use sha2::Sha256;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

pub fn router() -> Router<Arc<AppState>> {
    Router::new()
        .route("/twirp/:service/:method", post(twirp))
        .layer(DefaultBodyLimit::max(64 * 1024))
        .merge(
            Router::new()
                .route("/twirp/blobs/:id", get(get_blob).put(put_blob))
                .layer(DefaultBodyLimit::max(MAX_OBJECT_BYTES as usize)),
        )
}

fn denied() -> Error {
    Error(
        StatusCode::FORBIDDEN,
        "permission_denied",
        "Run credential or blob capability refused",
    )
}

#[derive(Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
struct Request {
    key: String,
    version: Value,
    name: String,
    #[serde(alias = "restore_keys")]
    restore_keys: Vec<String>,
    #[serde(alias = "workflow_run_backend_id")]
    workflow_run_backend_id: String,
    #[serde(alias = "workflow_job_run_backend_id")]
    workflow_job_run_backend_id: String,
    #[serde(alias = "size_bytes")]
    size_bytes: Value,
    size: Value,
    #[serde(alias = "expires_at")]
    expires_at: Option<String>,
    #[serde(alias = "name_filter")]
    name_filter: Option<String>,
    #[serde(alias = "id_filter")]
    id_filter: Option<String>,
}

fn valid_name(name: &str) -> bool {
    !name.is_empty() && name.len() <= 512 && !name.chars().any(char::is_control)
}

fn credential_hash(headers: &HeaderMap) -> Result<String, Error> {
    let token = codeyard_api_http::credentials::bearer(headers).ok_or_else(denied)?;
    Ok(codeyard_support::token_hash::token_hash(token))
}

async fn authenticate(state: &AppState, headers: &HeaderMap) -> Result<RuntimeRun, Error> {
    objects::authenticate(&state.pool, &credential_hash(headers)?)
        .await?
        .ok_or_else(denied)
}

async fn signed_url(
    state: &AppState,
    object: &ActionObject,
    run: i64,
    write: bool,
) -> Result<String, Error> {
    let token = codeyard_app::ci::runtime::secret();
    if !objects::grant_blob(
        &state.pool,
        object.id,
        run,
        &codeyard_support::token_hash::token_hash(&token),
        write,
    )
    .await?
    {
        return Err(denied());
    }
    let site = codeyard_db_actions::actions::get_actions_settings(&state.pool).await?;
    Ok(format!(
        "{}/twirp/blobs/{}?sig={token}",
        codeyard_app::ci::runtime::results_url(&site),
        object.id
    ))
}

async fn twirp(
    State(state): State<Arc<AppState>>,
    Path((service, method)): Path<(String, String)>,
    headers: HeaderMap,
    request: axum::extract::Request,
) -> Result<Json<Value>, Error> {
    let run = authenticate(&state, &headers).await?;
    let bytes = tokio::time::timeout(
        Duration::from_secs(30),
        axum::body::to_bytes(request.into_body(), 64 * 1024),
    )
    .await
    .map_err(|_| {
        Error(
            StatusCode::REQUEST_TIMEOUT,
            "deadline_exceeded",
            "Results body timed out",
        )
    })?
    .map_err(|_| invalid())?;
    let request: Request = serde_json::from_slice(&bytes).map_err(|_| invalid())?;
    let site = codeyard_db_actions::actions::get_actions_settings(&state.pool).await?;
    let repo =
        codeyard_db_actions::actions::get_repo_actions_settings(&state.pool, run.repo_id).await?;
    let cache_limit = site.cache_size_bytes(&repo);
    let now = chrono::Utc::now().timestamp();
    let artifact = service == "github.actions.results.api.v1.ArtifactService";
    if artifact {
        if request.workflow_run_backend_id != run.backend_id
            || request.workflow_job_run_backend_id != run.backend_id
        {
            return Err(denied());
        }
    } else if service != "github.actions.results.api.v1.CacheService" {
        return Err(missing());
    }
    let name = if artifact {
        &request.name
    } else {
        &request.key
    };
    let kind = if artifact { "artifact" } else { "cache" };
    let version = if artifact {
        ""
    } else {
        request.version.as_str().ok_or_else(invalid)?
    };
    let value = match (artifact, method.as_str()) {
        (false, "CreateCacheEntry") | (true, "CreateArtifact") => {
            if !valid_name(name)
                || version.len() > 512
                || (artifact && request.version.as_i64() != Some(4))
            {
                return Err(invalid());
            }
            let days = if artifact {
                let (repo_policy, org_policy) =
                    codeyard_db_actions::action_settings::effective(&state.pool, run.repo_id)
                        .await?;
                let private = codeyard_db_repos::repositories::records::get_repository_by_id(
                    &state.pool,
                    run.repo_id,
                )
                .await?
                .is_some_and(|repository| repository.is_private);
                codeyard_db_core::queries::action_settings::retention(
                    &repo_policy,
                    &org_policy,
                    site.artifact_retention_days,
                    private,
                )
            } else {
                site.cache_expiry_days(&repo)
            };
            let mut expires_at = now + i64::from(days) * 86400;
            if artifact {
                if let Some(expires) = &request.expires_at {
                    let requested = chrono::DateTime::parse_from_rfc3339(expires)
                        .map_err(|_| invalid())?
                        .timestamp();
                    if requested <= now {
                        return Err(invalid());
                    }
                    expires_at = expires_at.min(requested);
                }
            }
            let object = objects::create_object(
                &state.pool,
                NewObject {
                    run: &run,
                    kind,
                    name,
                    version,
                    expires_at,
                },
            )
            .await?;
            match object {
                Some(object) => {
                    json!({"ok":true,"signedUploadUrl":signed_url(&state,&object,run.id,true).await?})
                }
                None => {
                    return Err(Error(
                        StatusCode::CONFLICT,
                        "already_exists",
                        "Entry already exists or cache scope is read-only",
                    ))
                }
            }
        }
        (false, "FinalizeCacheEntryUpload") | (true, "FinalizeArtifact") => {
            let object = objects::own_object(&state.pool, run.id, kind, name, version)
                .await?
                .ok_or_else(missing)?;
            let size = size(if artifact {
                &request.size
            } else {
                &request.size_bytes
            })?;
            if !objects::finalize_object(
                &state.pool,
                run.id,
                &credential_hash(&headers)?,
                object.id,
                size,
                cache_limit,
            )
            .await?
            {
                return Err(invalid());
            }
            if artifact {
                json!({"ok":true,"artifactId":object.id.to_string()})
            } else {
                json!({"ok":true,"entryId":object.id.to_string()})
            }
        }
        (false, "GetCacheEntryDownloadURL") => {
            if !valid_name(name)
                || request.restore_keys.len() > 10
                || request.restore_keys.iter().any(|key| !valid_name(key))
            {
                return Err(invalid());
            }
            let keys = std::iter::once(request.key.clone())
                .chain(request.restore_keys)
                .collect::<Vec<_>>();
            match objects::find_cache(&state.pool, &run, &keys, version).await? {
                Some(object) => {
                    json!({"ok":true,"matchedKey":object.name,"signedDownloadUrl":signed_url(&state,&object,run.id,false).await?})
                }
                None => json!({"ok":false}),
            }
        }
        (true, "ListArtifacts") => {
            let id = request
                .id_filter
                .as_deref()
                .map(str::parse::<i64>)
                .transpose()
                .map_err(|_| invalid())?;
            let list =
                objects::list_artifacts(&state.pool, run.id, request.name_filter.as_deref(), id)
                    .await?;
            let list: Vec<_> = list.into_iter()
                .map(|o| json!({"workflowRunBackendId":run.backend_id,"workflowJobRunBackendId":run.backend_id,"databaseId":o.id.to_string(),"name":o.name,"size":o.size.to_string(),"createdAt":chrono::DateTime::from_timestamp(o.created_at,0).map(|t| t.to_rfc3339())})).collect();
            json!({"artifacts":list})
        }
        (true, "GetSignedArtifactURL") | (true, "DeleteArtifact") => {
            let object = if method == "DeleteArtifact" {
                objects::own_object(&state.pool, run.id, "artifact", name, "")
                    .await?
                    .filter(|o| o.finalized)
            } else {
                objects::artifact_for_run(&state.pool, run.id, name).await?
            }
            .ok_or_else(missing)?;
            if method == "DeleteArtifact" {
                if !objects::delete_artifact(&state.pool, object.id, run.id, None, None).await? {
                    return Err(missing());
                }
                json!({"ok":true,"artifactId":object.id.to_string()})
            } else {
                json!({"signedUrl":signed_url(&state,&object,run.id,false).await?})
            }
        }
        _ => return Err(missing()),
    };
    Ok(Json(value))
}

async fn capability(
    state: &AppState,
    id: i64,
    query: &HashMap<String, String>,
    write: bool,
) -> Result<ActionObject, Error> {
    let sig = query.get("sig").ok_or_else(denied)?;
    objects::blob_grant(
        &state.pool,
        id,
        &codeyard_support::token_hash::token_hash(sig),
        write,
    )
    .await?
    .ok_or_else(denied)
}

/// Azure emits only base64 block IDs, so no XML entities or arbitrary nested content is needed.
fn block_list(bytes: &[u8]) -> Result<Vec<String>, Error> {
    let mut text = std::str::from_utf8(bytes).map_err(|_| invalid())?.trim();
    if text.starts_with("<?xml ") {
        text = text.split_once("?>").ok_or_else(invalid)?.1.trim();
    }
    let mut text = text
        .strip_prefix("<BlockList>")
        .and_then(|v| v.strip_suffix("</BlockList>"))
        .ok_or_else(invalid)?
        .trim();
    let mut blocks = Vec::new();
    while !text.is_empty() {
        let tag = ["Latest", "Uncommitted", "Committed"]
            .into_iter()
            .find(|tag| text.starts_with(&format!("<{tag}>")))
            .ok_or_else(invalid)?;
        let rest = text.strip_prefix(&format!("<{tag}>")).ok_or_else(invalid)?;
        let (id, rest) = rest.split_once(&format!("</{tag}>")).ok_or_else(invalid)?;
        if !valid_block_id(id) || blocks.len() >= 50000 {
            return Err(invalid());
        }
        blocks.push(id.to_owned());
        text = rest.trim();
    }
    Ok(blocks)
}
fn valid_block_id(id: &str) -> bool {
    use base64::Engine;
    id.len() <= 128
        && base64::engine::general_purpose::STANDARD
            .decode(id)
            .is_ok_and(|v| !v.is_empty() && v.len() <= 64)
}

async fn put_blob(
    State(state): State<Arc<AppState>>,
    Path(id): Path<i64>,
    Query(query): Query<HashMap<String, String>>,
    request: axum::extract::Request,
) -> Result<Response, Error> {
    let object = capability(&state, id, &query, true).await?;
    let grant_hash = codeyard_support::token_hash::token_hash(query.get("sig").ok_or_else(denied)?);
    let io = Arc::new(state.storage.try_acquire_io()?);
    let _memory = state
        .upload_reads
        .clone()
        .try_acquire_owned()
        .map_err(|_| {
            Error(
                StatusCode::SERVICE_UNAVAILABLE,
                "unavailable",
                "Uploads busy",
            )
        })?;
    let comp = query.get("comp").map(String::as_str).unwrap_or("");
    if !matches!(comp, "" | "block" | "blocklist") {
        return Err(invalid());
    }
    // The cache toolkit uses single-shot PUTs up to 128 MiB, and 64 MiB blocks above that.
    let max = match comp {
        "blocklist" => 8 * 1024 * 1024,
        "" => 128 * 1024 * 1024,
        _ => MAX_OBJECT_BYTES as usize,
    };
    let bytes = tokio::time::timeout(
        Duration::from_secs(300),
        axum::body::to_bytes(request.into_body(), max),
    )
    .await
    .map_err(|_| {
        Error(
            StatusCode::REQUEST_TIMEOUT,
            "deadline_exceeded",
            "Blob upload timed out",
        )
    })?
    .map_err(|_| {
        Error(
            StatusCode::PAYLOAD_TOO_LARGE,
            "invalid_argument",
            "Blob block too large",
        )
    })?;
    if comp == "blocklist" {
        if bytes.len() > 8 * 1024 * 1024
            || !objects::seal_blob(&state.pool, id, &grant_hash, &block_list(&bytes)?).await?
        {
            return Err(invalid());
        }
    } else if comp == "block" || comp.is_empty() {
        let block = if comp == "block" {
            query
                .get("blockid")
                .filter(|id| valid_block_id(id))
                .ok_or_else(invalid)?
                .clone()
        } else {
            "cGxhaW4=".into()
        };
        if bytes.is_empty() {
            return Err(invalid());
        }
        let site = codeyard_db_actions::actions::get_actions_settings(&state.pool).await?;
        let repo =
            codeyard_db_actions::actions::get_repo_actions_settings(&state.pool, object.repo_id)
                .await?;
        let limit = if object.kind == "cache" {
            site.cache_size_bytes(&repo)
        } else {
            10 * 1024 * 1024 * 1024
        };
        // A single-shot PUT replaces an unfinished upload, so a retry after a lost response works.
        if comp.is_empty() {
            objects::reopen_blob(&state.pool, id, &grant_hash).await?;
        }
        let mut blocks = Vec::new();
        let chunk_size = if comp.is_empty() {
            64 * 1024 * 1024
        } else {
            MAX_OBJECT_BYTES as usize
        };
        let upload_id = uuid::Uuid::new_v4();
        for (index, bytes) in bytes.chunks(chunk_size).enumerate() {
            let block_id = if comp.is_empty() {
                format!("plain-{upload_id}-{index}")
            } else {
                block.clone()
            };
            let part = BlobPart {
                block_id: block_id.clone(),
                object_key: format!("actions/{}", uuid::Uuid::new_v4()),
                size: bytes.len() as i64,
            };
            if !objects::stage_block(&state.pool, id, &grant_hash, &part, limit).await? {
                return Err(invalid());
            }
            let hash = hex::encode(Sha256::digest(bytes));
            let upload = Upload {
                key: &part.object_key,
                bytes: part.size,
                content_type: "application/octet-stream",
                sha256_hex: &hash,
            };
            tokio::time::timeout(
                Duration::from_secs(300),
                state.storage.put_bytes(&upload, bytes.to_vec(), io.clone()),
            )
            .await
            .map_err(|_| {
                Error(
                    StatusCode::REQUEST_TIMEOUT,
                    "deadline_exceeded",
                    "Blob upload timed out",
                )
            })??;
            if !objects::ready_block(&state.pool, id, &grant_hash, &part.object_key).await? {
                return Err(invalid());
            }
            blocks.push(block_id);
        }
        if comp.is_empty() && !objects::seal_blob(&state.pool, id, &grant_hash, &blocks).await? {
            return Err(invalid());
        }
    } else {
        return Err(invalid());
    }
    Ok((
        StatusCode::CREATED,
        [
            (header::ETAG, "\"codeyard-blob\""),
            (
                header::HeaderName::from_static("x-ms-request-id"),
                "codeyard",
            ),
        ],
    )
        .into_response())
}

async fn get_blob(
    State(state): State<Arc<AppState>>,
    Path(id): Path<i64>,
    Query(query): Query<HashMap<String, String>>,
    headers: HeaderMap,
) -> Result<Response, Error> {
    let object = capability(&state, id, &query, false).await?;
    if !object.finalized {
        return Err(missing());
    }
    download(state, object, &headers).await
}

use codeyard_api_http::shared::ci::results::Error;

use codeyard_api_http::shared::ci::results::download;

use codeyard_api_http::shared::ci::results::invalid;

use codeyard_api_http::shared::ci::results::missing;

use codeyard_api_http::shared::ci::results::size;
