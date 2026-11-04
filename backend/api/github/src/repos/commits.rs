use axum::extract::MatchedPath;
use axum::extract::OriginalUri;
use axum::extract::Path;
use axum::extract::Query;
use axum::extract::State;
use axum::http::header;
use axum::http::HeaderMap;
use axum::response::IntoResponse;
use axum::response::Response;
use axum::routing::get;
use axum::routing::MethodRouter;
use axum::Json;
use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use codeyard_api_http::shared::github::pagination::Pagination;
use codeyard_app::api_shared::github::repository;
use codeyard_app::app::AppState;
use codeyard_git::CommitFilter;
use codeyard_git::CommitInfo;
use codeyard_git::DiffFileInfo;
use codeyard_git::DiffInfo;
use codeyard_model::github_error::Error;
use codeyard_model::github_error::Result;
use codeyard_model::urls::repo_url;
use codeyard_repos_core::api_shared::github::contents::store_error;
use codeyard_repos_core::repositories::store::repo_store;
use codeyard_repos_core::repositories::store::RepoStore;
use codeyard_webhooks::payloads::git_commit as commit_json;
use codeyard_webhooks::payloads::Person;
use codeyard_webui::pages::repos::ref_path::encode_path as path_url;
use serde::Deserialize;
use serde_json::json;
use serde_json::Value;
use std::collections::HashMap;
use std::sync::Arc;

/// The Git commit document, with its signature checked against the signer's keys.
pub(crate) async fn git_document(
    state: &AppState,
    signers: &mut codeyard_identity::signatures::Signers,
    commit: &CommitInfo,
    owner: &str,
    name: &str,
    repo: i64,
) -> Result<Value> {
    let mut value = commit_json(commit, owner, name, repo);
    value["verification"] = codeyard_identity::signatures::verification(
        &state.pool,
        signers,
        codeyard_identity::signatures::Signed {
            signature: commit.signature.as_deref(),
            payload: commit.signature_payload.as_deref(),
            email: Some(&commit.committer_email),
        },
    )
    .await
    .map_err(Error::unavailable)?;
    Ok(value)
}

pub(crate) async fn documents(
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
        codeyard_db_repos::repositories::commit_comments::comment_counts(&state.pool, repo, &shas)
            .await
            .map_err(Error::unavailable)?
            .into_iter()
            .collect();
    let emails = commits
        .iter()
        .flat_map(|commit| [&commit.author_email, &commit.committer_email])
        .cloned()
        .collect::<Vec<_>>();
    let users = codeyard_repos_core::repositories::reads::commit_authors(&state.pool, &emails)
        .await
        .map_err(Error::unavailable)?;
    let people: HashMap<String, Person> = users
        .into_iter()
        .map(|(email, user)| (email, Person::of(&user)))
        .collect();
    let mut values = Vec::with_capacity(commits.len());
    let mut signers = codeyard_identity::signatures::Signers::default();
    for commit in commits {
        values.push(codeyard_webhooks::payloads::commit(
            commit,
            git_document(state, &mut signers, commit, owner, name, repo).await?,
            owner,
            name,
            repo,
            counts.get(&commit.sha).copied().unwrap_or(0),
            people.get(&commit.author_email),
            people.get(&commit.committer_email),
        ));
    }
    Ok(values)
}

