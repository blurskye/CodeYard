use crate::backend::api::http::context::PageContext;
use crate::backend::api::http::error::ApiError;
use crate::backend::api::http::session::get_current_user;
use crate::backend::api::repos::access::check_repo_read_access;
use crate::backend::api::repos::chrome::{self, page_window};
use crate::backend::api::repos::milestones::milestone_view;
use crate::backend::api::repos::pulls::review_forms;
use crate::backend::api::repos::refs::references::resolve_references_or_none;
use crate::backend::api::user::responses::PublicUserResponse as UserResponse;
use crate::backend::app::AppState;
use crate::backend::db;
use crate::backend::db::pull_requests::reviews::DiscussionItem;
use crate::backend::issues::{COMMENTS_PER_PAGE, MAX_COMMENT_PAGE};
use crate::backend::repositories::pull_requests::mutations::UpdatePullRequestRequest;
use crate::backend::repositories::pull_requests::restack;
use crate::backend::repositories::pull_requests::stacks;
use crate::backend::repositories::store::{allowed_merge_methods, repo_store, MergeMethod};
use crate::webui::pages::repos::issues as view;
use crate::webui::pages::repos::issues::DetailDraft;
use crate::webui::pages::repos::issues::Kind;
use crate::webui::pages::repos::issues::NewDraft;
use crate::webui::pages::repos::pulls::MAX_LISTED_COMMITS;
use axum::extract::Path;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::http::StatusCode;
use axum::http::Uri;
use axum::response::Response;
use axum::Json;
use chrono::{DateTime, Utc};
use serde::Serialize;
use std::collections::HashMap;
use std::sync::Arc;
pub async fn update_pull_request(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, repo, number)): Path<(String, String, i32)>,
    Json(req): Json<UpdatePullRequestRequest>,
) -> Result<Json<PullRequestDetailResponse>, (StatusCode, Json<ApiError>)> {
    let current_actor = get_current_user(&state.pool, &headers).await?;
    let pull_request = crate::backend::repositories::pull_requests::mutations::update_pull_request(
        &state.pool,
        &state.ci_runs,
        &state.notification_broadcaster,
        current_actor.id,
        (owner, repo, number),
        req,
    )
    .await
    .map_err(super::operation_error)?;
    let author = db::users::accounts::get_user_by_id(&state.pool, pull_request.author_id)
        .await
        .map_err(crate::backend::api::http::error::discussion_write_error)?;

    Ok(Json(
        detail_response(&state.pool, pull_request, author.map(UserResponse::from)).await?,
    ))
}

#[derive(Serialize)]
pub struct PullRequestResponse {
    pub id: i64,
    pub number: i32,
    pub title: String,
    pub body: String,
    pub state: String,
    pub head_repo_id: Option<i64>,
    pub head_branch: String,
    pub head_sha: String,
    pub base_branch: String,
    pub base_sha: String,
    pub is_draft: bool,
    pub mergeable: Option<bool>,
    pub stack_id: Option<i64>,
    pub author_id: i64,
    pub author: Option<UserResponse>,
    pub created_at: String,
    pub updated_at: String,
    pub merged_at: Option<String>,
    pub closed_at: Option<String>,
}

impl PullRequestResponse {
    pub fn from_pull_request(
        pull_request: db::models::PullRequest,
        author: Option<UserResponse>,
    ) -> Self {
        Self {
            id: pull_request.id,
            number: pull_request.number,
            title: pull_request.title,
            body: pull_request.body,
            state: pull_request.state,
            head_repo_id: pull_request.head_repo_id,
            head_branch: pull_request.head_branch,
            head_sha: pull_request.head_sha,
            base_branch: pull_request.base_branch,
            base_sha: pull_request.base_sha,
            is_draft: pull_request.is_draft,
            mergeable: pull_request.mergeable,
            stack_id: pull_request.stack_id,
            author_id: pull_request.author_id,
            author,
            created_at: pull_request.created_at.to_rfc3339(),
            updated_at: pull_request.updated_at.to_rfc3339(),
            merged_at: pull_request.merged_at.map(|dt| dt.to_rfc3339()),
            closed_at: pull_request.closed_at.map(|dt| dt.to_rfc3339()),
        }
    }
}

/// A single pull request answer: the pull request, and the stack it is in when it is in one.
#[derive(Serialize)]
pub struct PullRequestDetailResponse {
    #[serde(flatten)]
    pub pull_request: PullRequestResponse,
    pub stack: Option<StackResponse>,
}

#[derive(Serialize)]
pub struct StackResponse {
    pub id: i64,
    pub base_branch: String,
    pub size: usize,
    pub position: usize,
    pub entries: Vec<StackEntryResponse>,
}

#[derive(Serialize)]
pub struct StackEntryResponse {
    pub number: i32,
    pub title: String,
    pub state: String,
    pub merged: bool,
    pub draft: bool,
    pub head_branch: String,
    pub base_branch: String,
}

