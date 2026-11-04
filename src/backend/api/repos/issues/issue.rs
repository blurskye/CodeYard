use crate::backend::api::http::context::PageContext;
use crate::backend::api::http::error::ApiError;
use crate::backend::api::http::session::get_current_user;
use crate::backend::api::repos::access::readable_repository;
use crate::backend::api::repos::discussion::*;
use crate::backend::api::repos::forms::authorize_post;
use crate::backend::api::repos::forms::field;
use crate::backend::api::repos::labels::LabelResponse;
use crate::backend::api::repos::responses::storage_unavailable;
use crate::backend::api::user::responses::PublicUserResponse as UserResponse;
use crate::backend::app::AppState;

use crate::backend::issues::mutations::UpdateIssueRequest;
use axum::extract::Form;
use axum::extract::Path;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::http::StatusCode;
use axum::http::Uri;
use axum::response::Response;
use axum::Json;
use codeyard_webui::pages::repos::issues::DetailDraft;
use codeyard_webui::pages::repos::issues::Kind;
use serde::Serialize;
use std::collections::HashMap;
use std::sync::Arc;
pub async fn update_issue(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, repo, number)): Path<(String, String, i32)>,
    Json(req): Json<UpdateIssueRequest>,
) -> Result<Json<IssueResponse>, (StatusCode, Json<ApiError>)> {
    let current_actor = get_current_user(&state.pool, &headers).await?;
    let repository = readable_repository(&state.pool, &owner, &repo, &headers).await?;
    let issue = crate::backend::issues::mutations::update_issue(
        &state.pool,
        &state.notification_broadcaster,
        current_actor.id,
        repository.id,
        number,
        req,
    )
    .await
    .map_err(crate::backend::api::repos::issues::operation_error)?;
    let author =
        codeyard_db_identity::users::accounts::get_user_by_id(&state.pool, issue.author_id)
            .await
            .map_err(crate::backend::api::http::error::discussion_write_error)?;
    let response = load_issue_response(&state.pool, issue, author.map(UserResponse::from))
        .await
        .map_err(crate::backend::api::http::error::discussion_write_error)?;

    Ok(Json(response))
}

#[derive(Serialize)]
pub struct IssueResponse {
    pub id: i64,
    pub number: i32,
    pub title: String,
    pub body: String,
    pub state: String,
    pub author_id: i64,
    pub author: Option<UserResponse>,
    pub labels: Vec<LabelResponse>,
    pub comments_count: i64,
    pub created_at: String,
    pub updated_at: String,
    pub closed_at: Option<String>,
}

impl IssueResponse {
    pub fn from_issue(
        issue: codeyard_db_core::models::Issue,
        author: Option<UserResponse>,
        labels: Vec<codeyard_db_core::models::Label>,
        comments_count: i64,
    ) -> Self {
        Self {
            id: issue.id,
            number: issue.number,
            title: issue.title,
            body: issue.body,
            state: issue.state,
            author_id: issue.author_id,
            author,
            labels: labels.into_iter().map(LabelResponse::from).collect(),
            comments_count,
            created_at: issue.created_at.to_rfc3339(),
            updated_at: issue.updated_at.to_rfc3339(),
            closed_at: issue.closed_at.map(|dt| dt.to_rfc3339()),
        }
    }
}

/// The response for one issue, with its labels and comment count read from storage.
/// Lists load the same data in batches instead.
pub(crate) async fn load_issue_response(
    pool: &codeyard_db_core::DbPool,
    issue: codeyard_db_core::models::Issue,
    author: Option<UserResponse>,
) -> anyhow::Result<IssueResponse> {
    let labels = codeyard_db_repos::labels::get_issue_labels(pool, issue.repo_id, issue.id).await?;
    let comments_count = codeyard_db_repos::issues::count_issue_comments(pool, &[issue.id])
        .await?
        .first()
        .map_or(0, |(_, count)| *count);
    Ok(IssueResponse::from_issue(
        issue,
        author,
        labels,
        comments_count,
    ))
}