pub(crate) async fn read_document(
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

pub(super) fn file_status(status: &str) -> &str {
    if status == "type_changed" {
        "changed"
    } else {
        status
    }
}

/// One page of a comparison's files with their patches; the totals cover every file.
pub(crate) struct FilePage {
    pub additions: i32,
    pub deletions: i32,
    pub files: Vec<DiffFileInfo>,
    pub has_next: bool,
}

/// GitHub lists at most this many files of one comparison.
const MAX_LISTED_FILES: usize = 3000;

fn file_page_of(diff: DiffInfo, offset: usize, limit: usize) -> FilePage {
    FilePage {
        additions: diff.additions,
        deletions: diff.deletions,
        has_next: diff.files.len().min(MAX_LISTED_FILES) > offset.saturating_add(limit),
        files: diff
            .files
            .into_iter()
            .take(MAX_LISTED_FILES)
            .skip(offset)
            .take(limit)
            .collect(),
    }
}

/// The page of files `base_ref...head_ref` changes. When the patches together are too large to
/// read, only the page's patches are read, file by file, and a patch that is itself too large is
/// left out, as GitHub leaves out patches it cannot show.
pub(crate) async fn file_page(
    store: &RepoStore,
    base_ref: &str,
    head_ref: &str,
    offset: usize,
    limit: usize,
) -> Result<FilePage> {
    match store.diff(base_ref, head_ref).await {
        Ok(diff) => Ok(file_page_of(diff, offset, limit)),
        Err(error) if error.is_response_too_large() => {
            let stats = store
                .diff_stats(base_ref, head_ref)
                .await
                .map_err(store_error)?;
            let mut page = file_page_of(stats, offset, limit);
            for file in &mut page.files {
                match store.diff_file(base_ref, head_ref, &file.filename).await {
                    Ok(one) => {
                        if let Some(found) = one
                            .files
                            .into_iter()
                            .find(|found| found.filename == file.filename)
                        {
                            file.patch = found.patch;
                        }
                    }
                    Err(error) if error.is_response_too_large() => {}
                    Err(error) => return Err(store_error(error)),
                }
            }
            Ok(page)
        }
        Err(error) => Err(store_error(error)),
    }
}

pub(crate) fn diff_files(
    files: Vec<DiffFileInfo>,
    owner: &str,
    name: &str,
    sha: &str,
    base: Option<(&str, &str, &str)>,
) -> Vec<Value> {
    files.into_iter().map(|file| {
        let (owner, name, sha) = if file.status == "removed" { base.unwrap_or((owner, name, sha)) } else { (owner, name, sha) };
        let mut value = json!({
            "sha": file.blob_sha, "filename": file.filename, "status": file_status(&file.status),
            "additions": file.additions, "deletions": file.deletions, "changes": file.additions + file.deletions,
            "blob_url": format!("{}/{owner}/{name}/blob/{sha}/{}", codeyard_model::http_config::link_origin(), path_url(&file.filename)),
            "raw_url": format!("{}/{owner}/{name}/raw/{sha}/{}", codeyard_model::http_config::link_origin(), path_url(&file.filename)),
            "contents_url": format!("{}/contents/{}?ref={sha}", repo_url(owner, name), path_url(&file.filename)),
        });
        if !file.patch.is_empty() {
            value["patch"] = json!(file.patch);
        }
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
        return Ok(Json(
            git_document(
                &state,
                &mut Default::default(),
                &commit,
                owner,
                name,
                repo.id,
            )
            .await?,
        )
        .into_response());
    }
    if let Some(media) = crate::diff_media::requested(&headers) {
        let diff = store.commit_diff(&commit.sha).await.map_err(store_error)?;
        return Ok(crate::diff_media::response(media, &commit, &diff.files));
    }
    let mut value = read_document(&state, repo.id, &commit, owner, name).await?;
    let (offset, limit) = page.window()?;
    // The first parent is where `commit_diff` compares from, and a three-dot comparison with it
    // is the same diff.
    let files = match commit.parents.first() {
        Some(parent) => file_page(&store, parent, &commit.sha, offset, limit).await?,
        None => file_page_of(
            store.commit_diff(&commit.sha).await.map_err(store_error)?,
            offset,
            limit,
        ),
    };
    value["stats"] = json!({"additions":files.additions,"deletions":files.deletions,"total":files.additions + files.deletions});
    value["files"] = json!(diff_files(
        files.files,
        owner,
        name,
        &commit.sha,
        commit
            .parents
            .first()
            .map(|sha| (owner.as_str(), name.as_str(), sha.as_str()))
    ));
    Ok((page.headers(&uri, files.has_next)?, Json(value)).into_response())
}

#[derive(Deserialize)]
struct List {
    page: Option<usize>,
    per_page: Option<usize>,
    sha: Option<String>,
    path: Option<String>,
    author: Option<String>,
    committer: Option<String>,
    since: Option<chrono::DateTime<chrono::Utc>>,
    until: Option<chrono::DateTime<chrono::Utc>>,
}

async fn identity_emails(
    pool: &codeyard_db_core::DbPool,
    value: Option<&str>,
) -> Result<Option<Vec<String>>> {
    match value.map(str::trim) {
        None | Some("") => Ok(Some(Vec::new())),
        Some(address) if address.contains('@') => Ok(Some(vec![address.to_owned()])),
        Some(username) => {
            let Some(user) =
                codeyard_db_identity::users::accounts::get_user_by_username(pool, username)
                    .await
                    .map_err(Error::unavailable)?
            else {
                return Ok(None);
            };
            let mut emails = codeyard_db_identity::users::emails::list_emails(pool, user.id)
                .await
                .map_err(Error::unavailable)?
                .into_iter()
                .filter(|address| address.verified)
                .map(|address| address.email)
                .collect::<Vec<_>>();
            emails.extend(user.email.clone().filter(|_| user.email_verified));
            emails.push(user.noreply_email());
            Ok(Some(emails))
        }
    }
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
    let Some(author_emails) = identity_emails(&state.pool, query.author.as_deref()).await? else {
        return page.response(&uri, Vec::new(), false);
    };
    let Some(committer_emails) = identity_emails(&state.pool, query.committer.as_deref()).await?
    else {
        return page.response(&uri, Vec::new(), false);
    };
    let store = repo_store(&repo, state.cluster.as_ref());
    let filter = CommitFilter {
        since: query.since.map(|time| time.timestamp()),
        until: query.until.map(|time| time.timestamp()),
        author_emails,
        committer_emails,
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
    if !codeyard_git::is_full_commit_id(&sha) {
        return Err(Error::missing());
    }
    let bytes = repo_store(&repo, state.cluster.as_ref())
        .blob_by_id(&sha)
        .await
        .map_err(store_error)?;
    if headers
        .get(header::ACCEPT)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.contains("application/vnd.github.raw"))
    {
        return Ok(([(header::CONTENT_TYPE, "application/octet-stream")], bytes).into_response());
    }
    Ok(Json(
        json!({"sha":sha.to_ascii_lowercase(),"node_id":codeyard_model::node_id::git_object("B", repo.id, &sha.to_ascii_lowercase()),"url":format!("{}/git/blobs/{sha}", repo_url(&owner, &name)),"size":bytes.len(),"encoding":"base64","content":STANDARD.encode(bytes)}),
    ).into_response())
}

pub(crate) fn handlers() -> Vec<(&'static str, MethodRouter<Arc<AppState>>)> {
    vec![
        ("repos/list-commits", get(list)),
        ("repos/get-commit", get(read)),
        ("git/get-commit", get(read)),
        ("git/get-blob", get(blob)),
    ]
}
