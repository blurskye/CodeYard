//! Pull requests use the published commits, including a fork's retained pull ref.
mod async_merge;
mod comments;
mod merge;
mod mutations;
pub(super) use mutations::operation_error;
mod reviews;
mod stacks;
mod team_reviewers;
use super::{
    contents::store_error,
    pagination::Pagination,
    repo_url, repository, Error, Result,
};
use crate::backend::{
    app::AppState,
    db,
    repositories::{access, store::repo_store},
    webhooks::payloads,
};
use axum::{
    extract::{OriginalUri, Path, Query, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, MethodRouter},
    Json,
};
use serde::Deserialize;
use serde_json::{json, Value};
use std::sync::Arc;

async fn target(
    state: &AppState,
    headers: &HeaderMap,
    owner: &str,
    name: &str,
    number: i32,
) -> Result<(db::models::Repository, db::models::PullRequest)> {
    let (repo, _) = repository(state, headers, owner, name, false).await?;
    let pull = db::pull_requests::records::get_pull_request(&state.pool, repo.id, number)
        .await
        .map_err(Error::unavailable)?
        .ok_or_else(Error::missing)?;
    Ok((repo, pull))
}

async fn person(state: &AppState, id: i64) -> Result<Value> {
    let user = db::users::accounts::get_user_by_id(&state.pool, id)
        .await
        .map_err(Error::unavailable)?;
    Ok(user.as_ref().map_or(Value::Null, payloads::user))
}

pub(super) async fn association(
    pool: &db::DbPool,
    repo: &db::models::Repository,
    user_id: i64,
) -> Result<&'static str> {
    access::author_association(pool, repo, user_id)
        .await
        .map_err(Error::unavailable)
}

pub(super) async fn readable_head(
    state: &AppState,
    auth: Option<&super::auth::Authentication>,
    pull: &db::models::PullRequest,
) -> Result<Option<db::models::Repository>> {
    match pull.head_repo_id {
        Some(id) => super::auth::referenced_repository(state, auth, id).await,
        None => Ok(None),
    }
}

/// The head of a pull request whose head repository the viewer cannot read: GitHub still
/// names the head owner, but not the repository.
pub(super) async fn hidden_head(state: &AppState, pull: &db::models::PullRequest) -> Result<Value> {
    let head = match pull.head_repo_id {
        Some(id) => db::repositories::records::get_repository_by_id(&state.pool, id)
            .await
            .map_err(Error::unavailable)?,
        None => None,
    };
    let owner = match &head {
        Some(head) => db::repositories::records::repository_owner_name(&state.pool, head.id)
            .await
            .map_err(Error::unavailable)?
            .map(|name| (payloads::repository_owner(head, &name), format!("{name}:{}", pull.head_branch))),
        None => None,
    };
    let (user, label) = owner.unwrap_or((Value::Null, pull.head_branch.clone()));
    Ok(json!({"label": label, "ref": pull.head_branch, "sha": pull.head_sha, "user": user, "repo": null}))
}

