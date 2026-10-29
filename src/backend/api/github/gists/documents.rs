use super::*;
use crate::backend::{gists::LoadedGist, webhooks::payloads};
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};

pub(super) async fn base(state: &AppState, loaded: &LoadedGist, contents: bool) -> Result<Value> {
    let owner = db::users::accounts::get_user_by_id(&state.pool, loaded.gist.owner_id)
        .await
        .map_err(Error::unavailable)?
        .ok_or_else(Error::missing)?;
    let store = repo_store(&loaded.repository, state.cluster.as_ref());
    let mut files = Map::new();
    let mut remaining = 1024 * 1024;
    for entry in &loaded.snapshot.files {
        let content = if contents {
            Some(store.blob_by_id(&entry.sha).await.map_err(failure_store)?)
        } else {
            None
        };
        let inline = content.as_ref().map(|bytes| {
            let mut length = remaining.min(bytes.len());
            if let Some(text) = domain::text_preview(bytes, remaining) {
                length = text.len();
            }
            remaining -= length;
            &bytes[..length]
        });
        files.insert(
            entry.path.clone(),
            payloads::gist_file(&loaded.gist.id, &loaded.snapshot.sha, entry, inline),
        );
    }
    // The entire file list is present; content truncation belongs to each file.
    Ok(payloads::gist(&loaded.gist, &owner, Value::Object(files)))
}
pub(super) async fn detail(state: &AppState, loaded: &LoadedGist) -> Result<Value> {
    let mut value = base(state, loaded, true).await?;
    value["history"] = json!(history(state, loaded, 0, 30).await?);
    let forks = db::gists::list(
        &state.pool,
        &db::gists::GistList {
            scope: db::gists::ListScope::Forks(loaded.repository.id),
            since: None,
            limit: 30,
            offset: 0,
        },
    )
    .await
    .map_err(Error::unavailable)?;
    let mut entries = Vec::new();
    for fork in forks {
        let user = db::users::accounts::get_user_by_id(&state.pool, fork.owner_id)
            .await
            .map_err(Error::unavailable)?
            .ok_or_else(Error::missing)?;
        entries.push(json!({"id":fork.id,"url":super::super::api_url(&format!("/gists/{}",fork.id)),"user":super::super::users::document(state,&user,false).await?,"created_at":fork.created_at,"updated_at":fork.updated_at}));
    }
    value["forks"] = json!(entries);
    value["fork_of"] = Value::Null;
    if let Some(parent) = loaded.gist.forked_from_id {
        if let Some(parent) = db::gists::by_repository(&state.pool, parent)
            .await
            .map_err(Error::unavailable)?
        {
            let parent = crate::backend::gists::load(state, &parent.id, None)
                .await
                .map_err(failure)?;
            value["fork_of"] = base(state, &parent, false).await?;
        }
    }
    Ok(value)
}
pub(super) async fn history(
    state: &AppState,
    loaded: &LoadedGist,
    offset: usize,
    limit: usize,
) -> Result<Vec<Value>> {
    let revisions = repo_store(&loaded.repository, state.cluster.as_ref())
        .gist_history(
            &state.pool,
            loaded.repository.id,
            Some(&loaded.snapshot.sha),
            offset,
            limit,
        )
        .await
        .map_err(failure_store)?;
    let mut entries = Vec::new();
    for revision in revisions {
        let author = db::gists::commit_user(&state.pool, &revision.author_email)
            .await
            .map_err(Error::unavailable)?;
        entries.push(
            payloads::gist_revision(&loaded.gist.id, &revision, author.as_ref())
                .map_err(failure_store)?,
        );
    }
    Ok(entries)
}
pub(super) async fn comment(
    state: &AppState,
    gist: &db::gists::Gist,
    comment: &db::gists::Comment,
) -> Result<Value> {
    let author = match comment.user_id {
        Some(user) => db::users::accounts::get_user_by_id(&state.pool, user)
            .await
            .map_err(Error::unavailable)?,
        None => None,
    };
    Ok(payloads::gist_comment(
        &gist.id,
        gist.owner_id,
        comment,
        author.as_ref(),
    ))
}
pub(super) fn validator(value: &impl serde::Serialize) -> Result<String> {
    let bytes = serde_json::to_vec(value).map_err(Error::unavailable)?;
    Ok(format!("W/\"{}\"", hex::encode(Sha256::digest(bytes))))
}
pub(super) fn conditional(
    headers: &HeaderMap,
    etag: String,
    mut response: Response,
) -> Result<Response> {
    if crate::backend::api::http::static_files::not_modified(headers, &etag) {
        *response.status_mut() = StatusCode::NOT_MODIFIED;
        *response.body_mut() = axum::body::Body::empty();
        response.headers_mut().remove(header::CONTENT_LENGTH);
    }
    response
        .headers_mut()
        .insert(header::ETAG, etag.parse().map_err(Error::unavailable)?);
    response.headers_mut().insert(
        header::CACHE_CONTROL,
        axum::http::HeaderValue::from_static("private, max-age=0, must-revalidate"),
    );
    Ok(response)
}
pub(super) fn response(headers: &HeaderMap, value: Value) -> Result<Response> {
    let etag = validator(&value)?;
    conditional(headers, etag, Json(value).into_response())
}
