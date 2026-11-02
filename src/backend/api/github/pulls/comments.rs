use super::super::{auth, pagination::Pagination, repository, Error, Result};
use super::{association, target};
use crate::backend::{
    app::AppState,
    db,
    issues::comments::CommentTarget,
    repositories::{
        pull_requests::{self, comments as domain},
        store::{repo_store, DiffFileInfo},
    },
    webhooks::payloads,
};
use axum::{
    body::Bytes,
    extract::{OriginalUri, Path, Query, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{delete, get, patch, post, MethodRouter},
    Json,
};
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::Arc;

fn operation_error(error: pull_requests::OperationError) -> Error {
    super::mutations::operation_error(error)
}

pub(super) async fn visible(
    state: &AppState,
    headers: &HeaderMap,
    repo: &db::models::Repository,
    id: i64,
) -> Result<(db::models::PullRequest, db::models::PullRequestComment)> {
    let comment = db::pull_requests::reviews::get_pull_request_comment_by_id(&state.pool, id)
        .await
        .map_err(Error::unavailable)?
        .filter(|c| c.path.is_some())
        .ok_or_else(Error::missing)?;
    let pull =
        db::pull_requests::records::get_pull_request_by_id(&state.pool, comment.pull_request_id)
            .await
            .map_err(Error::unavailable)?
            .filter(|p| p.repo_id == repo.id)
            .ok_or_else(Error::missing)?;
    if let Some(review) = comment.review_id {
        let review = db::pull_requests::reviews::get_pull_request_review_by_id(&state.pool, review)
            .await
            .map_err(Error::unavailable)?
            .ok_or_else(Error::missing)?;
        if review.state == "pending"
            && !auth::authenticate(state, headers, false)
                .await?
                .is_some_and(|a| a.user.id == review.author_id)
        {
            return Err(Error::missing());
        }
    }
    Ok((pull, comment))
}

pub(super) async fn document(
    state: &AppState,
    owner: &str,
    repo: &db::models::Repository,
    pull: &db::models::PullRequest,
    comment: &db::models::PullRequestComment,
) -> Result<Value> {
    document_cached(state, owner, repo, pull, comment, &mut HashMap::new()).await
}

/// Diffs a comment list keeps in memory at once, however many revision pairs its page names.
const MAX_CACHED_DIFFS: usize = 8;

/// Comments are anchored to pairs of revisions. A list reuses each pair's diff while it is held.
async fn document_cached(
    state: &AppState,
    owner: &str,
    repo: &db::models::Repository,
    pull: &db::models::PullRequest,
    comment: &db::models::PullRequestComment,
    diffs: &mut HashMap<(String, String), Vec<DiffFileInfo>>,
) -> Result<Value> {
    let user = db::users::accounts::get_user_by_id(&state.pool, comment.author_id)
        .await
        .map_err(Error::unavailable)?
        .ok_or_else(Error::missing)?;
    let mut value = payloads::pull_request_review_comment_json(
        comment,
        pull,
        &payloads::Person::of(&user),
        repo,
        owner,
    );
    value["user"] = payloads::user(&user);
    value["author_association"] = json!(association(&state.pool, repo, user.id).await?);
    let key = (
        pull.diff_base().to_owned(),
        comment
            .commit_sha
            .as_deref()
            .unwrap_or(&pull.head_sha)
            .to_owned(),
    );
    if !diffs.contains_key(&key) {
        if diffs.len() >= MAX_CACHED_DIFFS {
            diffs.clear();
        }
        let diff = repo_store(repo, state.cluster.as_ref())
            .diff(&key.0, &key.1)
            .await
            .map_err(super::store_error)?;
        diffs.insert(key.clone(), diff.files);
    }
    if let Some(file) = diffs[&key]
        .iter()
        .find(|f| Some(f.filename.as_str()) == comment.path.as_deref())
    {
        value["diff_hunk"] = json!(file.patch);
        if let Some(line) = comment.line {
            if let Some((position, _, _)) =
                positions(&file.patch).into_iter().find(|(_, left, right)| {
                    if comment.side.as_deref() == Some("LEFT") {
                        *left == Some(line)
                    } else {
                        *right == Some(line)
                    }
                })
            {
                value["position"] = json!(position);
                value["original_position"] = json!(position);
            }
        }
    }
    Ok(value)
}

// Positions count diff rows, not file lines; later hunk headers count too.
fn positions(patch: &str) -> Vec<(i32, Option<i32>, Option<i32>)> {
    let (mut old, mut new, mut pos, mut begun) = (0, 0, 0, false);
    let mut rows = Vec::new();
    for row in patch.lines() {
        if row.starts_with("@@ ") {
            let mut fields = row.split_whitespace();
            fields.next();
            old = fields
                .next()
                .and_then(|s| s.trim_start_matches('-').split(',').next()?.parse().ok())
                .unwrap_or(0);
            new = fields
                .next()
                .and_then(|s| s.trim_start_matches('+').split(',').next()?.parse().ok())
                .unwrap_or(0);
            if begun {
                pos += 1;
            }
            begun = true;
            continue;
        }
        if !begun {
            continue;
        }
        pos += 1;
        match row.as_bytes().first() {
            Some(b'+') => {
                rows.push((pos, None, Some(new)));
                new += 1;
            }
            Some(b'-') => {
                rows.push((pos, Some(old), None));
                old += 1;
            }
            Some(b' ') => {
                rows.push((pos, Some(old), Some(new)));
                old += 1;
                new += 1;
            }
            _ => {}
        }
    }
    rows
}

#[derive(Default, Deserialize)]
pub(super) struct Create {
    pub body: String,
    pub path: Option<String>,
    pub commit_id: Option<String>,
    pub line: Option<i32>,
    pub side: Option<String>,
    pub position: Option<i32>,
    pub start_line: Option<i32>,
    pub start_side: Option<String>,
    pub subject_type: Option<String>,
    pub in_reply_to: Option<i64>,
}

pub(super) async fn prepare(
    state: &AppState,
    headers: &HeaderMap,
    repo: &db::models::Repository,
    pull: &db::models::PullRequest,
    input: Create,
    pending: bool,
) -> Result<domain::CreatePullRequestCommentRequest> {
    let mut line = input.line;
    let mut side = input.side;
    if let Some(parent) = input.in_reply_to {
        let (parent_pull, parent) = visible(state, headers, repo, parent).await?;
        if parent_pull.id != pull.id || parent.in_reply_to_comment_id.is_some() {
            return Err(Error::invalid(
                "Reply must name a top-level comment on this pull request",
            ));
        }
        return Ok(domain::CreatePullRequestCommentRequest {
            body: input.body,
            repository_id: Some(repo.id),
            path: None,
            line: None,
            side: None,
            start_line: None,
            start_side: None,
            commit_sha: None,
            base_sha: None,
            in_reply_to_comment_id: input.in_reply_to,
            pending,
            review_id: None,
        });
    } else {
        if input
            .subject_type
            .as_deref()
            .is_some_and(|s| !matches!(s, "line" | "file"))
        {
            return Err(Error::invalid("Invalid subject_type"));
        }
        let path = input
            .path
            .as_deref()
            .ok_or_else(|| Error::invalid("path is required"))?;
        if input.commit_id.is_none() {
            return Err(Error::invalid("commit_id is required"));
        }
        if let Some(position) = input.position.filter(|_| line.is_none()) {
            let diff = repo_store(repo, state.cluster.as_ref())
                .diff(pull.diff_base(), &pull.head_sha)
                .await
                .map_err(super::store_error)?;
            let row = diff
                .files
                .iter()
                .find(|f| f.filename == path)
                .and_then(|f| positions(&f.patch).into_iter().find(|r| r.0 == position))
                .ok_or_else(|| Error::invalid("position is not in the diff"))?;
            if let Some(right) = row.2 {
                line = Some(right);
                side = Some("RIGHT".into());
            } else {
                line = row.1;
                side = Some("LEFT".into());
            }
        }
        if input.subject_type.as_deref() != Some("file") && line.is_none() {
            return Err(Error::invalid("line is required"));
        }
        if input.subject_type.as_deref() == Some("file") {
            line = None;
            side = None;
        } else if line.is_some() && side.is_none() {
            side = Some("RIGHT".into());
        }
    }
    Ok(domain::CreatePullRequestCommentRequest {
        body: input.body,
        repository_id: Some(repo.id),
        path: input.path,
        line,
        side,
        start_line: input.start_line,
        start_side: input.start_side,
        commit_sha: input.commit_id,
        base_sha: Some(pull.base_sha.clone()),
        in_reply_to_comment_id: input.in_reply_to,
        pending,
        review_id: None,
    })
}

async fn create(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, name, number)): Path<(String, String, i32)>,
    body: Bytes,
) -> Result<Response> {
    let (repo, viewer) = repository(&state, &headers, &owner, &name, true).await?;
    let actor = viewer.ok_or_else(auth::required)?.user;
    let (_, pull) = target(&state, &headers, &owner, &name, number).await?;
    let input = serde_json::from_slice(&body).map_err(|_| Error::invalid("Invalid comment"))?;
    let request = prepare(&state, &headers, &repo, &pull, input, false).await?;
    let comment = domain::create_pull_request_comment(
        &state.pool,
        state.cluster.as_ref(),
        &state.notification_broadcaster,
        actor.id,
        owner.clone(),
        name,
        number,
        request,
    )
    .await
    .map_err(operation_error)?;
    Ok((
        StatusCode::CREATED,
        Json(document(&state, &owner, &repo, &pull, &comment).await?),
    )
        .into_response())
}