pub(super) async fn document(
    state: &AppState,
    headers: &HeaderMap,
    repo: &db::models::Repository,
    owner: &str,
    pull: &db::models::PullRequest,
    detail: bool,
) -> Result<Value> {
    let viewer = super::auth::authenticate(state, headers, false).await?;
    let author = db::users::accounts::get_user_by_id(&state.pool, pull.author_id)
        .await
        .map_err(Error::unavailable)?
        .ok_or_else(Error::missing)?;
    let mut value = payloads::pull_request_json(pull, &payloads::Person::of(&author), repo, owner);
    value["user"] = payloads::user(&author);
    value["author_association"] = json!(association(&state.pool, repo, author.id).await?);
    let mut base_repo = payloads::repository(repo, owner);
    payloads::set_fork_counts(&mut base_repo, super::repos::public_fork_count(state, repo.id).await?);
    value["base"] = json!({"label": format!("{owner}:{}", pull.base_branch), "ref": pull.base_branch, "sha": pull.base_sha, "repo": base_repo, "user": person(state, repo.owner_id()).await?});
    if repo.owner_kind == "org" {
        value["base"]["user"] = value["base"]["repo"]["owner"].clone();
    }
    // The base shares the retained revision, not authority over the source repository.
    value["head"] = if let Some(head) = readable_head(state, viewer.as_ref(), pull).await? {
        let head_owner = db::repositories::records::repository_owner_name(&state.pool, head.id)
            .await
            .map_err(Error::unavailable)?
            .ok_or_else(Error::missing)?;
        let mut head_value = payloads::repository(&head, &head_owner);
        payloads::set_fork_counts(&mut head_value, super::repos::public_fork_count(state, head.id).await?);
        json!({"label": format!("{head_owner}:{}", pull.head_branch), "ref": pull.head_branch, "sha": pull.head_sha, "user": head_value["owner"], "repo": head_value})
    } else {
        hidden_head(state, pull).await?
    };
    let labels = db::labels::get_pull_request_labels(&state.pool, pull.repo_id, pull.id)
        .await
        .map_err(Error::unavailable)?;
    value["labels"] = json!(labels
        .iter()
        .map(|label| payloads::label(label, owner, &repo.name))
        .collect::<Vec<_>>());
    if let Some(id) = pull.milestone_id {
        let summary = db::milestones::get_milestone_by_id(&state.pool, repo.id, id)
            .await
            .map_err(Error::unavailable)?;
        value["milestone"] = match summary {
            Some(summary) => {
                super::milestones::document(&state.pool, &summary, owner, &repo.name).await?
            }
            None => Value::Null,
        };
    }
    let names = db::assignees::list_assignees(
        &state.pool,
        db::assignees::AssignedItem::PullRequest(pull.id),
    )
    .await
    .map_err(Error::unavailable)?;
    let mut assignees = Vec::new();
    for name in names {
        if let Some(user) = db::users::accounts::get_user_by_username(&state.pool, &name)
            .await
            .map_err(Error::unavailable)?
        {
            assignees.push(person(state, user.id).await?);
        }
    }
    value["assignee"] = assignees.first().cloned().unwrap_or(Value::Null);
    value["assignees"] = json!(assignees);
    let reviewers = db::pull_requests::reviews::list_pull_request_reviewers(&state.pool, pull.id)
        .await
        .map_err(Error::unavailable)?;
    let mut requested = Vec::new();
    for user in reviewers {
        requested.push(person(state, user.id).await?);
    }
    value["requested_reviewers"] = json!(requested);
    value["auto_merge"] = crate::backend::webhooks::documents::auto_merge(&state.pool, pull.id)
        .await
        .map_err(Error::unavailable)?;
    value["requested_teams"] = json!(
        team_reviewers::documents(
            state,
            repo.id,
            pull.id,
            viewer.as_ref().map(|viewer| viewer.user.id)
        )
        .await?
    );
    if detail {
        let store = repo_store(repo, state.cluster.as_ref());
        let diff = store
            .diff_stats(pull.diff_base(), &pull.head_sha)
            .await
            .map_err(store_error)?;
        let (commits, _) = store
            .pull_request_commits(pull.diff_base(), &pull.head_sha, 0, 0)
            .await
            .map_err(store_error)?;
        let (comments, review_comments) =
            db::pull_requests::reviews::pull_request_comment_counts(&state.pool, pull.id)
                .await
                .map_err(Error::unavailable)?;
        value["comments"] = json!(comments);
        value["review_comments"] = json!(review_comments);
        value["commits"] = json!(commits);
        value["additions"] = json!(diff.additions);
        value["deletions"] = json!(diff.deletions);
        value["changed_files"] = json!(diff.changed_files);
        value["mergeable"] = json!(pull.mergeable);
        value["mergeable_state"] = json!(if pull.merged { "clean" } else { "unknown" });
        value["maintainer_can_modify"] = json!(pull.maintainer_can_modify);
        value["merged_by"] = match pull.merged_by_id {
            Some(id) => person(state, id).await?,
            None => Value::Null,
        };
    }
    Ok(value)
}

#[derive(Deserialize)]
struct List {
    page: Option<usize>,
    per_page: Option<usize>,
    state: Option<String>,
    head: Option<String>,
    base: Option<String>,
    sort: Option<String>,
    direction: Option<String>,
}

async fn list(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, name)): Path<(String, String)>,
    Query(query): Query<List>,
    OriginalUri(uri): OriginalUri,
) -> Result<Response> {
    let (repo, viewer) = repository(&state, &headers, &owner, &name, false).await?;
    let page = Pagination {
        page: query.page,
        per_page: query.per_page,
    };
    let (offset, limit) = page.window()?;
    let state_filter = match query.state.as_deref().unwrap_or("open") {
        "all" => None,
        state @ ("open" | "closed") => Some(state),
        _ => return Err(Error::invalid("Invalid state")),
    };
    let sort = query.sort.as_deref().unwrap_or("created");
    if !matches!(sort, "created" | "updated" | "popularity" | "long-running") {
        return Err(Error::invalid("Invalid sort"));
    }
    let ascending = match query.direction.as_deref() {
        Some("asc") => true,
        Some("desc") => false,
        None => sort != "created",
        _ => return Err(Error::invalid("Invalid direction")),
    };
    let (head_owner, head_branch) = match query.head.as_deref() {
        Some(head) => {
            let (owner, branch) = head
                .split_once(':')
                .ok_or_else(|| Error::invalid("head must be owner:branch"))?;
            (Some(owner), Some(branch))
        }
        None => (None, None),
    };
    let filter = db::pull_requests::records::PullRequestFilter {
        viewer_id: viewer.as_ref().map(|actor| actor.user.id),
        state: state_filter,
        base: query.base.as_deref(),
        head_owner,
        head_branch,
        sort,
        ascending,
        month_ago: chrono::Utc::now() - chrono::Duration::days(30),
    };
    let rows = db::pull_requests::records::list_filtered_pull_requests(
        &state.pool,
        repo.id,
        &filter,
        offset as i64,
        (limit + 1) as i64,
    )
    .await
    .map_err(Error::unavailable)?;
    let has_next = rows.len() > limit;
    let mut items = Vec::new();
    for pull in rows.iter().take(limit) {
        items.push(document(&state, &headers, &repo, &owner, pull, false).await?);
    }
    page.response(&uri, items, has_next)
}

