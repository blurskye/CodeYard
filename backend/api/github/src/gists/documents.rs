use super::*;
use crate::gists::failure;
use crate::gists::failure_store;
use axum::response::IntoResponse;
use codeyard_app::app::AppState;
use codeyard_app::gists::LoadedGist;
use codeyard_app::repositories::store::repo_store;
use serde_json::json;
use serde_json::Map;
use serde_json::Value;
use sha2::Digest;
use sha2::Sha256;

pub(super) async fn base(state: &AppState, loaded: &LoadedGist, contents: bool) -> Result<Value> {
    let owner =
        codeyard_db_identity::users::accounts::get_user_by_id(&state.pool, loaded.gist.owner_id)
            .await
            .map_err(Error::unavailable)?
            .ok_or_else(Error::missing)?;
    let store = repo_store(&loaded.repository, state.cluster.as_ref());
    let mut files = Map::new();
    for entry in &loaded.snapshot.files {
        let remaining = 1024 * 1024;
        let content = if contents {
            let mut bytes = store
                .blob_range(
                    &loaded.snapshot.sha,
                    &entry.path,
                    codeyard_storage::range::ByteRange::Window {
                        offset: 0,
                        length: (remaining + 3) as u64,
                    },
                )
                .await
                .map_err(failure_store)?
                .bytes;
            if let Err(error) = std::str::from_utf8(&bytes) {
                if error.error_len().is_none() {
                    bytes.truncate(error.valid_up_to());
                }
            }
            Some(bytes)
        } else {
            None
        };
        let inline = content.as_ref().map(|bytes| {
            let mut length = remaining.min(bytes.len());
            if let Some(text) = domain::text_preview(bytes, remaining) {
                length = text.len();
            }
            &bytes[..length]
        });
        files.insert(
            entry.path.clone(),
            codeyard_webhooks::payloads::gists::gist_file(
                &loaded.gist.id,
                &loaded.snapshot.sha,
                entry,
                inline,
            ),
        );
    }
    // The entire file list is present; content truncation belongs to each file.
    Ok(codeyard_webhooks::payloads::gists::gist(
        &loaded.gist,
        &owner,
        Value::Object(files),
    ))
}
pub(super) async fn detail(state: &AppState, loaded: &LoadedGist) -> Result<Value> {
    let mut value = base(state, loaded, true).await?;
    value["history"] = json!(history(state, loaded, 0, 30).await?);
    let forks = codeyard_db_repos::gists::list(
        &state.pool,
        &codeyard_db_repos::gists::GistList {
            scope: codeyard_db_repos::gists::ListScope::Forks(loaded.repository.id),
            since: None,
            limit: 30,
            offset: 0,
        },
    )
    .await
    .map_err(Error::unavailable)?;
    let mut entries = Vec::new();
    for fork in forks {
        let user =
            codeyard_db_identity::users::accounts::get_user_by_id(&state.pool, fork.owner_id)
                .await
                .map_err(Error::unavailable)?
                .ok_or_else(Error::missing)?;
        // GitHub lists each fork's owner as a full public profile.
        let user = crate::users::document(state, &user, None).await?;
        entries.push(json!({"id":fork.id,"url":codeyard_model::urls::api_url(&format!("/gists/{}",fork.id)),"user":user,"created_at":fork.created_at,"updated_at":fork.updated_at}));
    }
    value["forks"] = json!(entries);
    value["fork_of"] = Value::Null;
    if let Some(parent) = loaded.gist.forked_from_id {
        if let Some(parent) = codeyard_db_repos::gists::by_repository(&state.pool, parent)
            .await
            .map_err(Error::unavailable)?
        {
            let parent = codeyard_app::gists::load(state, &parent.id, None)
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
    let emails = revisions
        .iter()
        .map(|revision| revision.author_email.clone())
        .collect::<Vec<_>>();
    let authors = codeyard_app::repositories::reads::commit_authors(&state.pool, &emails)
        .await
        .map_err(Error::unavailable)?;
    let mut entries = Vec::new();
    for revision in revisions {
        entries.push(
            codeyard_webhooks::payloads::gists::gist_revision(
                &loaded.gist.id,
                &revision,
                authors.get(&revision.author_email),
            )
            .map_err(failure_store)?,
        );
    }
    Ok(entries)
}
pub(super) async fn comment(
    state: &AppState,
    gist: &codeyard_db_repos::gists::Gist,
    comment: &codeyard_db_repos::gists::Comment,
) -> Result<Value> {
    let author = match comment.user_id {
        Some(user) => codeyard_db_identity::users::accounts::get_user_by_id(&state.pool, user)
            .await
            .map_err(Error::unavailable)?,
        None => None,
    };
    Ok(codeyard_webhooks::payloads::gists::gist_comment(
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
    if codeyard_api_http::static_files::not_modified(headers, &etag) {
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