/// The single pull request answer. A storage failure is an error, never an answer without
/// the stack.
pub(crate) async fn detail_response(
    pool: &db::DbPool,
    pull_request: db::models::PullRequest,
    author: Option<UserResponse>,
) -> Result<PullRequestDetailResponse, ApiFailure> {
    let stack = stacks::stack_of(pool, &pull_request).await.map_err(|_| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ApiError::new(
                "database_error",
                "Collaboration storage is temporarily unavailable",
            )),
        )
    })?;
    let stack = stack.map(|stack| {
        let position = stack.position(pull_request.id);
        let base_branch = stack.trunk().to_owned();
        StackResponse {
            id: stack.id,
            base_branch,
            size: stack.layers.len(),
            position,
            entries: stack
                .layers
                .into_iter()
                .map(|layer| StackEntryResponse {
                    number: layer.number,
                    title: layer.title,
                    state: layer.state,
                    merged: layer.merged,
                    draft: layer.is_draft,
                    head_branch: layer.head_branch,
                    base_branch: layer.base_branch,
                })
                .collect(),
        }
    });
    Ok(PullRequestDetailResponse {
        pull_request: PullRequestResponse::from_pull_request(pull_request, author),
        stack,
    })
}

pub async fn get_pull_request(
    State(state): State<Arc<AppState>>,
    Path((owner, repo, number)): Path<(String, String, i32)>,
    headers: HeaderMap,
) -> Result<Json<PullRequestDetailResponse>, (StatusCode, Json<ApiError>)> {
    let repository = db::repositories::records::get_repository(&state.pool, &owner, &repo)
        .await
        .map_err(|_| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ApiError::new(
                    "database_error",
                    "Collaboration storage is temporarily unavailable",
                )),
            )
        })?
        .ok_or_else(|| {
            (
                StatusCode::NOT_FOUND,
                Json(ApiError::new("not_found", "Repository not found")),
            )
        })?;

    check_repo_read_access(&state.pool, &repository, &headers).await?;

    let pull_request =
        db::pull_requests::records::get_pull_request(&state.pool, repository.id, number)
            .await
            .map_err(|_| {
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(ApiError::new(
                        "database_error",
                        "Collaboration storage is temporarily unavailable",
                    )),
                )
            })?
            .ok_or_else(|| {
                (
                    StatusCode::NOT_FOUND,
                    Json(ApiError::new("not_found", "Pull request not found")),
                )
            })?;

    let author = db::users::accounts::get_user_by_id(&state.pool, pull_request.author_id)
        .await
        .map_err(crate::backend::api::http::error::discussion_write_error)?;

    Ok(Json(
        detail_response(&state.pool, pull_request, author.map(UserResponse::from)).await?,
    ))
}

pub(crate) type ResultPage<T> = Result<T, Response>;
pub(crate) type ApiFailure = (StatusCode, Json<ApiError>);
// These handlers are registered only on the two exact families above.
pub(crate) fn kind(uri: &Uri) -> Kind {
    if uri.path().split('/').nth(3) == Some("pulls") {
        Kind::Pull
    } else {
        Kind::Issue
    }
}
pub(crate) fn unavailable(ctx: &PageContext) -> Response {
    ctx.error(
        StatusCode::SERVICE_UNAVAILABLE,
        "Collaboration data is temporarily unavailable. Please try again.",
    )
}
pub(crate) fn safe_api_error(failure: &ApiFailure) -> (StatusCode, String) {
    let (status, Json(error)) = failure;
    let message = if status.is_server_error() {
        match error.message.as_str() {
            "The branch was merged, but the pull request status could not be updated." => "The branch was merged, but the pull request status could not be updated. Reload before retrying.",
            "Pull request opened but its head could not be fetched; retry after storage is available" => "The pull request was opened, but its head could not be fetched. Reload before retrying.",
            "The change was saved, but pull request revisions could not be refreshed. Reload before retrying." => "The change was saved, but pull request revisions could not be refreshed. Reload before retrying.",
            _ => "The operation could not be completed. Its outcome may be uncertain; reload before retrying.",
        }
    } else {
        match error.error.as_str() {
            "validation_error"
            | "invalid_state"
            | "branch_protection"
            | "unresolved_conversations"
            | "ci_not_queued"
            | "stack_needs_merge_commit"
            | "stack_needs_restack"
            | "stack_broken"
            | "stack_layer_closed"
            | "stack_join_refused"
            | "stack_fork_head"
            | "not_stacked"
            | "rebase_conflict"
            | "merge_method_not_allowed"
            | "unsupported_merge"
            | "revision_conflict"
            | "response_too_large"
            | "milestone_title_taken"
            | "unassignable"
            | "too_many_assignees" => &error.message,
            "merge_conflict" => "Git could not merge these branches. Resolve the conflict before retrying.",
            "not_found" => "The requested resource was not found or you do not have access.",
            "unauthorized" => "Please sign in again before continuing.",
            "forbidden" => "You do not have permission to perform this action.",
            "git_error" => "The selected Git branches are unavailable. Check the comparison and try again.",
            _ => "The operation was rejected. Reload the page and check your permissions and the current state.",
        }
    };
    (*status, message.to_owned())
}
pub(crate) fn api_error(ctx: &PageContext, failure: ApiFailure) -> Response {
    let (status, message) = safe_api_error(&failure);
    ctx.error(status, &message)
}
pub(crate) fn with_status(mut response: Response, status: StatusCode) -> Response {
    // Preserve shared renderer/runtime failure instead of masking it as recovery.
    if response.status() == StatusCode::OK {
        *response.status_mut() = status;
    }
    response
}
pub(crate) fn field(form: &HashMap<String, String>, name: &str) -> String {
    form.get(name).cloned().unwrap_or_default()
}
pub(crate) fn submitted_new(form: &HashMap<String, String>) -> NewDraft {
    NewDraft {
        head_repo: field(form, "head_repo"),
        title: field(form, "title"),
        body: field(form, "body"),
        head: field(form, "head"),
        base: field(form, "base"),
        draft: form.get("draft").is_some_and(|v| v == "on"),
        ..NewDraft::default()
    }
}
pub(crate) fn submitted_detail(form: &HashMap<String, String>, action: &str) -> DetailDraft {
    match action {
        "edit" => DetailDraft {
            title: Some(field(form, "title")),
            body: Some(field(form, "body")),
            preview: field(form, "intent") == "preview",
            ..Default::default()
        },
        "comment" => DetailDraft {
            comment: field(form, "body"),
            preview: field(form, "intent") == "preview",
            ..Default::default()
        },
        "label" => DetailDraft {
            label: field(form, "label"),
            ..Default::default()
        },
        _ => DetailDraft::default(),
    }
}
#[expect(
    clippy::result_large_err,
    reason = "HTML adapters preserve complete Axum responses under the PageContext contract."
)]
pub(crate) fn authorize_post(ctx: &PageContext, form: &HashMap<String, String>) -> ResultPage<()> {
    ctx.check_csrf(form)?;
    ctx.require_user()?;
    Ok(())
}

