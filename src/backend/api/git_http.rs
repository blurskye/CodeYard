//! Git smart HTTP. Basic auth accepts a personal access token, never a password or session.
use crate::backend::api::http::session;
use crate::backend::db::models::{AccessLevel, Repository};
use crate::backend::identity::tokens::token_hash;
use crate::backend::repositories::git_sessions::ReceiveCredential;
use crate::backend::repositories::store::{
    repo_store, GitService, GitSession, GitStreams, RepoError,
};
use crate::backend::{
    app::AppState, db, git::repository::process as git_process, identity::admission,
};
use axum::{
    body::{to_bytes, Body},
    extract::{Path, Query, Request, State},
    http::{header, HeaderMap, StatusCode},
    response::{IntoResponse, Response},
};
use base64::{engine::general_purpose::STANDARD, Engine};
use futures_util::{StreamExt, TryStreamExt};
use serde::Deserialize;
use std::{io::Read, sync::Arc};
use tokio::sync::oneshot;
use tokio_util::io::{ReaderStream, StreamReader};

#[cfg(test)]
mod tests;

const UPLOAD_REQUEST_LIMIT: usize = 16 * 1024 * 1024;

#[derive(Deserialize)]
pub struct ServiceQuery {
    pub service: Option<String>,
}

fn challenge() -> Response {
    (
        StatusCode::UNAUTHORIZED,
        [(header::WWW_AUTHENTICATE, "Basic realm=\"Codeyard\"")],
        "Git token required",
    )
        .into_response()
}

fn unavailable() -> Response {
    (StatusCode::SERVICE_UNAVAILABLE, "Git operation unavailable").into_response()
}

fn store_unavailable(error: &RepoError) -> Response {
    if error.reason == Some(crate::backend::repositories::store::RepoErrorReason::SessionsBusy) {
        return (StatusCode::SERVICE_UNAVAILABLE, "Git sessions busy").into_response();
    }
    if error.is_storage_node_unavailable() {
        return (StatusCode::SERVICE_UNAVAILABLE, "storage node unavailable").into_response();
    }
    unavailable()
}

fn basic_token(headers: &HeaderMap) -> Option<String> {
    let mut values = headers.get_all(header::AUTHORIZATION).iter();
    let value = values.next()?.to_str().ok()?;
    if values.next().is_some() {
        return None;
    }
    let (scheme, encoded) = value.split_once(' ')?;
    if !scheme.eq_ignore_ascii_case("basic") || encoded.len() > 2048 {
        return None;
    }
    let decoded = String::from_utf8(STANDARD.decode(encoded).ok()?).ok()?;
    let (_, token) = decoded.split_once(':')?;
    Some(token.to_owned())
}

async fn authenticate(
    state: &AppState,
    headers: &HeaderMap,
) -> Result<Option<(db::models::PersonalAccessToken, db::models::User)>, Response> {
    if !headers.contains_key(header::AUTHORIZATION) {
        return Ok(None);
    }
    let presented = basic_token(headers);
    let found = match presented.as_deref() {
        Some(value) => db::users::tokens::find_token(&state.pool, &token_hash(value))
            .await
            .map_err(|_| unavailable())?,
        None => None,
    };
    let mut user_id = None;
    if let Some(token) = found {
        match db::users::tokens::token_user(&state.pool, token.id, false)
            .await
            .map_err(|_| unavailable())?
        {
            Some(user) => return Ok(Some((token, user))),
            None => user_id = Some(token.user_id),
        }
    }
    // Successful Git requests do not spend the failed-login budget: a clone/fetch can make many
    // RPCs. A refused token counts against the client address and the token's user. An unknown
    // token is its own account key, like an unknown username.
    match admission::admit_login(
        &state.pool,
        session::client_address(headers),
        user_id,
        presented.as_deref().unwrap_or("git-http"),
    )
    .await
    {
        Ok(_) => Err(challenge()),
        Err(admission::AdmissionError::Limited) => {
            Err((StatusCode::TOO_MANY_REQUESTS, "Too many failed Git logins").into_response())
        }
        Err(admission::AdmissionError::Unavailable) => Err(unavailable()),
    }
}

