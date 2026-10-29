use super::{
    contents::{commit_json, path_url, store_error},
    pagination::Pagination,
    repo_url, repository, Error, Result,
};
use crate::backend::{
    app::AppState,
    db,
    repositories::store::{repo_store, CommitFilter, CommitInfo, DiffFileInfo},
    webhooks::payloads::{self, Person},
};
use axum::{
    extract::{MatchedPath, OriginalUri, Path, Query, State},
    http::{header, HeaderMap},
    response::{IntoResponse, Response},
    routing::{get, MethodRouter},
    Json,
};
use base64::{engine::general_purpose::STANDARD, Engine};
use serde::Deserialize;
use serde_json::{json, Value};
use std::{collections::HashMap, sync::Arc};

/// The Git commit document, with its signature checked against the signer's keys.
pub(super) async fn git_document(
    state: &AppState,
    commit: &CommitInfo,
    owner: &str,
    name: &str,
) -> Result<Value> {
    let mut value = commit_json(commit, owner, name);
    value["verification"] = crate::backend::identity::signatures::verification(
        &state.pool,
        crate::backend::identity::signatures::Signed {
            signature: commit.signature.as_deref(),
            payload: commit.signature_payload.as_deref(),
            email: Some(&commit.committer_email),
        },
    )
    .await
    .map_err(Error::unavailable)?;
    Ok(value)
}

fn document(
    commit: &CommitInfo,
    mut detail: Value,
    owner: &str,
    name: &str,
    count: i64,
    people: &HashMap<String, Person>,
) -> Value {
    detail["parents"].take();
    let parents: Vec<_> = commit.parents.iter().map(|sha| json!({"sha": sha, "url": format!("{}/commits/{sha}", repo_url(owner, name)), "html_url": format!("{}/{owner}/{name}/commit/{sha}", crate::backend::api::http::config::link_origin())})).collect();
    detail["comment_count"] = json!(count);
    json!({
        "sha": commit.sha, "node_id": super::node_id::git("C", &commit.sha),
        "url": format!("{}/commits/{}", repo_url(owner, name), commit.sha),
        "html_url": detail["html_url"],
        "comments_url": format!("{}/commits/{}/comments", repo_url(owner, name), commit.sha),
        "commit": detail, "author": people.get(&commit.author_email).map(payloads::person),
        "committer": people.get(&commit.committer_email).map(payloads::person), "parents": parents,
    })
}

pub(super) async fn documents(
    state: &AppState,
    repo: i64,
    commits: &[CommitInfo],
    owner: &str,
    name: &str,
) -> Result<Vec<Value>> {
    let shas = commits
        .iter()
        .map(|commit| commit.sha.clone())
        .collect::<Vec<_>>();
    let counts: HashMap<_, _> =
        db::repositories::commit_comments::comment_counts(&state.pool, repo, &shas)
            .await
            .map_err(Error::unavailable)?
            .into_iter()
            .collect();
    let emails = commits
        .iter()
        .flat_map(|commit| [&commit.author_email, &commit.committer_email])
        .cloned()
        .collect::<Vec<_>>();
    let users = crate::backend::repositories::reads::commit_authors(&state.pool, &emails)
        .await
        .map_err(Error::unavailable)?;
    let people = users
        .into_iter()
        .map(|(email, user)| (email, Person::of(&user)))
        .collect();
    let mut values = Vec::with_capacity(commits.len());
    for commit in commits {
        values.push(document(
            commit,
            git_document(state, commit, owner, name).await?,
            owner,
            name,
            counts.get(&commit.sha).copied().unwrap_or(0),
            &people,
        ));
    }
    Ok(values)
}

pub(super) async fn read_document(
    state: &AppState,
    repo: i64,
    commit: &CommitInfo,
    owner: &str,
    name: &str,
) -> Result<Value> {
    documents(state, repo, std::slice::from_ref(commit), owner, name)
        .await?
        .pop()
        .ok_or_else(|| Error::unavailable(()))
}

pub(super) fn diff_files(
    files: Vec<DiffFileInfo>,
    owner: &str,
    name: &str,
    sha: &str,
    base: Option<(&str, &str, &str)>,
) -> Vec<Value> {
    files.into_iter().map(|file| {
        let (owner, name, sha) = if file.status == "removed" { base.unwrap_or((owner, name, sha)) } else { (owner, name, sha) };
        let mut value = json!({
            "sha": file.blob_sha, "filename": file.filename, "status": file.status,
            "additions": file.additions, "deletions": file.deletions, "changes": file.additions + file.deletions,
            "patch": file.patch,
            "blob_url": format!("{}/{owner}/{name}/blob/{sha}/{}", crate::backend::api::http::config::link_origin(), path_url(&file.filename)),
            "raw_url": format!("{}/{owner}/{name}/raw/{sha}/{}", crate::backend::api::http::config::link_origin(), path_url(&file.filename)),
            "contents_url": format!("{}/contents/{}?ref={sha}", repo_url(owner, name), path_url(&file.filename)),
        });
        if let Some(previous) = file.previous_filename {
            value["previous_filename"] = json!(previous);
        }
        value
    }).collect()
}

