//! Git smart HTTP. Basic auth accepts a personal access token, never a password or session.
use futures_util::StreamExt;
use futures_util::TryStreamExt;
use std::io::Read;

use axum::body::to_bytes;
use axum::body::Body;
use axum::extract::Path;
use axum::extract::Query;
use axum::extract::Request;
use axum::extract::State;
use axum::http::header;
use axum::http::HeaderMap;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::response::Response;
use codeyard_app::app::AppState;
use codeyard_app::repositories::git_sessions::ReceiveCredential;
use codeyard_app::repositories::store::repo_store;
use codeyard_app::repositories::store::types::GitService;
use codeyard_app::repositories::store::types::GitSession;
use codeyard_app::repositories::store::types::GitStreams;
use codeyard_db_core::models::Repository;
use codeyard_git::error::RepoError;
use codeyard_git::repository::process as git_process;
use codeyard_support::token_hash::token_hash;
use serde::Deserialize;
use std::sync::Arc;
use tokio::sync::oneshot;
use tokio_util::io::ReaderStream;
use tokio_util::io::StreamReader;

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
    if error.reason == Some(codeyard_git::error::RepoErrorReason::SessionsBusy) {
        return (StatusCode::SERVICE_UNAVAILABLE, "Git sessions busy").into_response();
    }
    if error.is_storage_node_unavailable() {
        return (StatusCode::SERVICE_UNAVAILABLE, "storage node unavailable").into_response();
    }
    unavailable()
}

async fn authenticate(
    state: &AppState,
    headers: &HeaderMap,
) -> Result<
    Option<(
        codeyard_db_core::models::PersonalAccessToken,
        codeyard_db_core::models::User,
    )>,
    Response,
> {
    if !headers.contains_key(header::AUTHORIZATION) {
        return Ok(None);
    }
    let presented = codeyard_api_http::credentials::basic(headers).map(|(_, token)| token);
    let found = match presented.as_deref() {
        Some(value) => {
            codeyard_db_identity::users::tokens::find_token(&state.pool, &token_hash(value))
                .await
                .map_err(|_| unavailable())?
        }
        None => None,
    };
    let mut user_id = None;
    if let Some(token) = found {
        match codeyard_db_identity::users::tokens::token_user(&state.pool, token.id, false)
            .await
            .map_err(|_| unavailable())?
        {
            Some(user) => {
                codeyard_app::api_shared::github::auth::touch(state, &token, headers).await;
                return Ok(Some((token, user)));
            }
            None => user_id = Some(token.user_id),
        }
    }
    // Successful Git requests do not spend the failed-login budget: a clone/fetch can make many
    // RPCs. A refused token counts against the client address and the token's user. An unknown
    // token is its own account key, like an unknown username.
    match codeyard_db_identity::auth_admission::admit_login(
        &state.pool,
        codeyard_app::api_shared::http::session::client_address(headers),
        user_id,
        presented.as_deref().unwrap_or("git-http"),
    )
    .await
    {
        Ok(_) => Err(challenge()),
        Err(codeyard_db_identity::auth_admission::AdmissionError::Limited) => {
            Err((StatusCode::TOO_MANY_REQUESTS, "Too many failed Git logins").into_response())
        }
        Err(codeyard_db_identity::auth_admission::AdmissionError::Unavailable) => {
            Err(unavailable())
        }
    }
}

#[derive(Debug)]
struct AuthorizedRepository {
    repo: Repository,
    token_id: Option<i64>,
    actor_id: Option<i64>,
}