pub async fn get_issue(
    State(state): State<Arc<AppState>>,
    Path((owner, repo, number)): Path<(String, String, i32)>,
    headers: HeaderMap,
) -> Result<Json<IssueResponse>, (StatusCode, Json<ApiError>)> {
    let repository = readable_repository(&state.pool, &owner, &repo, &headers).await?;

    let issue = codeyard_db_repos::issues::get_issue(&state.pool, repository.id, number)
        .await
        .map_err(|_| storage_unavailable())?
        .ok_or_else(|| {
            (
                StatusCode::NOT_FOUND,
                Json(ApiError::new("not_found", "Issue not found")),
            )
        })?;

    let author =
        codeyard_db_identity::users::accounts::get_user_by_id(&state.pool, issue.author_id)
            .await
            .map_err(|_| storage_unavailable())?;
    let response = load_issue_response(&state.pool, issue, author.map(UserResponse::from))
        .await
        .map_err(|_| storage_unavailable())?;

    Ok(Json(response))
}

pub(crate) async fn native_detail(
    State(state): State<Arc<AppState>>,
    Path((owner, name, number)): Path<(String, String, i32)>,
    headers: HeaderMap,
    uri: Uri,
) -> ResultPage<Response> {
    let mut ctx = PageContext::load(state, headers, &uri).await?;
    let kind = kind(&uri);
    let repo = repository(&ctx, &owner, &name).await?;
    let value = match item(&ctx, &repo, kind, number).await {
        Ok(value) => value,
        // Issues and pull requests share one number sequence, and a pull request's page is under
        // `/pulls`. Links to `/issues/N` reach it through this redirect, which keeps the fragment.
        Err(response) if kind == Kind::Issue && response.status() == StatusCode::NOT_FOUND => {
            if let Some(moved) = codeyard_db_repos::issues::management::redirect(
                &ctx.state.pool,
                repo.record.id,
                number,
            )
            .await
            .map_err(|_| unavailable(&ctx))?
            {
                let target = codeyard_db_repos::repositories::records::get_repository_by_id(
                    &ctx.state.pool,
                    moved.repo_id,
                )
                .await
                .map_err(|_| unavailable(&ctx))?
                .ok_or_else(|| ctx.error(StatusCode::NOT_FOUND, "Issue not found."))?;
                if !codeyard_identity::repository_access::can_read(
                    &ctx.state.pool,
                    ctx.user.as_ref(),
                    &target,
                )
                .await
                .map_err(|_| unavailable(&ctx))?
                {
                    return Err(response);
                }
                let owner = codeyard_db_repos::repositories::records::repository_owner_name(
                    &ctx.state.pool,
                    target.id,
                )
                .await
                .map_err(|_| unavailable(&ctx))?
                .ok_or_else(|| unavailable(&ctx))?;
                // The comment and the page the old link asked for still apply at the new home.
                let comment = ctx
                    .query
                    .get("comment")
                    .and_then(|id| id.parse::<i64>().ok())
                    .filter(|id| *id > 0);
                let page = ctx
                    .query
                    .get("page")
                    .and_then(|page| page.parse::<usize>().ok())
                    .filter(|page| (2..=crate::backend::issues::MAX_COMMENT_PAGE).contains(page));
                let suffix = match (comment, page) {
                    (Some(id), _) => format!("?comment={id}#comment-{id}"),
                    (None, Some(page)) => format!("?page={page}"),
                    (None, None) => String::new(),
                };
                return Ok(ctx.redirect(&format!(
                    "/{owner}/{}/issues/{}{suffix}",
                    target.name, moved.number
                )));
            }
            let pull_request = codeyard_db_repos::pull_requests::records::get_pull_request(
                &ctx.state.pool,
                repo.record.id,
                number,
            )
            .await
            .map_err(|_| unavailable(&ctx))?;
            let Some(pull_request) = pull_request else {
                return Err(response);
            };
            let mut location = format!("{}/{}", repo.view.family_url(Kind::Pull), number);
            if let Some(source) = ctx.query.get("comment") {
                let source = source
                    .parse::<i64>()
                    .ok()
                    .filter(|id| *id > 0)
                    .ok_or_else(|| ctx.error(StatusCode::BAD_REQUEST, "Invalid comment."))?;
                if let Some(id) =
                    codeyard_db_repos::pull_requests::reviews::converted_issue_comment(
                        &ctx.state.pool,
                        pull_request.id,
                        source,
                    )
                    .await
                    .map_err(|_| unavailable(&ctx))?
                {
                    location.push_str(&format!("?comment={id}#comment-{id}"));
                    return Ok(ctx.redirect(&location));
                }
            }
            let page = ctx
                .query
                .get("page")
                .and_then(|page| page.parse::<usize>().ok())
                .filter(|page| (2..=crate::backend::issues::MAX_COMMENT_PAGE).contains(page));
            if let Some(page) = page {
                location.push_str(&format!("?page={page}"));
            }
            return Ok(ctx.redirect(&location));
        }
        Err(response) => return Err(response),
    };
    if kind == Kind::Issue {
        if let Some(id) = ctx.query.get("comment") {
            let id = id
                .parse::<i64>()
                .map_err(|_| ctx.error(StatusCode::BAD_REQUEST, "Invalid comment."))?;
            let issue =
                codeyard_db_repos::issues::get_issue(&ctx.state.pool, repo.record.id, number)
                    .await
                    .map_err(|_| unavailable(&ctx))?
                    .ok_or_else(|| ctx.error(StatusCode::NOT_FOUND, "Issue not found."))?;
            let comment = codeyard_db_repos::issues::get_issue_comment(&ctx.state.pool, id)
                .await
                .map_err(|_| unavailable(&ctx))?
                .filter(|comment| comment.issue_id == issue.id)
                .ok_or_else(|| ctx.error(StatusCode::NOT_FOUND, "Comment not found."))?;
            let position =
                codeyard_db_repos::issues::issue_comment_position(&ctx.state.pool, comment.id)
                    .await
                    .map_err(|_| unavailable(&ctx))?;
            ctx.query.insert(
                "page".into(),
                crate::backend::issues::discussion_page(Ok(position)).to_string(),
            );
        }
    }
    // A pull request comment link names the comment; the page and review offset come from where
    // the comment is now.
    if kind == Kind::Pull {
        if let Some(id) = ctx.query.get("comment") {
            let id = id
                .parse::<i64>()
                .map_err(|_| ctx.error(StatusCode::BAD_REQUEST, "Invalid comment."))?;
            let pull = codeyard_db_repos::pull_requests::records::get_pull_request(
                &ctx.state.pool,
                repo.record.id,
                number,
            )
            .await
            .map_err(|_| unavailable(&ctx))?
            .ok_or_else(|| ctx.error(StatusCode::NOT_FOUND, "Pull request not found."))?;
            let comment =
                codeyard_db_repos::pull_requests::reviews::get_pull_request_comment_by_id(
                    &ctx.state.pool,
                    id,
                )
                .await
                .map_err(|_| unavailable(&ctx))?
                .filter(|comment| comment.pull_request_id == pull.id)
                .ok_or_else(|| ctx.error(StatusCode::NOT_FOUND, "Comment not found."))?;
            let base = format!("{}/{}", repo.view.family_url(Kind::Pull), number);
            let location =
                crate::backend::repositories::pull_requests::comments::saved_comment_link(
                    &ctx.state.pool,
                    &base,
                    &comment,
                )
                .await;
            return Ok(ctx.redirect(&location));
        }
        // A review link names the review the same way.
        if let Some(id) = ctx.query.get("review") {
            let id = id
                .parse::<i64>()
                .map_err(|_| ctx.error(StatusCode::BAD_REQUEST, "Invalid review."))?;
            let pull = codeyard_db_repos::pull_requests::records::get_pull_request(
                &ctx.state.pool,
                repo.record.id,
                number,
            )
            .await
            .map_err(|_| unavailable(&ctx))?
            .ok_or_else(|| ctx.error(StatusCode::NOT_FOUND, "Pull request not found."))?;
            let review = codeyard_db_repos::pull_requests::reviews::get_pull_request_review_by_id(
                &ctx.state.pool,
                id,
            )
            .await
            .map_err(|_| unavailable(&ctx))?
            .filter(|review| review.pull_request_id == pull.id && review.state != "pending")
            .ok_or_else(|| ctx.error(StatusCode::NOT_FOUND, "Review not found."))?;
            let page = crate::backend::issues::discussion_page(
                codeyard_db_repos::pull_requests::reviews::count_discussion_items_through(
                    &ctx.state.pool,
                    pull.id,
                    codeyard_db_repos::pull_requests::reviews::DiscussionRef::Review(review.id),
                )
                .await,
            );
            let base = format!("{}/{}", repo.view.family_url(Kind::Pull), number);
            let location = crate::backend::issues::discussion_link(
                &base,
                page,
                &format!("review-{}", review.id),
            );
            return Ok(ctx.redirect(&location));
        }
    }
    render_detail(
        &ctx,
        &repo,
        kind,
        &value,
        &DetailDraft::default(),
        None,
        StatusCode::OK,
    )
    .await
}