async fn read(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, name, reference)): Path<(String, String, String)>,
    route: MatchedPath,
    Query(page): Query<Pagination>,
    OriginalUri(uri): OriginalUri,
) -> Result<Response> {
    let (owner, name, reference) = (&owner, &name, &reference);
    let (repo, _) = repository(&state, &headers, owner, name, false).await?;
    let store = repo_store(&repo, state.cluster.as_ref());
    let commit = store.commit(reference).await.map_err(store_error)?;
    if route.as_str().contains("/git/commits/") {
        return Ok(Json(git_document(&state, &commit, owner, name).await?).into_response());
    }
    let diff = store.commit_diff(&commit.sha).await.map_err(store_error)?;
    if let Some(media) = super::diff_media::requested(&headers) {
        return Ok(super::diff_media::response(media, &commit, &diff.files));
    }
    let mut value = read_document(&state, repo.id, &commit, owner, name).await?;
    value["stats"] = json!({"additions":diff.additions,"deletions":diff.deletions,"total":diff.additions + diff.deletions});
    let (offset, limit) = page.window()?;
    let has_next = diff.files.len() > offset + limit;
    let files = diff.files.into_iter().skip(offset).take(limit).collect();
    value["files"] = json!(diff_files(files, owner, name, &commit.sha, commit.parents.first().map(|sha| (owner.as_str(), name.as_str(), sha.as_str()))));
    Ok((page.headers(&uri, has_next)?, Json(value)).into_response())
}

#[derive(Deserialize)]
struct List {
    page: Option<usize>,
    per_page: Option<usize>,
    sha: Option<String>,
    path: Option<String>,
    author: Option<String>,
    since: Option<chrono::DateTime<chrono::Utc>>,
    until: Option<chrono::DateTime<chrono::Utc>>,
}

async fn list(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, name)): Path<(String, String)>,
    Query(query): Query<List>,
    OriginalUri(uri): OriginalUri,
) -> Result<Response> {
    let (repo, _) = repository(&state, &headers, &owner, &name, false).await?;
    let page = Pagination {
        page: query.page,
        per_page: query.per_page,
    };
    let (offset, limit) = page.window()?;
    // GitHub matches an address exactly, and a username through the account's addresses.
    let author_emails = match query.author.as_deref().map(str::trim) {
        None | Some("") => Vec::new(),
        Some(address) if address.contains('@') => vec![address.to_owned()],
        Some(username) => {
            let Some(user) = db::users::get_user_by_username(&state.pool, username)
                .await
                .map_err(Error::unavailable)?
            else {
                return page.response(&uri, Vec::new(), false);
            };
            let mut emails = db::users::emails::list_emails(&state.pool, user.id)
                .await
                .map_err(Error::unavailable)?
                .into_iter()
                .filter(|address| address.verified)
                .map(|address| address.email)
                .collect::<Vec<_>>();
            emails.extend(user.email.clone().filter(|_| user.email_verified));
            emails.push(user.noreply_email());
            emails
        }
    };
    let store = repo_store(&repo, state.cluster.as_ref());
    let filter = CommitFilter {
        since: query.since.map(|time| time.timestamp()),
        until: query.until.map(|time| time.timestamp()),
        author_emails,
        ..CommitFilter::default()
    };
    let reference = query.sha.as_deref().unwrap_or(&repo.default_branch);
    let revision = store.commit(reference).await.map_err(store_error)?.sha;
    let fetch = |offset, limit| {
        let store = &store;
        let revision = &revision;
        let filter = &filter;
        let path = query.path.as_deref();
        async move {
            match path {
                Some(path) => {
                    store
                        .path_commits(revision, path, offset, limit, filter)
                        .await
                }
                None => store.commits(revision, offset, limit, filter).await,
            }
            .map_err(store_error)
        }
    };
    let commits = fetch(offset, limit).await?;
    let has_next = commits.len() == limit && !fetch(offset + limit, 1).await?.is_empty();
    page.response(
        &uri,
        documents(&state, repo.id, &commits, &owner, &name).await?,
        has_next,
    )
}

async fn blob(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, name, sha)): Path<(String, String, String)>,
) -> Result<Response> {
    let (repo, _) = repository(&state, &headers, &owner, &name, false).await?;
    if !crate::backend::git::is_full_commit_id(&sha) {
        return Err(Error::missing());
    }
    let bytes = repo_store(&repo, state.cluster.as_ref())
        .blob_by_id(&sha)
        .await
        .map_err(store_error)?;
    if headers.get(header::ACCEPT).and_then(|value| value.to_str().ok()).is_some_and(|value| value.contains("application/vnd.github.raw")) {
        return Ok(([(header::CONTENT_TYPE, "application/octet-stream")], bytes).into_response());
    }
    Ok(Json(
        json!({"sha":sha.to_ascii_lowercase(),"node_id":super::node_id::git("B", &sha.to_ascii_lowercase()),"url":format!("{}/git/blobs/{sha}", repo_url(&owner, &name)),"size":bytes.len(),"encoding":"base64","content":STANDARD.encode(bytes)}),
    ).into_response())
}

pub(super) fn handlers() -> Vec<(&'static str, MethodRouter<Arc<AppState>>)> {
    vec![
        ("repos/list-commits", get(list)),
        ("repos/get-commit", get(read)),
        ("git/get-commit", get(read)),
        ("git/get-blob", get(blob)),
    ]
}
