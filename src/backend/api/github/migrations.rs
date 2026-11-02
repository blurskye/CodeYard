//! Source-import status and retry use the same durable attempt as the background Git transfer.
use super::{auth, contents::store_error, repo_url, repository, Error, Result};
use crate::backend::{
    app::AppState,
    db::{self, source_imports::SourceImport},
    repositories::{access, imports, store::repo_store},
};
use axum::{
    body::to_bytes,
    extract::{Path, Request, State},
    http::{header, HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{delete, get, patch, put, MethodRouter},
    Json,
};
use serde::Deserialize;
use serde_json::{json, Value};
use std::sync::Arc;

mod authors;
mod export_worker;
mod exports;

pub(super) fn aliases() -> axum::Router<Arc<AppState>> {
    axum::Router::new().route("/migration-archives/:guid", get(exports::archive))
}

pub(super) fn handlers() -> Vec<(&'static str, MethodRouter<Arc<AppState>>)> {
    let mut handlers = vec![
        ("migrations/get-import-status", get(status)),
        ("migrations/start-import", put(start)),
        ("migrations/update-import", patch(retry)),
        ("migrations/cancel-import", delete(cancel)),
    ];
    handlers.extend(exports::handlers());
    handlers.extend(authors::handlers());
    handlers
}

async fn administrator(
    state: &AppState,
    headers: &HeaderMap,
    owner: &str,
    name: &str,
    write: bool,
) -> Result<(db::models::Repository, auth::Authentication)> {
    let (repo, actor) = repository(state, headers, owner, name, write).await?;
    let actor = actor.ok_or_else(auth::required)?;
    if !access::can_admin(&state.pool, &actor.user, &repo)
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
        "html_url": format!("{}/{}/{}", crate::backend::api::http::config::link_origin(), crate::webui::encode_component(owner), crate::webui::encode_component(name)),
        "authors_url": format!("{base}/import/authors"),
    })
}

async fn status(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, name)): Path<(String, String)>,
) -> Result<Json<Value>> {
    let (repo, _) = administrator(&state, &headers, &owner, &name, false).await?;
    let import = db::source_imports::get(&state.pool, repo.id)
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

fn credentials(input: &Input) -> Result<Option<imports::Credentials>> {
    if input.vcs.as_deref().is_some_and(|vcs| vcs != "git") {
        return Err(Error::validation("Import", "vcs", "invalid"));
    }
    let username = input.vcs_username.clone().unwrap_or_else(|| "git".into());
    let password = input.vcs_password.clone().unwrap_or_default();
    Ok(
        if input.vcs_password.is_some() || input.vcs_username.is_some() {
            Some(imports::Credentials { username, password })
        } else {
            None
        },
    )
}

async fn begin(
    state: Arc<AppState>,
    repo: db::models::Repository,
    actor: auth::Authentication,
    input: Input,
    url: &str,
) -> Result<SourceImport> {
    imports::start(
        state,
        repo,
        imports::ImportCredential::Token(actor.token.id),
        url,
        credentials(&input)?,
    )
    .await
    .map_err(|error| match error {
        imports::ImportError::Source => Error::validation("Import", "vcs_url", "invalid"),
        imports::ImportError::Credentials => Error::validation("Import", "vcs_password", "invalid"),
        imports::ImportError::Denied => Error::new(StatusCode::FORBIDDEN, error.to_string()),
        imports::ImportError::NotEmpty | imports::ImportError::Running => {
            Error::new(StatusCode::CONFLICT, error.to_string())
        }
        imports::ImportError::Unavailable => Error::unavailable(error),
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
    let previous = db::source_imports::get(&state.pool, repo.id)
        .await
        .map_err(Error::unavailable)?
        .ok_or_else(Error::missing)?;
    let bytes = to_bytes(request.into_body(), 16 * 1024)
        .await
        .map_err(|_| Error::invalid("Invalid import request"))?;
    let input = if bytes.is_empty() {
        Input::default()
    } else {
        serde_json::from_slice::<Option<Input>>(&bytes)
            .map_err(|_| Error::invalid("Invalid import request"))?
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