#[derive(Deserialize)]
struct Body {
    body: String,
}
async fn reply(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, name, number, id)): Path<(String, String, i32, i64)>,
    body: Bytes,
) -> Result<Response> {
    let input: Body =
        serde_json::from_slice(&body).map_err(|_| Error::invalid("body is required"))?;
    create(
        State(state),
        headers,
        Path((owner, name, number)),
        Bytes::from(json!({"body":input.body,"in_reply_to":id}).to_string()),
    )
    .await
}

async fn read(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, name, id)): Path<(String, String, i64)>,
) -> Result<Json<Value>> {
    let (repo, _) = repository(&state, &headers, &owner, &name, false).await?;
    let (pull, comment) = visible(&state, &headers, &repo, id).await?;
    Ok(Json(
        document(&state, &owner, &repo, &pull, &comment).await?,
    ))
}
async fn update(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, name, id)): Path<(String, String, i64)>,
    body: Bytes,
) -> Result<Json<Value>> {
    let (repo, viewer) = repository(&state, &headers, &owner, &name, true).await?;
    let actor = viewer.ok_or_else(auth::required)?.user;
    let (pull, _) = visible(&state, &headers, &repo, id).await?;
    let input: Body =
        serde_json::from_slice(&body).map_err(|_| Error::invalid("body is required"))?;
    let comment = domain::edit_pull_request_comment(
        &state.pool,
        &state.notification_broadcaster,
        actor.id,
        CommentTarget {
            owner: owner.clone(),
            repo: name,
            number: pull.number,
            comment_id: id,
        },
        input.body,
        None,
    )
    .await
    .map_err(operation_error)?;
    Ok(Json(
        document(&state, &owner, &repo, &pull, &comment).await?,
    ))
}
async fn remove(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, name, id)): Path<(String, String, i64)>,
) -> Result<StatusCode> {
    let (repo, viewer) = repository(&state, &headers, &owner, &name, true).await?;
    let actor = viewer.ok_or_else(auth::required)?.user;
    let (pull, _) = visible(&state, &headers, &repo, id).await?;
    domain::delete_pull_request_comment(
        &state.pool,
        actor.id,
        CommentTarget {
            owner,
            repo: name,
            number: pull.number,
            comment_id: id,
        },
    )
    .await
    .map_err(operation_error)?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Default, Deserialize)]