pub(crate) struct RepositoryData {
    pub(crate) record: db::models::Repository,
    pub(crate) view: view::Repository,
}
pub(crate) async fn repository(
    ctx: &PageContext,
    owner: &str,
    name: &str,
) -> ResultPage<RepositoryData> {
    let record = db::repositories::records::get_repository(&ctx.state.pool, owner, name)
        .await
        .map_err(|_| unavailable(ctx))?
        .ok_or_else(|| ctx.error(StatusCode::NOT_FOUND, "Repository not found."))?;
    check_repo_read_access(&ctx.state.pool, &record, &ctx.headers)
        .await
        .map_err(|failure| api_error(ctx, failure))?;
    let can_write = match &ctx.user {
        Some(user) => {
            crate::backend::repositories::access::can_write(&ctx.state.pool, user, &record)
                .await
                .map_err(|_| unavailable(ctx))?
        }
        None => false,
    };
    let can_admin = match &ctx.user {
        Some(user) => {
            crate::backend::repositories::access::can_admin(&ctx.state.pool, user, &record)
                .await
                .map_err(|_| unavailable(ctx))?
        }
        None => false,
    };
    // Reuse the Code page's authorized projection, including visibility, real
    // engagement counts and actor-scoped actions. Never serialize the DB record.
    let chrome = crate::backend::api::repos::chrome::load_chrome(ctx, owner, name).await?;
    Ok(RepositoryData {
        record,
        view: view::Repository {
            owner: owner.to_owned(),
            name: name.to_owned(),
            signed_in: ctx.user.is_some(),
            can_write,
            can_admin,
            csrf: ctx.csrf_token.clone(),
            viewer: ctx
                .user
                .as_ref()
                .map(|user| user.username.clone())
                .unwrap_or_default(),
            chrome,
        },
    })
}
pub(crate) async fn author(ctx: &PageContext, id: i64) -> ResultPage<String> {
    Ok(db::users::accounts::get_user_by_id(&ctx.state.pool, id)
        .await
        .map_err(|_| unavailable(ctx))?
        .map(|user| user.username)
        .unwrap_or_else(|| "Deleted user".into()))
}
pub(crate) fn label(value: db::models::Label) -> view::Label {
    view::Label {
        id: value.id,
        name: value.name,
        color: value.color,
        description: value.description.unwrap_or_default(),
    }
}
pub(crate) fn issue_item(
    issue: db::models::Issue,
    author: String,
    labels: Vec<db::models::Label>,
) -> view::Item {
    view::Item {
        number: issue.number,
        title: issue.title,
        body: issue.body,
        state: issue.state,
        author,
        created: issue.created_at.to_rfc3339(),
        updated: issue.updated_at.to_rfc3339(),
        labels: labels.into_iter().map(label).collect(),
        stacked: false,
        stack: None,
        stack_candidate: None,
        assignees: Vec::new(),
        milestone: None,
        comparison: None,
        comment_count: 0,
        commit_count: None,
        file_count: None,
        check_count: 0,
        review_state: None,
        check_state: None,
        reactions: Vec::new(),
    }
}
/// A pull request's head repository as this viewer may see it. The current name and
/// link are shown only to a viewer who can read that repository.
pub(crate) enum HeadRepository {
    Readable(db::pull_requests::records::PullRequestHeadDisplay),
    /// It exists, but this viewer cannot read it.
    Unreadable,
    Deleted,
}

async fn can_view_head(ctx: &PageContext, head: &db::models::Repository) -> ResultPage<bool> {
    crate::backend::repositories::access::can_read(&ctx.state.pool, ctx.user.as_ref(), head)
        .await
        .map_err(|_| unavailable(ctx))
}

/// The head repository of a pull request opened from a fork, for the detail page.
async fn head_for_viewer(
    ctx: &PageContext,
    head: Option<db::models::Repository>,
) -> ResultPage<HeadRepository> {
    let Some(head) = head else {
        return Ok(HeadRepository::Deleted);
    };
    if !can_view_head(ctx, &head).await? {
        return Ok(HeadRepository::Unreadable);
    }
    let owner_name = db::repositories::records::repository_owner_name(&ctx.state.pool, head.id)
        .await
        .map_err(|_| unavailable(ctx))?;
    Ok(HeadRepository::Readable(
        db::pull_requests::records::PullRequestHeadDisplay {
            id: head.id,
            name: head.name,
            owner_name,
        },
    ))
}