pub(crate) async fn native_edit(
    State(state): State<Arc<AppState>>,
    Path((owner, name, number)): Path<(String, String, i32)>,
    headers: HeaderMap,
    uri: Uri,
    Form(form): Form<HashMap<String, String>>,
) -> ResultPage<Response> {
    let ctx = PageContext::load(state, headers, &uri).await?;
    authorize_post(&ctx, &form)?;
    let kind = kind(&uri);
    let repo = repository(&ctx, &owner, &name).await?;
    let value = item(&ctx, &repo, kind, number).await?;
    let draft = submitted_detail(&form, "edit");
    let title = field(&form, "title");
    let body = field(&form, "body");
    if !value.can_edit {
        return Err(ctx.error(
            StatusCode::FORBIDDEN,
            "Only the author or a repository writer may edit this entry.",
        ));
    }
    if let Err(message) = expected(&form, &value) {
        return render_detail(
            &ctx,
            &repo,
            kind,
            &value,
            &draft,
            Some(message),
            StatusCode::CONFLICT,
        )
        .await;
    }
    if let Err(message) = codeyard_model::issue_input::validate_title(&title)
        .and_then(|_| codeyard_model::issue_input::validate_body(&body, false))
    {
        return render_detail(
            &ctx,
            &repo,
            kind,
            &value,
            &draft,
            Some(message),
            StatusCode::UNPROCESSABLE_ENTITY,
        )
        .await;
    }
    if draft.preview {
        return render_detail(&ctx, &repo, kind, &value, &draft, None, StatusCode::OK).await;
    }
    let result = save_entry_change(
        &ctx,
        kind,
        ctx.require_user()?.id,
        (repo.record.id, number),
        EntryChange {
            title: Some(title),
            body: Some(body),
            state: None,
            expected_state: field(&form, "expected_state"),
            expected_updated: field(&form, "expected_updated"),
        },
    )
    .await;
    match result {
        Ok(()) => Ok(ctx.redirect(&format!("{}/{}", repo.view.family_url(kind), number))),
        Err(failure) => {
            let (status, message) = safe_api_error(&failure);
            render_detail(&ctx, &repo, kind, &value, &draft, Some(&message), status).await
        }
    }
}