async fn repository(
    state: &AppState,
    owner: &str,
    name: &str,
    headers: &HeaderMap,
    write: bool,
) -> Result<(Repository, Option<i64>), Response> {
    let found = db::repositories::records::get_repository(
        &state.pool,
        owner,
        name.strip_suffix(".git").unwrap_or(name),
    )
    .await
    .map_err(|_| unavailable())?;
    authorize_repository(state, found, headers, write).await
}

async fn authorize_repository(
    state: &AppState,
    found: Option<Repository>,
    headers: &HeaderMap,
    write: bool,
) -> Result<(Repository, Option<i64>), Response> {
    let config = db::node_config::get_node_configuration(&state.pool)
        .await
        .map_err(|_| unavailable())?
        .filter(|config| config.setup_completed)
        .ok_or_else(|| StatusCode::NOT_FOUND.into_response())?;
    let token = authenticate(state, headers).await?;
    let Some((token, user)) = token else {
        // A repository that does not exist looks like a private one, so names are not revealed.
        return match found {
            Some(repo) if !(write || repo.is_private || config.is_private()) => Ok((repo, None)),
            _ => Err(challenge()),
        };
    };
    let repo = found.ok_or_else(|| StatusCode::NOT_FOUND.into_response())?;
    let access = crate::backend::repositories::access::user_access(&state.pool, &user, &repo)
        .await
        .map_err(|_| unavailable())?;
    if access < AccessLevel::Read {
        // A repository its user cannot read does not exist as far as they can tell.
        return Err(StatusCode::NOT_FOUND.into_response());
    }
    if write && token.scope != "write" {
        return Err((StatusCode::FORBIDDEN, "Token is read-only").into_response());
    }
    if write && (access < AccessLevel::Write || repo.is_archived) {
        return Err((StatusCode::FORBIDDEN, "Repository access denied").into_response());
    }
    Ok((repo, Some(token.id)))
}

pub async fn info_refs(
    State(state): State<Arc<AppState>>,
    Path((owner, name)): Path<(String, String)>,
    Query(query): Query<ServiceQuery>,
    headers: HeaderMap,
) -> Response {
    let service = match query.service.as_deref() {
        Some("git-upload-pack") => GitService::UploadPack,
        Some("git-receive-pack") => GitService::ReceivePack,
        _ => return StatusCode::NOT_FOUND.into_response(),
    };
    let write = matches!(service, GitService::ReceivePack);
    let (repo, _) = match repository(&state, &owner, &name, &headers, write).await {
        Ok(repo) => repo,
        Err(response) => return response,
    };
    let Some(permit) = super::http::middleware::wait_for_slot(&state.repository_downloads).await
    else {
        return unavailable();
    };
    let refs = match repo_store(&repo, state.cluster.as_ref())
        .advertise_refs(service)
        .await
    {
        Ok(refs) => refs,
        Err(error) => return store_unavailable(&error),
    };
    // The advertisement may have waited for a session slot, and access can change meanwhile. The
    // check is for the repository the refs came from, even if its name has been reused.
    let found = match db::repositories::records::get_repository_by_id(&state.pool, repo.id).await {
        Ok(found) => found,
        Err(_) => return unavailable(),
    };
    if let Err(response) = authorize_repository(&state, found, &headers, write).await {
        return response;
    }
    let service = query.service.unwrap();
    let line = format!("# service={service}\n");
    let mut body = format!("{:04x}{line}0000", line.len() + 4).into_bytes();
    body.extend(refs);
    git_response(
        &format!("application/x-{service}-advertisement"),
        super::http::middleware::buffered_download_body(body, permit),
    )
}

