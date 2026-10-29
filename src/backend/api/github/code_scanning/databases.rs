//! CodeQL archives uploaded by workflows and downloaded by repository readers.
use super::{failure, scope};
use crate::backend::{
    api::github::{auth, repo_url, repository, Error, Result},
    app::AppState,
    db::{self, codeql},
    storage::{attachment_disposition, Download, Upload, MAX_OBJECT_BYTES},
    webhooks::payloads,
};
use axum::{
    body::{to_bytes, Body},
    extract::{Path, Query, Request, State},
    http::{header, HeaderMap, StatusCode},
    response::{IntoResponse, Response},
};
use serde::Deserialize;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{io::Cursor, sync::Arc, time::Duration};
use tokio_util::io::ReaderStream;

pub(crate) const UPLOAD_ROUTE: &str =
    "/api/uploads/repos/:owner/:repo/code-scanning/codeql/databases/:language";
pub(crate) const API_UPLOAD_ROUTE: &str =
    "/api/v3/repos/:owner/:repo/code-scanning/codeql/databases/:language";
pub(crate) const HOST_UPLOAD_ROUTE: &str =
    "/repos/:owner/:repo/code-scanning/codeql/databases/:language";

async fn document(
    state: &AppState,
    row: &codeql::Database,
    owner: &str,
    name: &str,
) -> Result<Value> {
    let uploader = match row.uploader_id {
        Some(id) => db::users::accounts::get_user_by_id(&state.pool, id)
            .await
            .map_err(Error::unavailable)?
            .as_ref()
            .map(payloads::user),
        None => None,
    }
    .unwrap_or_else(|| {
        payloads::person(&payloads::Person {
            login: "ghost".into(),
            id: 0,
            site_admin: false,
        })
    });
    Ok(
        json!({"id":row.id,"name":row.name,"language":row.language,"uploader":uploader,
        "content_type":"application/zip","size":row.size,"created_at":row.created_at,
        "updated_at":row.updated_at,"url":format!("{}/code-scanning/codeql/databases/{}",repo_url(owner,name),row.language),"commit_oid":null}),
    )
}

async fn readable(state: &AppState, headers: &HeaderMap, owner: &str, name: &str) -> Result<i64> {
    let (repo, auth) = repository(state, headers, owner, name, false).await?;
    auth.ok_or_else(auth::required)?;
    Ok(repo.id)
}

pub(super) async fn list(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, name)): Path<(String, String)>,
) -> Result<axum::Json<Value>> {
    let repo = readable(&state, &headers, &owner, &name).await?;
    let rows = codeql::list(&state.pool, repo).await.map_err(failure)?;
    let mut values = Vec::new();
    for row in rows {
        values.push(document(&state, &row, &owner, &name).await?);
    }
    Ok(axum::Json(json!(values)))
}

pub(super) async fn get(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, name, language)): Path<(String, String, String)>,
) -> Result<Response> {
    let repo = readable(&state, &headers, &owner, &name).await?;
    let row = codeql::get(&state.pool, repo, &language)
        .await
        .map_err(failure)?
        .ok_or_else(Error::missing)?;
    if !headers
        .get(header::ACCEPT)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| {
            value
                .split(',')
                .any(|media| media.split(';').next().unwrap_or("").trim() == "application/zip")
        })
    {
        return Ok(axum::Json(document(&state, &row, &owner, &name).await?).into_response());
    }
    let permit = state
        .storage
        .try_acquire_download()
        .ok_or_else(|| Error::unavailable(()))?;
    let mut response = match state
        .storage
        .download(&row.object_key, &row.name, None)
        .await
        .map_err(Error::unavailable)?
    {
        Download::Redirect(location) => {
            (StatusCode::FOUND, [(header::LOCATION, location)]).into_response()
        }
        Download::Stream { file, bytes } => {
            let stream = futures_util::stream::unfold(
                (ReaderStream::new(file), permit),
                |(mut reader, permit)| async move {
                    use futures_util::StreamExt;
                    reader.next().await.map(|chunk| (chunk, (reader, permit)))
                },
            );
            (
                [
                    (header::CONTENT_TYPE, "application/zip".to_owned()),
                    (header::CONTENT_LENGTH, bytes.to_string()),
                    (
                        header::CONTENT_DISPOSITION,
                        attachment_disposition(&row.name),
                    ),
                ],
                Body::from_stream(stream),
            )
                .into_response()
        }
    };
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, "private, no-store".parse().unwrap());
    response
        .headers_mut()
        .insert(header::X_CONTENT_TYPE_OPTIONS, "nosniff".parse().unwrap());
    Ok(response)
}