/// What an edit changes: the title and body, or the state, with the revision they were read at.
/// The update rules check the writer and the revision.
pub(crate) struct EntryChange {
    pub title: Option<String>,
    pub body: Option<String>,
    pub state: Option<String>,
    pub expected_state: String,
    pub expected_updated: String,
}

/// Saves a change to an issue or pull request through the same update the edit form uses.
pub(crate) async fn save_entry_change(
    ctx: &PageContext,
    kind: Kind,
    actor_id: i64,
    (repository_id, number): (i64, i32),
    text: EntryChange,
) -> Result<(), ApiFailure> {
    match kind {
        Kind::Issue => crate::backend::issues::mutations::update_issue(
            &ctx.state.pool,
            &ctx.state.notification_broadcaster,
            actor_id,
            repository_id,
            number,
            crate::backend::issues::mutations::UpdateIssueRequest {
                expected_state: text.expected_state,
                expected_updated: text.expected_updated,
                title: text.title,
                body: text.body,
                state: text.state,
                state_reason: None,
            },
        )
        .await
        .map_err(crate::backend::api::repos::issues::operation_error)
        .map(|_| ()),
        Kind::Pull => crate::backend::repositories::pull_requests::mutations::update_pull_request(
            &ctx.state.pool,
            &ctx.state.ci_runs,
            &ctx.state.notification_broadcaster,
            actor_id,
            (repository_id, number),
            crate::backend::repositories::pull_requests::mutations::UpdatePullRequestRequest {
                expected_state: text.expected_state,
                expected_updated: text.expected_updated,
                title: text.title,
                body: text.body,
                state: text.state,
                base: None,
                maintainer_can_modify: None,
            },
        )
        .await
        .map_err(crate::backend::api::repos::pulls::operation_error)
        .map(|_| ()),
    }
}