async fn get_pull(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, name, number)): Path<(String, String, i32)>,
) -> Result<Response> {
    // A head repository the viewer cannot read is left out of the document; its owner is not.
    let (repo, pull) = target(&state, &headers, &owner, &name, number).await?;
    if let Some(media) = super::diff_media::requested(&headers) {
        return diff_response(&state, &repo, &pull, media).await;
    }
    Ok(Json(document(&state, &headers, &repo, &owner, &pull, true).await?).into_response())
}

/// The pull request's diff or patch as plain text, for `gh pr diff` and `.diff` links.
pub(super) async fn diff_response(
    state: &AppState,
    repo: &db::models::Repository,
    pull: &db::models::PullRequest,
    media: super::diff_media::Media,
) -> Result<Response> {
    let store = repo_store(repo, state.cluster.as_ref());
    if media == super::diff_media::Media::Patch {
        let (total, commits) = store
            .pull_request_commits(pull.diff_base(), &pull.head_sha, 0, 250)
            .await
            .map_err(store_error)?;
        let total = usize::try_from(total).unwrap_or(usize::MAX);
        return super::diff_media::series(&store, &commits, total)
            .await
            .map_err(store_error);
    }
    let diff = store
        .diff(pull.diff_base(), &pull.head_sha)
        .await
        .map_err(store_error)?;
    let head = store.commit(&pull.head_sha).await.map_err(store_error)?;
    Ok(super::diff_media::response(media, &head, &diff.files))
}

async fn merged(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, name, number)): Path<(String, String, i32)>,
) -> Result<Response> {
    let merged = match target(&state, &headers, &owner, &name, number).await {
        Ok((_, pull)) => pull.merged,
        Err(Error(StatusCode::NOT_FOUND, _, _)) => false,
        Err(error) => return Err(error),
    };
    let status = if merged {
        StatusCode::NO_CONTENT
    } else {
        StatusCode::NOT_FOUND
    };
    Ok(super::empty(status))
}

async fn commits(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, name, number)): Path<(String, String, i32)>,
    Query(page): Query<Pagination>,
    OriginalUri(uri): OriginalUri,
) -> Result<Response> {
    let (repo, pull) = target(&state, &headers, &owner, &name, number).await?;
    let (offset, limit) = page.window()?;
    let (total, rows) = repo_store(&repo, state.cluster.as_ref())
        .pull_request_commits(pull.diff_base(), &pull.head_sha, offset, limit)
        .await
        .map_err(store_error)?;
    let has_next = (offset as u64).saturating_add(limit as u64) < total.min(250);
    let items = super::commits::documents(&state, repo.id, &rows, &owner, &name).await?;
    page.response(&uri, items, has_next)
}

async fn files(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, name, number)): Path<(String, String, i32)>,
    Query(page): Query<Pagination>,
    OriginalUri(uri): OriginalUri,
) -> Result<Response> {
    let (repo, pull) = target(&state, &headers, &owner, &name, number).await?;
    let (offset, limit) = page.window()?;
    let store = repo_store(&repo, state.cluster.as_ref());
    let page_files = super::commits::file_page(&store, pull.diff_base(), &pull.head_sha, offset, limit).await?;
    let has_next = page_files.has_next;
    let files = page_files.files;
    let base = if files.iter().any(|file| file.status == "removed") {
        store.comparison_from(&store, pull.diff_base(), &pull.head_sha, 0, 0).await.map_err(store_error)?.merge_base_commit.sha
    } else {
        pull.diff_base().to_owned()
    };
    let items = super::commits::diff_files(files, &owner, &name, &pull.head_sha, Some((&owner, &name, &base)));
    page.response(&uri, items, has_next)
}

pub(super) fn handlers() -> Vec<(&'static str, MethodRouter<Arc<AppState>>)> {
    let mut handlers = vec![
        ("pulls/list", get(list)),
        ("pulls/get", get(get_pull)),
        ("pulls/check-if-merged", get(merged)),
        ("pulls/list-commits", get(commits)),
        ("pulls/list-files", get(files)),
    ];
    handlers.extend(merge::handlers());
    handlers.extend(async_merge::handlers());
    handlers.extend(mutations::handlers());
    handlers.extend(comments::handlers());
    handlers.extend(stacks::handlers());
    handlers.extend(reviews::handlers());
    handlers
}