fn git_response(content_type: &str, body: impl Into<Body>) -> Response {
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, content_type)
        .header(header::CACHE_CONTROL, "no-store")
        .body(body.into())
        .unwrap()
}

async fn request_body(headers: &HeaderMap, body: Body) -> Result<Vec<u8>, Response> {
    let encoding = headers
        .get(header::CONTENT_ENCODING)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("identity");
    if !matches!(encoding, "gzip" | "identity") {
        return Err(StatusCode::UNSUPPORTED_MEDIA_TYPE.into_response());
    }
    let gzip = encoding == "gzip";
    let bytes = tokio::time::timeout(
        git_process::RPC_TIMEOUT,
        to_bytes(body, UPLOAD_REQUEST_LIMIT),
    )
    .await
    .map_err(|_| StatusCode::REQUEST_TIMEOUT.into_response())?
    .map_err(|_| StatusCode::PAYLOAD_TOO_LARGE.into_response())?;
    if !gzip {
        return Ok(bytes.to_vec());
    }
    tokio::task::spawn_blocking(move || {
        let mut decoded = Vec::new();
        flate2::read::GzDecoder::new(bytes.as_ref())
            .take(UPLOAD_REQUEST_LIMIT as u64 + 1)
            .read_to_end(&mut decoded)
            .map_err(|_| StatusCode::BAD_REQUEST)?;
        if decoded.len() > UPLOAD_REQUEST_LIMIT {
            return Err(StatusCode::PAYLOAD_TOO_LARGE);
        }
        Ok(decoded)
    })
    .await
    .map_err(|_| unavailable())?
    .map_err(IntoResponse::into_response)
}