pub(crate) async fn native_change_state(
    State(state): State<Arc<AppState>>,
    Path((owner, name, number)): Path<(String, String, i32)>,
    headers: HeaderMap,
    uri: Uri,
    Form(form): Form<HashMap<String, String>>,
) -> ResultPage<Response> {
    let ctx = PageContext::load(state, headers, &uri).await?;
    authorize_post(&ctx, &form)?;
    let kind = kind(&uri);
    let repo = repository(&ctx, &owner, &name).await?;
    let value = item(&ctx, &repo, kind, number).await?;
    let target = field(&form, "state");
    let draft = DetailDraft::default();
    if let Err(message) = expected(&form, &value) {
        return render_detail(
            &ctx,
            &repo,
            kind,
            &value,
            &draft,
            Some(message),
            StatusCode::CONFLICT,
        )
        .await;
    }
    if !matches!(
        (value.state.as_str(), target.as_str()),
        ("open", "closed") | ("closed", "open")
    ) {
        return render_detail(
            &ctx,
            &repo,
            kind,
            &value,
            &draft,
            Some("Only open entries can be closed and closed entries reopened."),
            StatusCode::CONFLICT,
        )
        .await;
    }
    let result = save_entry_change(
        &ctx,
        kind,
        ctx.require_user()?.id,
        (repo.record.id, number),
        EntryChange {
            title: None,
            body: None,
            state: Some(target),
            expected_state: field(&form, "expected_state"),
            expected_updated: field(&form, "expected_updated"),
        },
    )
    .await;
    match result {
        Ok(()) => Ok(ctx.redirect(&format!("{}/{}", repo.view.family_url(kind), number))),
        Err(failure) => {
            let (status, message) = safe_api_error(&failure);
            render_detail(&ctx, &repo, kind, &value, &draft, Some(&message), status).await
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use codeyard_webui::pages::repos::issues as view;
    fn value() -> view::Item {
        view::Item {
            id: 1,
            number: 1,
            stacked: false,
            stack: None,
            stack_candidate: None,
            assignees: Vec::new(),
            milestone: None,
            title: "title".into(),
            body: "body".into(),
            state: "open".into(),
            author: "alice".into(),
            created: "now".into(),
            updated: "revision-2".into(),
            labels: vec![],
            comparison: None,
            comment_count: 0,
            can_edit: false,
            can_tick: false,
            tasks: Default::default(),
            commit_count: None,
            file_count: None,
            check_count: 0,
            review_state: None,
            check_state: None,
            projects: vec![],
            reactions: Vec::new(),
        }
    }
    #[test]
    fn stale_or_missing_expected_state_never_matches() {
        let mut form = HashMap::new();
        assert!(expected(&form, &value()).is_err());
        form.insert("expected_state".into(), "open".into());
        form.insert("expected_updated".into(), "revision-1".into());
        assert!(expected(&form, &value()).is_err());
        form.insert("expected_updated".into(), "revision-2".into());
        assert!(expected(&form, &value()).is_ok());
        form.insert("expected_state".into(), "closed".into());
        assert!(expected(&form, &value()).is_err());
    }
    #[test]
    fn submitted_text_is_preserved_without_trimming() {
        let form = HashMap::from([
            ("title".into(), "  text & <tag>  ".into()),
            ("body".into(), "\n</textarea>\n".into()),
            ("head".into(), "topic/a".into()),
            ("base".into(), "main".into()),
            ("draft".into(), "on".into()),
        ]);
        let draft = submitted_new(&form);
        assert_eq!(draft.title, "  text & <tag>  ");
        assert_eq!(draft.body, "\n</textarea>\n");
        assert_eq!(draft.head, "topic/a");
        assert!(draft.draft);
        assert_eq!(submitted_detail(&form, "comment").comment, draft.body);
    }
    #[test]
    fn internal_and_git_diagnostics_do_not_reach_html_errors() {
        for (status, code) in [
            (StatusCode::INTERNAL_SERVER_ERROR, "database_error"),
            (StatusCode::CONFLICT, "merge_conflict"),
            (StatusCode::BAD_REQUEST, "git_error"),
        ] {
            let failure = (
                status,
                Json(ApiError::new(
                    code,
                    "postgres://secret@private/internal/repo.git",
                )),
            );
            let (rendered_status, message) = safe_api_error(&failure);
            assert_eq!(rendered_status, status);
            assert!(!message.contains("secret"));
            assert!(!message.contains("/internal/"));
        }
    }
    #[test]
    fn validation_and_branch_protection_failures_remain_meaningful() {
        let failure = (
            StatusCode::CONFLICT,
            Json(ApiError::new(
                "branch_protection",
                "2 approving reviews required, found 0",
            )),
        );
        assert!(safe_api_error(&failure).1.contains("2 approving review"));
    }
}
