//! Source-import status and retry use the same durable attempt as the background Git transfer.
use axum::response::IntoResponse;

use axum::body::to_bytes;
use axum::extract::Path;
use axum::extract::Request;
use axum::extract::State;
use axum::http::header;
use axum::http::HeaderMap;
use axum::http::StatusCode;
use axum::response::Response;
use axum::routing::delete;
use axum::routing::get;
use axum::routing::patch;
use axum::routing::put;
use axum::routing::MethodRouter;
use axum::Json;
use codeyard_repos_core::api_shared::github::contents::store_error;
use codeyard_app::api_shared::github::repository;
use codeyard_app::app::AppState;
use codeyard_repos_core::repositories::store::repo_store;
use codeyard_db_repos::source_imports::SourceImport;
use codeyard_model::github_error::Error;
use codeyard_model::github_error::Result;
use codeyard_model::urls::repo_url;
use serde::Deserialize;
use serde_json::json;
use serde_json::Value;
use std::sync::Arc;

mod authors;
mod export_worker;
mod exports;

pub(super) fn aliases() -> axum::Router<Arc<AppState>> {
    axum::Router::new().route(
        "/migration-archives/:guid",
        get(crate::migrations::exports::archive),
    )
}

pub(super) fn handlers() -> Vec<(&'static str, MethodRouter<Arc<AppState>>)> {
    let mut handlers = vec![
        ("migrations/get-import-status", get(status)),
        ("migrations/start-import", put(start)),
        ("migrations/update-import", patch(retry)),
        ("migrations/cancel-import", delete(cancel)),
    ];
    handlers.extend(crate::migrations::exports::handlers());
    handlers.extend(crate::migrations::authors::handlers());
    handlers
}

async fn administrator(
    state: &AppState,
    headers: &HeaderMap,
    owner: &str,
    name: &str,
    write: bool,
) -> Result<(
    codeyard_db_core::models::Repository,
    codeyard_repos_core::api_shared::github::auth::Authentication,
)> {
    let (repo, actor) = repository(state, headers, owner, name, write).await?;
    let actor = actor.ok_or_else(codeyard_repos_core::api_shared::github::auth::required)?;
    if !codeyard_identity::repository_access::can_admin(&state.pool, &actor.user, &repo)
        .await
        .map_err(Error::unavailable)?
    {
        return Err(Error::new(
            StatusCode::FORBIDDEN,
            "Repository administration denied",
        ));
    }
    Ok((repo, actor))
}

fn document(import: &SourceImport, owner: &str, name: &str) -> Value {
    let base = repo_url(owner, name);
    let status = import.state();
    json!({
        "vcs": "git", "vcs_url": import.vcs_url, "status": status,
        "status_text": match status { "complete" => "Import complete", "error" => "Import failed; retry with updated credentials", _ => "Importing Git data" },
        "error_message": if status == "error" { Some("Import failed or timed out. Git data already published is retained.") } else { None },
        "url": format!("{base}/import"), "repository_url": base,
        "html_url": format!("{}/{}/{}", codeyard_model::http_config::link_origin(), codeyard_webui::encode_component(owner), codeyard_webui::encode_component(name)),
        "authors_url": format!("{base}/import/authors"),
    })
}

async fn status(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, name)): Path<(String, String)>,
) -> Result<Json<Value>> {
    let (repo, _) = administrator(&state, &headers, &owner, &name, false).await?;
    let import = codeyard_db_repos::source_imports::get(&state.pool, repo.id)
        .await
        .map_err(Error::unavailable)?
        .ok_or_else(Error::missing)?;
    Ok(Json(document(&import, &owner, &name)))
}

#[derive(Default, Deserialize)]
struct Input {
    vcs_url: Option<String>,
    vcs: Option<String>,
    vcs_username: Option<String>,
    vcs_password: Option<String>,
}

fn credentials(input: &Input) -> Result<Option<codeyard_repos::imports::Credentials>> {
    if input.vcs.as_deref().is_some_and(|vcs| vcs != "git") {
        return Err(Error::validation("Import", "vcs", "invalid"));
    }
    let username = input.vcs_username.clone().unwrap_or_else(|| "git".into());
    let password = input.vcs_password.clone().unwrap_or_default();
    Ok(
        if input.vcs_password.is_some() || input.vcs_username.is_some() {
            Some(codeyard_repos::imports::Credentials { username, password })
        } else {
            None
        },
    )
}

async fn begin(
    state: Arc<AppState>,
    repo: codeyard_db_core::models::Repository,
    actor: codeyard_repos_core::api_shared::github::auth::Authentication,
    input: Input,
    url: &str,
) -> Result<SourceImport> {
    codeyard_repos_pulls::repositories::imports::start(
        state,
        repo,
        codeyard_repos::imports::ImportCredential::Token(actor.token.id),
        url,
        credentials(&input)?,
    )
    .await
    .map_err(|error| match error {
        codeyard_repos::imports::ImportError::Source => {
            Error::validation("Import", "vcs_url", "invalid")
        }
        codeyard_repos::imports::ImportError::Credentials => {
            Error::validation("Import", "vcs_password", "invalid")
        }
        codeyard_repos::imports::ImportError::Denied => {
            Error::new(StatusCode::FORBIDDEN, error.to_string())
        }
        codeyard_repos::imports::ImportError::NotEmpty
        | codeyard_repos::imports::ImportError::Running => {
            Error::new(StatusCode::CONFLICT, error.to_string())
        }
        codeyard_repos::imports::ImportError::Unavailable => Error::unavailable(error),
    })
}

async fn start(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, name)): Path<(String, String)>,
    Json(input): Json<Input>,
) -> Result<Response> {
    let (repo, actor) = administrator(&state, &headers, &owner, &name, true).await?;
    let url = input
        .vcs_url
        .clone()
        .ok_or_else(|| Error::validation("Import", "vcs_url", "missing_field"))?;
    let import = begin(state, repo, actor, input, &url).await?;
    Ok((
        StatusCode::CREATED,
        [(
            header::LOCATION,
            format!("{}/import", repo_url(&owner, &name)),
        )],
        Json(document(&import, &owner, &name)),
    )
        .into_response())
}

async fn retry(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, name)): Path<(String, String)>,
    request: Request,
) -> Result<Json<Value>> {
    let (repo, actor) = administrator(&state, &headers, &owner, &name, true).await?;
    let previous = codeyard_db_repos::source_imports::get(&state.pool, repo.id)
        .await
        .map_err(Error::unavailable)?
        .ok_or_else(Error::missing)?;
    let bytes = to_bytes(request.into_body(), 16 * 1024)
        .await
        .map_err(codeyard_model::github_error::json_bytes_error)?;
    let input = if bytes.is_empty() {
        Input::default()
    } else {
        serde_json::from_slice::<Option<Input>>(&bytes)
            .map_err(codeyard_model::github_error::json_parse_error)?
            .unwrap_or_default()
    };
    let import = begin(state, repo, actor, input, &previous.vcs_url).await?;
    Ok(Json(document(&import, &owner, &name)))
}

async fn cancel(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, name)): Path<(String, String)>,
) -> Result<StatusCode> {
    let (repo, actor) = administrator(&state, &headers, &owner, &name, true).await?;
    repo_store(&repo, state.cluster.as_ref())
        .cancel_import_as(&state.pool, repo.id, actor.user.id)
        .await
        .map_err(store_error)?;
    Ok(StatusCode::NO_CONTENT)
}