async fn repository(
    state: &AppState,
    owner: &str,
    name: &str,
    headers: &HeaderMap,
    write: bool,
) -> Result<AuthorizedRepository, Response> {
    let found = codeyard_db_repos::repositories::records::get_git_repository(
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
) -> Result<AuthorizedRepository, Response> {
    let config = codeyard_db_cluster::node_config::get_node_configuration(&state.pool)
        .await
        .map_err(|_| unavailable())?
        .filter(|config| config.setup_completed)
        .ok_or_else(|| StatusCode::NOT_FOUND.into_response())?;
    let token = authenticate(state, headers).await?;
    let Some((token, _)) = token else {
        // A repository that does not exist looks like a private one, so names are not revealed.
        return match found {
            Some(repo) if !(write || repo.is_private || config.is_private()) => {
                Ok(AuthorizedRepository {
                    repo,
                    token_id: None,
                    actor_id: None,
                })
            }
            _ => Err(challenge()),
        };
    };
    let repo = found.ok_or_else(|| StatusCode::NOT_FOUND.into_response())?;
    let (user, _) = ReceiveCredential::Token(token.id)
        .authorize(&state.pool, &repo, write)
        .await
        .map_err(credential_refusal)?;
    Ok(AuthorizedRepository {
        repo,
        token_id: Some(token.id),
        actor_id: Some(user.id),
    })
}

fn credential_refusal(error: anyhow::Error) -> Response {
    use codeyard_app::repositories::git_sessions::CredentialRefusal;
    let message = match error.downcast_ref::<CredentialRefusal>() {
        Some(CredentialRefusal::Credential) => return challenge(),
        Some(CredentialRefusal::Missing) => return StatusCode::NOT_FOUND.into_response(),
        Some(CredentialRefusal::ReadOnly) => "Token is read-only",
        Some(CredentialRefusal::TokenScope) => "Resource not accessible by token",
        Some(CredentialRefusal::Integration) => "Resource not accessible by integration",
        Some(CredentialRefusal::Classic) => {
            "This organization forbids access with a personal access token (classic)"
        }
        Some(CredentialRefusal::Migration) => "Repository is locked for migration",
        Some(CredentialRefusal::Denied | CredentialRefusal::Archived) => "Repository access denied",
        None => return unavailable(),
    };
    (StatusCode::FORBIDDEN, message).into_response()
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
    let AuthorizedRepository { repo, .. } =
        match repository(&state, &owner, &name, &headers, write).await {
            Ok(repo) => repo,
            Err(response) => return response,
        };
    let Some(permit) =
        codeyard_api_http::middleware::wait_for_slot(&state.repository_downloads).await
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
    let found =
        match codeyard_db_repos::repositories::records::get_repository_by_id(&state.pool, repo.id)
            .await
        {
            Ok(found) => found,
            Err(_) => return unavailable(),
        };
    if let Err(response) = authorize_repository(&state, found, &headers, write).await {
        return response;
    }
    let service = query.service.unwrap();
    let mut body = Vec::new();
    codeyard_repos::source::append_packet(&mut body, &format!("# service={service}\n"));
    body.extend_from_slice(b"0000");
    body.extend(refs);
    git_response(
        &format!("application/x-{service}-advertisement"),
        codeyard_api_http::middleware::buffered_download_body(body, permit),
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
    let AuthorizedRepository { repo, .. } =
        match repository(&state, &owner, &name, &parts.headers, write).await {
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
    let found =
        match codeyard_db_repos::repositories::records::get_repository_by_id(&state.pool, repo.id)
            .await
        {
            Ok(found) => found,
            Err(_) => return unavailable(),
        };
    let AuthorizedRepository {
        repo,
        token_id,
        actor_id,
    } = match authorize_repository(&state, found, &parts.headers, write).await {
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
    let visitor = if write {
        None
    } else {
        match actor_id {
            Some(id) => codeyard_db_repos::repositories::traffic::Identity::account(id),
            None => codeyard_app::api_shared::http::session::client_address(&parts.headers)
                .and_then(|address| {
                    codeyard_db_repos::repositories::traffic::Identity::client(
                        address,
                        parts
                            .headers
                            .get(header::USER_AGENT)
                            .and_then(|header| header.to_str().ok())
                            .unwrap_or(""),
                    )
                }),
        }
    };
    let (output, reader) = tokio::io::duplex(git_process::CHUNK);
    let (finished, result) = oneshot::channel();
    let response_state = state.clone();
    let repository_id = repo.id;
    // The task owns the input body and hooks. Dropping the response only closes
    // output; Git rejects incomplete packs and finishes hooks for complete ones.
    let git_tasks = state.git_tasks.clone();
    if !git_tasks.spawn(async move {
        let _permit = permit;
        let observation = codeyard_repos::traffic::clone::Observation::default();
        if !write {
            input = Box::new(observation.input(input));
        }
        let mut output: Box<dyn tokio::io::AsyncWrite + Unpin + Send> = if write {
            Box::new(output)
        } else {
            Box::new(observation.output(output))
        };
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
        if status.is_ok() && !write && observation.initial_transfer() {
            if let Some(visitor) = visitor {
                codeyard_repos::traffic::observe(
                    &state.pool,
                    repo.id,
                    &visitor,
                    &codeyard_db_repos::repositories::traffic::Metric::clone_transfer(),
                )
                .await;
            }
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
        let found = match codeyard_db_repos::repositories::records::get_repository_by_id(
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