/// The head repository of a pull request in a list. The listing supplies the current
/// name; the viewer's read access decides whether it is shown.
pub(crate) async fn listed_head(
    ctx: &PageContext,
    head: Option<db::pull_requests::records::PullRequestHeadDisplay>,
) -> ResultPage<HeadRepository> {
    let Some(head) = head else {
        return Ok(HeadRepository::Deleted);
    };
    let Some(record) = db::repositories::records::get_repository_by_id(&ctx.state.pool, head.id)
        .await
        .map_err(|_| unavailable(ctx))?
    else {
        return Ok(HeadRepository::Deleted);
    };
    if can_view_head(ctx, &record).await? {
        Ok(HeadRepository::Readable(head))
    } else {
        Ok(HeadRepository::Unreadable)
    }
}

/// The assignees of each listed issue or pull request, by item id, in name order.
pub(crate) async fn listed_assignees(
    ctx: &PageContext,
    item_ids: &[i64],
    pull_requests: bool,
) -> ResultPage<HashMap<i64, Vec<String>>> {
    let mut assignees: HashMap<i64, Vec<String>> = HashMap::new();
    for (id, username) in
        db::assignees::list_item_assignees(&ctx.state.pool, pull_requests, item_ids)
            .await
            .map_err(|_| unavailable(ctx))?
    {
        assignees.entry(id).or_default().push(username);
    }
    Ok(assignees)
}

pub(crate) fn pull_item(
    ctx: &PageContext,
    repo: &RepositoryData,
    pull: db::models::PullRequest,
    author: String,
    head_repository: HeadRepository,
) -> Result<view::Item, Box<Response>> {
    // Presentation only: the store decides again when the form is sent.
    let can_ready = pull.is_draft
        && pull.state == "open"
        && !repo.record.is_archived
        && (repo.view.can_write
            || ctx
                .user
                .as_ref()
                .is_some_and(|user| user.id == pull.author_id));
    let (head_label, head_url) = match pull.head_repo_id {
        Some(id) if id == repo.record.id => (
            pull.head_branch.clone(),
            Some(crate::webui::pages::repos::ref_path::file_url(
                &repo.view.url(),
                "tree",
                &pull.head_branch,
                "",
            )),
        ),
        Some(_) => match head_repository {
            HeadRepository::Readable(head) => {
                let owner = head.owner_name.ok_or_else(|| Box::new(unavailable(ctx)))?;
                let label = format!("{owner}/{}:{}", head.name, pull.head_branch);
                let url = crate::webui::pages::repos::ref_path::file_url(
                    &view::repo_url(&owner, &head.name),
                    "tree",
                    &pull.head_branch,
                    "",
                );
                (label, Some(url))
            }
            HeadRepository::Unreadable => ("unavailable repository".into(), None),
            HeadRepository::Deleted => ("deleted repository".into(), None),
        },
        None => ("deleted repository".into(), None),
    };
    let may_execute = ctx.user.as_ref().is_some_and(|user| {
        user.is_admin
            || ctx
                .state
                .ci_runs
                .config
                .trusted_actor_ids
                .contains(&user.id)
    });
    let can_run_workflows = repo.view.can_write
        && may_execute
        && pull.state == "open"
        && !pull.is_draft
        && pull.head_repo_id != Some(repo.record.id);
    // Presentation only: the review handler checks the write right and the author again.
    let can_vote = repo.view.can_write
        && ctx
            .user
            .as_ref()
            .is_some_and(|user| user.id != pull.author_id);
    Ok(view::Item {
        number: pull.number,
        title: pull.title,
        body: pull.body,
        state: pull.state,
        author,
        created: pull.created_at.to_rfc3339(),
        updated: pull.updated_at.to_rfc3339(),
        labels: vec![],
        stacked: pull.stack_id.is_some(),
        stack: None,
        stack_candidate: None,
        assignees: Vec::new(),
        milestone: None,
        comment_count: 0,
        commit_count: None,
        file_count: None,
        check_count: 0,
        review_state: None,
        check_state: None,
        reactions: Vec::new(),
        comparison: Some(view::Comparison {
            head_label,
            head_url,
            can_run_workflows,
            head: pull.head_branch,
            merge_target: pull.base_branch.clone(),
            merge_with: Vec::new(),
            blockers: Vec::new(),
            base: pull.base_branch,
            head_sha: pull.head_sha,
            base_sha: pull.base_sha,
            draft: pull.is_draft,
            can_ready,
            merge_commit_only: false,
            merge_commit_refused: false,
            merged_commit: pull.merge_commit_sha,
            can_vote,
            merge_methods: allowed_merge_methods(&repo.record)
                .into_iter()
                .map(|method| method.as_str().to_owned())
                .collect(),
            has_checks: false,
            has_workflows: false,
        }),
    })
}
/// The stack a pull request page shows, with the viewed pull request marked.
fn stack_map(stack: &stacks::Stack, current: i64, restack: bool) -> view::StackMap {
    view::StackMap {
        trunk: stack.trunk().to_owned(),
        position: stack.position(current),
        restack,
        layers: stack
            .layers
            .iter()
            .map(|layer| view::StackLayer {
                number: layer.number,
                title: layer.title.clone(),
                state: layer.state.clone(),
                draft: layer.is_draft,
                current: layer.id == current,
            })
            .collect(),
    }
}