struct List {
    page: Option<usize>,
    per_page: Option<usize>,
    sort: Option<String>,
    direction: Option<String>,
    since: Option<chrono::DateTime<chrono::Utc>>,
}
async fn collection(
    state: Arc<AppState>,
    headers: HeaderMap,
    namespace: (String, String),
    pull_id: Option<i64>,
    review_id: Option<i64>,
    query: List,
    uri: axum::http::Uri,
) -> Result<Response> {
    let (owner, name) = namespace;
    let (repo, viewer) = repository(&state, &headers, &owner, &name, false).await?;
    let page = Pagination {
        page: query.page,
        per_page: query.per_page,
    };
    let (offset, limit) = page.window()?;
    let updated = match query.sort.as_deref().unwrap_or("created") {
        "created" => false,
        "updated" => true,
        _ => return Err(Error::invalid("Invalid sort")),
    };
    let ascending = match query.direction.as_deref().unwrap_or("asc") {
        "asc" => true,
        "desc" => false,
        _ => return Err(Error::invalid("Invalid direction")),
    };
    let rows = db::pull_requests::reviews::list_visible_review_comments(
        &state.pool,
        &db::pull_requests::reviews::ReviewCommentFilter {
            repo_id: repo.id,
            pull_id,
            review_id,
            viewer: viewer.as_ref().map(|a| a.user.id),
            updated,
            ascending,
            since: query.since,
            limit: (limit + 1) as i64,
            offset: offset as i64,
        },
    )
    .await
    .map_err(Error::unavailable)?;
    let has_next = rows.len() > limit;
    let mut items = Vec::new();
    let mut diffs = HashMap::new();
    for comment in rows.iter().take(limit) {
        let pull = db::pull_requests::records::get_pull_request_by_id(
            &state.pool,
            comment.pull_request_id,
        )
        .await
        .map_err(Error::unavailable)?
        .ok_or_else(Error::missing)?;
        items.push(document_cached(&state, &owner, &repo, &pull, comment, &mut diffs).await?);
    }
    page.response(&uri, items, has_next)
}
async fn list_repo(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, name)): Path<(String, String)>,
    Query(query): Query<List>,
    OriginalUri(uri): OriginalUri,
) -> Result<Response> {
    collection(state, headers, (owner, name), None, None, query, uri).await
}
async fn list_pull(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, name, number)): Path<(String, String, i32)>,
    Query(query): Query<List>,
    OriginalUri(uri): OriginalUri,
) -> Result<Response> {
    let (_, pull) = target(&state, &headers, &owner, &name, number).await?;
    collection(
        state,
        headers,
        (owner, name),
        Some(pull.id),
        None,
        query,
        uri,
    )
    .await
}
async fn list_review(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, name, number, id)): Path<(String, String, i32, i64)>,
    Query(query): Query<List>,
    OriginalUri(uri): OriginalUri,
) -> Result<Response> {
    let (_, pull) = target(&state, &headers, &owner, &name, number).await?;
    let review = db::pull_requests::reviews::get_pull_request_review_by_id(&state.pool, id)
        .await
        .map_err(Error::unavailable)?
        .filter(|r| r.pull_request_id == pull.id)
        .ok_or_else(Error::missing)?;
    if review.state == "pending"
        && !auth::authenticate(&state, &headers, false)
            .await?
            .is_some_and(|a| a.user.id == review.author_id)
    {
        return Err(Error::missing());
    }
    collection(
        state,
        headers,
        (owner, name),
        Some(pull.id),
        Some(id),
        query,
        uri,
    )
    .await
}

pub(super) fn handlers() -> Vec<(&'static str, MethodRouter<Arc<AppState>>)> {
    vec![
        ("pulls/create-review-comment", post(create)),
        ("pulls/create-reply-for-review-comment", post(reply)),
        ("pulls/get-review-comment", get(read)),
        ("pulls/update-review-comment", patch(update)),
        ("pulls/delete-review-comment", delete(remove)),
        ("pulls/list-review-comments-for-repo", get(list_repo)),
        ("pulls/list-review-comments", get(list_pull)),
        ("pulls/list-comments-for-review", get(list_review)),
    ]
}