async fn rpc(
    state: Arc<AppState>,
    owner: String,
    name: String,
    request: Request,
    write: bool,
) -> Response {
    let (parts, body) = request.into_parts();
    let expected = if write {
        "application/x-git-receive-pack-request"
    } else {
        "application/x-git-upload-pack-request"
    };
    if parts
        .headers
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        != Some(expected)
    {
        return (StatusCode::BAD_REQUEST, "Invalid content type").into_response();
    }
    let (repo, _) = match repository(&state, &owner, &name, &parts.headers, write).await {
        Ok(repo) => repo,
        Err(response) => return response,
    };
    let permit = match repo_store(&repo, state.cluster.as_ref())
        .session_permit()
        .await
    {
        Ok(permit) => permit,
        Err(_) => return (StatusCode::SERVICE_UNAVAILABLE, "Git sessions busy").into_response(),
    };
    let mut input: Box<dyn tokio::io::AsyncRead + Unpin + Send> = if write {
        if parts
            .headers
            .get_all(header::CONTENT_ENCODING)
            .iter()
            .any(|encoding| encoding != "identity")
        {
            return StatusCode::UNSUPPORTED_MEDIA_TYPE.into_response();
        }
        Box::new(StreamReader::new(
            body.into_data_stream().map_err(std::io::Error::other),
        ))
    } else {
        match request_body(&parts.headers, body).await {
            Ok(body) => Box::new(std::io::Cursor::new(body)),
            Err(response) => return response,
        }
    };
    // The wait for the slot and the body can both outlast credentials or access, so access is
    // checked again now. It is keyed by the original repository id, so a name reused meanwhile
    // does not stand in for it.
    let found = match db::repositories::records::get_repository_by_id(&state.pool, repo.id).await {
        Ok(found) => found,
        Err(_) => return unavailable(),
    };
    let (repo, token_id) = match authorize_repository(&state, found, &parts.headers, write).await {
        Ok(authorized) => authorized,
        Err(response) => return response,
    };
    let mut options = GitSession::new(
        if write {
            GitService::ReceivePack
        } else {
            GitService::UploadPack
        },
        true,
    );
    options.credential = token_id.map(ReceiveCredential::Token);
    if write {
        options.owner = owner;
    }
    let (mut output, reader) = tokio::io::duplex(git_process::CHUNK);
    let (finished, result) = oneshot::channel();
    let response_state = state.clone();
    let repository_id = repo.id;
    // The task owns the input body and hooks. Dropping the response only closes
    // output; Git rejects incomplete packs and finishes hooks for complete ones.
    let git_tasks = state.git_tasks.clone();
    if !git_tasks.spawn(async move {
        let _permit = permit;
        let store = repo_store(&repo, state.cluster.as_ref());
        let status = store
            .git_session(
                options,
                write.then(|| (state.pool.clone(), state.ci_runs.clone())),
                GitStreams {
                    input: &mut input,
                    output: &mut output,
                    errors: &mut tokio::io::sink(),
                },
            )
            .await
            .and_then(|status| {
                anyhow::ensure!(status.success(), "Git session failed");
                Ok(())
            });
        if let Err(error) = &status {
            tracing::warn!(%error, "Git HTTP session failed");
        }
        let _ = finished.send(status);
    }) {
        return unavailable();
    }
    let mut reader = ReaderStream::with_capacity(reader, git_process::CHUNK);
    let first = reader.next().await;
    // Remote storage may have waited for its own session slot. Do not disclose even the
    // first bytes until the original credential and repository have been checked again.
    if !write {
        let found = match db::repositories::records::get_repository_by_id(
            &response_state.pool,
            repository_id,
        )
        .await
        {
            Ok(found) => found,
            Err(_) => return unavailable(),
        };
        if let Err(response) =
            authorize_repository(&response_state, found, &parts.headers, false).await
        {
            return response;
        }
    }
    if first.is_none() {
        return match result.await {
            Ok(Ok(())) => git_response(
                if write {
                    "application/x-git-receive-pack-result"
                } else {
                    "application/x-git-upload-pack-result"
                },
                Body::empty(),
            ),
            Ok(Err(error)) => error
                .downcast_ref::<RepoError>()
                .map_or_else(unavailable, store_unavailable),
            Err(_) => unavailable(),
        };
    }
    let stream = futures_util::stream::try_unfold(
        (first, reader, Some(result)),
        |(mut first, mut reader, mut result)| async move {
            let bytes = if first.is_some() {
                first.take()
            } else {
                reader.next().await
            };
            if let Some(bytes) = bytes {
                return Ok(Some((bytes?, (first, reader, result))));
            }
            if let Some(result) = result.take() {
                result
                    .await
                    .map_err(std::io::Error::other)?
                    .map_err(std::io::Error::other)?;
            }
            Ok::<_, std::io::Error>(None)
        },
    );
    git_response(
        if write {
            "application/x-git-receive-pack-result"
        } else {
            "application/x-git-upload-pack-result"
        },
        Body::from_stream(stream),
    )
}

pub async fn upload_pack(
    State(state): State<Arc<AppState>>,
    Path((owner, name)): Path<(String, String)>,
    request: Request,
) -> Response {
    rpc(state, owner, name, request, false).await
}
pub async fn receive_pack(
    State(state): State<Arc<AppState>>,
    Path((owner, name)): Path<(String, String)>,
    request: Request,
) -> Response {
    rpc(state, owner, name, request, true).await
}

pub async fn info_refs_head(
    State(state): State<Arc<AppState>>,
    Path((owner, name)): Path<(String, String)>,
    headers: HeaderMap,
) -> Response {
    match repository(&state, &owner, &name, &headers, false).await {
        Ok(_) => StatusCode::OK.into_response(),
        Err(response) => response,
    }
}

pub fn router() -> axum::Router<Arc<AppState>> {
    axum::Router::new()
        .route(
            "/:owner/:repo/info/refs",
            axum::routing::get(info_refs).head(info_refs_head),
        )
        .route(
            "/:owner/:repo/git-upload-pack",
            axum::routing::post(upload_pack),
        )
        .route(
            "/:owner/:repo/git-receive-pack",
            axum::routing::post(receive_pack),
        )
}