/// The milestone an issue or pull request names, as its page shows it.
pub(crate) async fn named_milestone(
    ctx: &PageContext,
    repo: &RepositoryData,
    milestone_id: Option<i64>,
) -> ResultPage<Option<view::Milestone>> {
    let Some(id) = milestone_id else {
        return Ok(None);
    };
    Ok(
        db::milestones::list_repo_milestones(&ctx.state.pool, repo.record.id, None)
            .await
            .map_err(|_| unavailable(ctx))?
            .into_iter()
            .find(|summary| summary.milestone.id == id)
            .map(milestone_view),
    )
}

pub(crate) async fn item(
    ctx: &PageContext,
    repo: &RepositoryData,
    kind: Kind,
    number: i32,
) -> ResultPage<view::Item> {
    if number <= 0 {
        return Err(ctx.error(StatusCode::NOT_FOUND, "Entry not found."));
    }
    match kind {
        Kind::Issue => {
            let value = db::issues::get_issue(&ctx.state.pool, repo.record.id, number)
                .await
                .map_err(|_| unavailable(ctx))?
                .ok_or_else(|| ctx.error(StatusCode::NOT_FOUND, "Issue not found."))?;
            let author = author(ctx, value.author_id).await?;
            let labels = db::labels::get_issue_labels(&ctx.state.pool, value.id)
                .await
                .map_err(|_| unavailable(ctx))?;
            let assignees = db::assignees::list_assignees(
                &ctx.state.pool,
                db::assignees::AssignedItem::Issue(value.id),
            )
            .await
            .map_err(|_| unavailable(ctx))?;
            let milestone = named_milestone(ctx, repo, value.milestone_id).await?;
            let comment_count = db::issues::count_issue_comments(&ctx.state.pool, &[value.id])
                .await
                .map_err(|_| unavailable(ctx))?
                .first()
                .map_or(0, |(_, count)| *count);
            let mut shown = issue_item(value, author, labels);
            shown.assignees = assignees;
            shown.milestone = milestone;
            shown.comment_count = comment_count;
            Ok(shown)
        }
        Kind::Pull => {
            let value = db::pull_requests::records::get_pull_request(
                &ctx.state.pool,
                repo.record.id,
                number,
            )
            .await
            .map_err(|_| unavailable(ctx))?
            .ok_or_else(|| ctx.error(StatusCode::NOT_FOUND, "Pull request not found."))?;
            let author = author(ctx, value.author_id).await?;
            let head = match value.head_repo_id {
                Some(id) if id != repo.record.id => {
                    db::repositories::records::get_repository_by_id(&ctx.state.pool, id)
                        .await
                        .map_err(|_| unavailable(ctx))?
                }
                _ => None,
            };
            let head_repository = head_for_viewer(ctx, head).await?;
            let current = value.id;
            let stack = stacks::stack_of(&ctx.state.pool, &value)
                .await
                .map_err(|_| unavailable(ctx))?;
            let stack_candidate = stacks::join_candidate(&ctx.state.pool, &value)
                .await
                .map_err(|_| unavailable(ctx))?;
            let (merge_target, merge_with) = stacks::merge_target(&ctx.state.pool, &value)
                .await
                .map_err(|_| unavailable(ctx))?;
            // Only a writer sees the button, so readers do not pay for the check.
            let restack = match &stack {
                Some(stack) if repo.view.can_write => {
                    restack::restack_pending(
                        &ctx.state.ci_runs,
                        &repo.record,
                        &value,
                        &stack.layers,
                    )
                    .await
                }
                _ => false,
            };
            let milestone_id = value.milestone_id;
            let labels = db::labels::get_pull_request_labels(&ctx.state.pool, current)
                .await
                .map_err(|_| unavailable(ctx))?;
            let head_sha = value.head_sha.clone();
            let mut view_item = pull_item(ctx, repo, value, author, head_repository)
                .map_err(|response| *response)?;
            view_item.labels = labels.into_iter().map(label).collect();
            view_item.milestone = named_milestone(ctx, repo, milestone_id).await?;
            view_item.stack = stack
                .as_ref()
                .map(|stack| stack_map(stack, current, restack));
            view_item.stack_candidate = stack_candidate;
            view_item.assignees = db::assignees::list_assignees(
                &ctx.state.pool,
                db::assignees::AssignedItem::PullRequest(current),
            )
            .await
            .map_err(|_| unavailable(ctx))?;
            view_item.comment_count =
                db::pull_requests::reviews::count_pull_request_comments(&ctx.state.pool, current)
                    .await
                    .map_err(|_| unavailable(ctx))?;
            // A check run or a commit status on the head commit is what makes a green merge box mean
            // something; the merge box says so when there is none.
            let runs = db::ci::latest_ci_runs_for_commit(
                &ctx.state.pool,
                repo.record.id,
                &head_sha,
                current,
            )
            .await
            .map_err(|_| unavailable(ctx))?;
            let statuses = db::repositories::protection::list_commit_statuses(
                &ctx.state.pool,
                repo.record.id,
                &head_sha,
            )
            .await
            .map_err(|_| unavailable(ctx))?;
            view_item.check_count = runs.len() as i64;
            // The Commits tab counts the commits of the pull request. When Git cannot count them the
            // count is left out, so the tab shows no number instead of a wrong one.
            let commit_count = match view_item.comparison.as_ref() {
                Some(compare) => repo_store(&repo.record, ctx.state.cluster.as_ref())
                    .commits_between(&compare.base_sha, &compare.head_sha, MAX_LISTED_COMMITS)
                    .await
                    .ok()
                    .map(|commits| commits.len() as i64),
                None => None,
            };
            view_item.commit_count = commit_count;
            // The Files changed tab counts the files of the pull request. As with the commits, the count
            // is left out when Git cannot diff the two revisions.
            let file_count = match view_item.comparison.as_ref() {
                Some(compare) => repo_store(&repo.record, ctx.state.cluster.as_ref())
                    .diff(&compare.base_sha, &compare.head_sha)
                    .await
                    .ok()
                    .map(|diff| i64::from(diff.changed_files)),
                None => None,
            };
            view_item.file_count = file_count;
            if let Some(compare) = view_item.comparison.as_mut() {
                compare.merge_commit_only = merge_with.len() > 1;
                compare.merge_target = merge_target;
                compare.merge_with = merge_with;
                compare.has_checks = !runs.is_empty() || !statuses.is_empty();
            }
            Ok(view_item)
        }
    }
}
/// One page of the discussion, from the `page` query value (default 1). One extra
/// row is read to tell whether a next page exists.
pub(crate) async fn discussion(
    ctx: &PageContext,
    repo: &RepositoryData,
    kind: Kind,
    number: i32,
) -> ResultPage<view::CommentPage> {
    let page = chrome::number(ctx, "page", 1, MAX_COMMENT_PAGE)?;
    if page == 0 {
        return Err(ctx.error(StatusCode::BAD_REQUEST, "Page numbers start at 1."));
    }
    let (offset, _) = page_window(page, COMMENTS_PER_PAGE)
        .ok_or_else(|| ctx.error(StatusCode::BAD_REQUEST, "Invalid comment page."))?;
    let fetch = (COMMENTS_PER_PAGE + 1) as i64;
    let offset = offset as i64;
    let mut comments = Vec::new();
    let mut reviews = Vec::new();
    let has_more: bool;
    // The item and the span of time this page covers, for the events that fall in it.
    let subject: (db::timeline::Subject, i64);
    let span: (Option<DateTime<Utc>>, Option<DateTime<Utc>>);
    match kind {
        Kind::Issue => {
            let issue = db::issues::get_issue(&ctx.state.pool, repo.record.id, number)
                .await
                .map_err(|_| unavailable(ctx))?
                .ok_or_else(|| ctx.error(StatusCode::NOT_FOUND, "Issue not found."))?;
            let rows = db::issues::list_issue_comments(&ctx.state.pool, issue.id, fetch, offset)
                .await
                .map_err(|_| unavailable(ctx))?;
            has_more = chrome::has_next_page(page, rows.len(), COMMENTS_PER_PAGE);
            subject = (db::timeline::Subject::Issue, issue.id);
            span = (
                (page > 1)
                    .then(|| rows.first().map(|row| row.comment.created_at))
                    .flatten(),
                rows.get(COMMENTS_PER_PAGE)
                    .map(|row| row.comment.created_at),
            );
            for comment in rows.into_iter().take(COMMENTS_PER_PAGE) {
                let author = comment.author.username;
                let comment = comment.comment;
                comments.push(view::Comment {
                    id: comment.id,
                    author,
                    body: comment.body,
                    created: comment.created_at.to_rfc3339(),
                    location: None,
                    conversation: None,
                    reactions: Vec::new(),
                });
            }
        }
        Kind::Pull => {
            let pull = db::pull_requests::records::get_pull_request(
                &ctx.state.pool,
                repo.record.id,
                number,
            )
            .await
            .map_err(|_| unavailable(ctx))?
            .ok_or_else(|| ctx.error(StatusCode::NOT_FOUND, "Pull request not found."))?;
            // The page is a run of Conversation items, so the count, the links and the reviews all
            // agree. The page's span runs from its first item up to the next page's first item.
            let rows = db::pull_requests::reviews::list_discussion_items(
                &ctx.state.pool,
                pull.id,
                fetch,
                offset,
            )
            .await
            .map_err(|_| unavailable(ctx))?;
            has_more = chrome::has_next_page(page, rows.len(), COMMENTS_PER_PAGE);
            let first = rows.first().map(DiscussionItem::created_at);
            span = (
                (page > 1).then_some(first).flatten(),
                rows.get(COMMENTS_PER_PAGE).map(DiscussionItem::created_at),
            );
            subject = (db::timeline::Subject::PullRequest, pull.id);
            let mut page_reviews = Vec::new();
            for row in rows.into_iter().take(COMMENTS_PER_PAGE) {
                match row {
                    DiscussionItem::Comment(entry) => {
                        comments.push(review_forms::discussion_comment(ctx, repo, &pull, entry));
                    }
                    DiscussionItem::Review(entry) => page_reviews.push(entry),
                }
            }
            reviews = review_forms::page_reviews(ctx, repo, &pull, page_reviews).await?;
        }
    }
    let events = db::timeline::list_events(
        &ctx.state.pool,
        subject.0.name(),
        subject.1,
        span.0,
        span.1,
        db::timeline::EVENT_LIMIT,
    )
    .await
    .map_err(|_| unavailable(ctx))?
    .into_iter()
    .map(|event| view::Event {
        actor: event.actor.unwrap_or_else(|| "Deleted user".to_owned()),
        kind: event.kind,
        detail: event.detail,
        previous: event.previous,
        created: event.created_at.to_rfc3339(),
    })
    .collect();
    Ok(view::CommentPage {
        comments,
        reviews,
        page,
        has_more,
        events,
    })
}
pub(crate) async fn render_detail(
    ctx: &PageContext,
    repo: &RepositoryData,
    kind: Kind,
    shown: &view::Item,
    draft: &DetailDraft,
    error: Option<&str>,
    status: StatusCode,
) -> ResultPage<Response> {
    let mut value = shown.clone();
    fill_merge_blockers(ctx, repo, kind, draft, &mut value).await;
    let mut comments = discussion(ctx, repo, kind, value.number).await?;
    crate::backend::api::repos::issues::reactions::attach(
        ctx,
        repo,
        kind,
        &mut value,
        &mut comments,
    )
    .await?;
    // Every text this page renders as Markdown, including what a form re-renders.
    let mut texts = vec![value.body.as_str(), draft.comment.as_str()];
    texts.extend(draft.body.as_deref());
    texts.extend(
        comments
            .comments
            .iter()
            .map(|comment| comment.body.as_str()),
    );
    texts.extend(
        comments
            .reviews
            .iter()
            .filter_map(|review| review.body.as_deref()),
    );
    texts.extend(
        comments
            .reviews
            .iter()
            .flat_map(|review| review.comments.iter().map(|comment| comment.body.as_str())),
    );
    // A detail page also re-renders the user's submitted draft, so a failed lookup links nothing.
    // Every text here renders as a comment, so the references are resolved in comment mode.
    let references = resolve_references_or_none(
        ctx,
        &repo.record,
        &repo.view.owner,
        &repo.view.name,
        &texts,
        false,
    )
    .await;
    // The label picker lists every label of the repository. A label the item has is removed by
    // clicking it, and any other label is added the same way.
    let labels = db::labels::list_repo_labels(&ctx.state.pool, repo.record.id)
        .await
        .map_err(|_| unavailable(ctx))?
        .into_iter()
        .map(label)
        .collect();
    // Choices are offered to a writer only. Each form checks the write right again when it is sent.
    let (assignable, milestones): (Vec<String>, Vec<view::Milestone>) = if repo.view.can_write {
        let assignable = if (value.assignees.len() as i64) < db::assignees::MAX_ASSIGNEES {
            db::assignees::list_assignable_usernames(&ctx.state.pool, repo.record.id)
                .await
                .map_err(|_| unavailable(ctx))?
                .into_iter()
                .filter(|name| !value.assignees.contains(name))
                .collect()
        } else {
            Vec::new()
        };
        let milestones =
            db::milestones::list_repo_milestones(&ctx.state.pool, repo.record.id, None)
                .await
                .map_err(|_| unavailable(ctx))?
                .into_iter()
                .map(milestone_view)
                .collect();
        (assignable, milestones)
    } else {
        (Vec::new(), Vec::new())
    };
    let reviewers = match &value.comparison {
        Some(compare) if kind == Kind::Pull => {
            review_forms::reviewers(ctx, repo, value.number, &compare.head_sha).await?
        }
        _ => Vec::new(),
    };
    Ok(with_status(
        ctx.page_model(
            &format!("{} #{}", value.title, value.number),
            page(view::Page::Detail {
                repository: repo.view.clone(),
                kind,
                item: value.clone(),
                comments,
                available_labels: labels,
                draft: draft.clone(),
                error: error.map(str::to_owned),
                references: Box::new(references),
                reviewers,
                assignable,
                milestones,
            }),
        ),
        status,
    ))
}