#[derive(Deserialize)]
pub(in crate::backend::api::github) struct UploadQuery {
    name: Option<String>,
}

pub(in crate::backend::api::github) async fn upload(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, name, language)): Path<(String, String, String)>,
    Query(query): Query<UploadQuery>,
    request: Request,
) -> Result<Response> {
    let (repo, auth) = scope(&state, &headers, &owner, &name, true, false).await?;
    if ![
        "cpp",
        "csharp",
        "go",
        "java",
        "javascript",
        "python",
        "ruby",
        "rust",
        "swift",
    ]
    .contains(&language.as_str())
    {
        return Err(Error::validation("CodeqlDatabase", "language", "invalid"));
    }
    let filename = query
        .name
        .ok_or_else(|| Error::validation("CodeqlDatabase", "name", "missing_field"))?;
    if filename.is_empty()
        || filename.len() > 255
        || filename.chars().any(char::is_control)
        || filename.contains(['/', '\\'])
    {
        return Err(Error::validation("CodeqlDatabase", "name", "invalid"));
    }
    if headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.split(';').next())
        != Some("application/zip")
    {
        return Err(Error::new(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "Expected application/zip",
        ));
    }
    let _permit = state
        .upload_reads
        .clone()
        .try_acquire_owned()
        .map_err(Error::unavailable)?;
    let _io = state.storage.try_acquire_io().map_err(Error::unavailable)?;
    let bytes = tokio::time::timeout(
        Duration::from_secs(30),
        to_bytes(request.into_body(), MAX_OBJECT_BYTES as usize),
    )
    .await
    .map_err(|_| Error::new(StatusCode::REQUEST_TIMEOUT, "Upload timed out"))?
    .map_err(|_| {
        Error::new(
            StatusCode::PAYLOAD_TOO_LARGE,
            "CodeQL database is too large",
        )
    })?;
    let bytes = tokio::task::spawn_blocking(move || {
        zip::ZipArchive::new(Cursor::new(&bytes)).map_err(|_| ())?;
        Ok::<_, ()>(bytes.to_vec())
    })
    .await
    .map_err(Error::unavailable)?
    .map_err(|_| Error::validation("CodeqlDatabase", "file", "invalid"))?;
    let size = bytes.len() as i64;
    let sha256 = format!("{:x}", Sha256::digest(&bytes));
    let key = format!("codeql/{}", uuid::Uuid::new_v4());
    codeql::stage(&state.pool, auth.user.id, repo.id, &key)
        .await
        .map_err(failure)?;
    state
        .storage
        .put_bytes(
            &Upload {
                key: &key,
                bytes: size,
                content_type: "application/zip",
                sha256_hex: &sha256,
            },
            bytes,
        )
        .await
        .map_err(Error::unavailable)?;
    let row = codeql::publish(
        &state.pool,
        &codeql::NewDatabase {
            repo: repo.id,
            actor: auth.user.id,
            language: &language,
            name: &filename,
            key: &key,
            size,
        },
    )
    .await
    .map_err(failure)?;
    Ok((
        StatusCode::CREATED,
        axum::Json(document(&state, &row, &owner, &name).await?),
    )
        .into_response())
}

pub(super) async fn delete(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, name, language)): Path<(String, String, String)>,
) -> Result<StatusCode> {
    let (repo, auth) = scope(&state, &headers, &owner, &name, true, false).await?;
    codeql::delete(&state.pool, auth.user.id, repo.id, &language)
        .await
        .map_err(failure)?;
    Ok(StatusCode::NO_CONTENT)
}