/// The page after a form that changes an issue or pull request: the item again, or the item
/// with the reason the change was refused.
pub(crate) async fn detail_outcome(
    ctx: &PageContext,
    repo: &RepositoryData,
    kind: Kind,
    shown: &view::Item,
    number: i32,
    outcome: Result<(), ApiFailure>,
) -> ResultPage<Response> {
    match outcome {
        Ok(()) => Ok(ctx.redirect(&format!("{}/{}", repo.view.family_url(kind), number))),
        Err(failure) => {
            let (status, message) = safe_api_error(&failure);
            render_detail(
                ctx,
                repo,
                kind,
                shown,
                &DetailDraft::default(),
                Some(&message),
                status,
            )
            .await
        }
    }
}

/// Lists what blocks a merge in the merge panel. Only the detail page shows the panel, so only
/// its render asks, and only a writer gets the list. The list is for the method the form starts on,
/// which is the submitted one when a refused merge shows the form again. The list is advisory, so a
/// failed read leaves it empty.
async fn fill_merge_blockers(
    ctx: &PageContext,
    repo: &RepositoryData,
    kind: Kind,
    draft: &DetailDraft,
    value: &mut view::Item,
) {
    let Some(viewer) = ctx.user.as_ref().filter(|_| repo.view.can_write) else {
        return;
    };
    if kind != Kind::Pull || value.state != "open" {
        return;
    }
    let number = value.number;
    let Some(compare) = value.comparison.as_mut() else {
        return;
    };
    // The merge box links "No checks configured" to the Actions page only when the head commit has
    // workflows. A failed read leaves the link out.
    if !compare.has_checks {
        compare.has_workflows = repo_store(&repo.record, ctx.state.cluster.as_ref())
            .workflow_files(&compare.head_sha)
            .await
            .is_ok_and(|files| !files.is_empty());
    }
    let Ok(Some(pull_request)) =
        db::pull_requests::records::get_pull_request(&ctx.state.pool, repo.record.id, number).await
    else {
        return;
    };
    let blockers = |method| {
        crate::backend::repositories::pull_requests::merge::page_blockers(
            &ctx.state.pool,
            &ctx.state.ci_runs,
            &repo.record,
            viewer,
            &pull_request,
            method,
        )
    };
    // A branch that requires a linear history refuses the merge commit method for this viewer, so
    // the form starts on squash or rebase. A failed read leaves the method on offer, and the merge
    // checks again.
    let merge_blockers = blockers(MergeMethod::Merge).await;
    compare.merge_commit_refused = merge_blockers
        .iter()
        .any(|message| message.ends_with(view::MERGE_COMMIT_REFUSED));
    let allowed = allowed_merge_methods(&repo.record);
    let preselected = view::preselected_merge_method(compare, &draft.merge_method)
        .and_then(|name| allowed.into_iter().find(|method| method.as_str() == name));
    let Some(method) = preselected else {
        // No method can be chosen, for example a stack on a branch that requires a linear
        // history, so the page says why instead of offering a merge that cannot start.
        compare.blockers = merge_blockers;
        if compare.blockers.is_empty() {
            compare
                .blockers
                .push("No merge method is available for this pull request.".to_owned());
        }
        return;
    };
    compare.blockers = if method == MergeMethod::Merge {
        merge_blockers
    } else {
        blockers(method).await
    };
}

pub(crate) async fn branches(
    ctx: &PageContext,
    repo: &RepositoryData,
) -> ResultPage<(Vec<String>, HashMap<String, String>)> {
    let refs = repo_store(&repo.record, ctx.state.cluster.as_ref())
        .refs()
        .await
        .map_err(|error| {
            api_error(
                ctx,
                crate::backend::api::http::error::repo_store_error(
                    error,
                    StatusCode::SERVICE_UNAVAILABLE,
                ),
            )
        })?;
    let mut branches: Vec<_> = refs
        .keys()
        .filter_map(|r| r.strip_prefix("refs/heads/").map(str::to_owned))
        .collect();
    branches.sort();
    Ok((branches, refs))
}

pub(crate) async fn head_choices(
    ctx: &PageContext,
    base: &RepositoryData,
    draft: &mut NewDraft,
) -> ResultPage<(
    view::BranchChoices,
    db::models::Repository,
    HashMap<String, String>,
)> {
    let mut candidates = vec![base.record.clone()];
    let mut forks =
        db::repositories::discovery::list_forks(&ctx.state.pool, base.record.id, 100, 0)
            .await
            .map_err(|_| unavailable(ctx))?;
    // The user's own fork and an explicitly selected head stay reachable beyond the first
    // page. The loop below still checks read access for each of them.
    let own_fork = match &ctx.user {
        Some(user) => {
            db::repositories::discovery::get_user_fork(&ctx.state.pool, user.id, base.record.id)
                .await
                .map_err(|_| unavailable(ctx))?
        }
        None => None,
    };
    let selected = match draft.head_repo.parse::<i64>() {
        Ok(id) => db::repositories::records::get_repository_by_id(&ctx.state.pool, id)
            .await
            .map_err(|_| unavailable(ctx))?,
        Err(_) => None,
    };
    for fork in own_fork
        .into_iter()
        .chain(selected.filter(|repo| repo.forked_from_id == Some(base.record.id)))
    {
        if !forks.iter().any(|candidate| candidate.id == fork.id) {
            forks.push(fork);
        }
    }
    for fork in forks {
        match check_repo_read_access(&ctx.state.pool, &fork, &ctx.headers).await {
            Ok(()) => candidates.push(fork),
            Err((StatusCode::NOT_FOUND, _)) => {}
            Err(error) => return Err(api_error(ctx, error)),
        }
    }
    if draft.head_repo.is_empty() {
        let own = candidates.iter().find(|head| {
            head.id != base.record.id
                && ctx
                    .user
                    .as_ref()
                    .is_some_and(|user| head.is_personal_owner(user.id))
        });
        draft.head_repo = own.unwrap_or(&base.record).id.to_string();
    }
    let selected = candidates
        .iter()
        .find(|head| head.id.to_string() == draft.head_repo)
        .cloned()
        .ok_or_else(|| {
            ctx.error(
                StatusCode::NOT_FOUND,
                "Head repository not found or unavailable.",
            )
        })?;
    let ids = candidates
        .iter()
        .map(|candidate| candidate.id)
        .collect::<Vec<_>>();
    let mut owners = db::repositories::records::repository_owner_names(&ctx.state.pool, &ids)
        .await
        .map_err(|_| unavailable(ctx))?;
    let mut repositories = Vec::new();
    for candidate in candidates {
        let owner = owners
            .remove(&candidate.id)
            .ok_or_else(|| unavailable(ctx))?;
        repositories.push((candidate.id, format!("{owner}/{}", candidate.name)));
    }
    let (base_branches, _) = branches(ctx, base).await?;
    let refs = repo_store(&selected, ctx.state.cluster.as_ref())
        .refs()
        .await
        .map_err(|_| unavailable(ctx))?;
    let mut head_branches = refs
        .keys()
        .filter_map(|reference| reference.strip_prefix("refs/heads/").map(str::to_owned))
        .collect::<Vec<_>>();
    head_branches.sort();
    Ok((
        view::BranchChoices {
            repositories,
            base: base_branches,
            head: head_branches,
        },
        selected,
        refs,
    ))
}

/// The page model, boxed: it is much larger than most pages.
pub(crate) fn page(model: view::Page) -> crate::webui::app::Page {
    crate::webui::app::Page::Collaboration(Box::new(model))
}

pub(crate) fn expected(
    form: &HashMap<String, String>,
    value: &view::Item,
) -> Result<(), &'static str> {
    if form.get("expected_state") != Some(&value.state)
        || form.get("expected_updated") != Some(&value.updated)
    {
        Err("This entry changed after the form was opened. Review the current entry and your preserved submission before retrying.")
    } else {
        Ok(())
    }
}
