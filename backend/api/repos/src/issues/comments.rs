use crate::access::readable_repository;
use crate::discussion::item;
use crate::discussion::kind;
use crate::discussion::render_detail;
use crate::discussion::repository;
use crate::discussion::safe_api_error;
use crate::discussion::submitted_detail;
use crate::discussion::ResultPage;
use crate::paging::json_page;
use crate::paging::PageQuery;
use crate::responses::storage_unavailable;
use axum::extract::Form;
use axum::extract::Path;
use axum::extract::Query;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::http::StatusCode;
use axum::http::Uri;
use axum::response::Response;
use axum::Json;
use codeyard_api_http::context::PageContext;
use codeyard_api_http::error::ApiError;
use codeyard_api_http::session::get_current_user;
use codeyard_api_http::shared::repos::forms::authorize_post;
use codeyard_api_http::shared::user::responses::PublicUserResponse as UserResponse;
use codeyard_app::app::AppState;
use codeyard_repositories::issues::comment_link;
use codeyard_repositories::issues::comments::CreateCommentRequest;
use codeyard_webui::pages::repos::issues::DetailDraft;
use serde::Serialize;
use std::collections::HashMap;
use std::sync::Arc;

pub async fn create_issue_comment(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, repo, number)): Path<(String, String, i32)>,
    Json(req): Json<CreateCommentRequest>,
) -> Result<(StatusCode, Json<CommentResponse>), (StatusCode, Json<ApiError>)> {
    let current_actor = get_current_user(&state.pool, &headers).await?;
    let session = codeyard_api_http::session::extract_session_id(&headers).unwrap_or_default();
    let comment = codeyard_repositories::issues::comments::create_issue_comment(
        &state.pool,
        &state.notification_broadcaster,
        current_actor.id,
        &codeyard_database::queries::users::accounts::AccountCredential::Session(&session),
        owner,
        repo,
        number,
        req,
    )
    .await
    .map_err(crate::issues::operation_error)?;

    Ok((
        StatusCode::CREATED,
        Json(CommentResponse::new(
            comment,
            UserResponse::from(current_actor),
        )),
    ))
}

#[derive(Serialize)]
pub struct CommentResponse {
    pub id: i64,
    pub issue_id: i64,
    pub author_id: i64,
    pub author: UserResponse,
    pub body: String,
    pub created_at: String,
    pub updated_at: String,
}

impl CommentResponse {
    fn new(comment: codeyard_database::models::IssueComment, author: UserResponse) -> Self {
        Self {
            id: comment.id,
            issue_id: comment.issue_id,
            author_id: comment.author_id,
            author,
            body: comment.body,
            created_at: comment.created_at.to_rfc3339(),
            updated_at: comment.updated_at.to_rfc3339(),
        }
    }
}

pub(crate) async fn list_issue_comments(
    State(state): State<Arc<AppState>>,
    Path((owner, repo, number)): Path<(String, String, i32)>,
    Query(query): Query<PageQuery>,
    headers: HeaderMap,
) -> Result<Json<Vec<CommentResponse>>, (StatusCode, Json<ApiError>)> {
    let repository = readable_repository(&state.pool, &owner, &repo, &headers).await?;
    let (offset, limit) = json_page(query.page, query.per_page)?;

    let issue = codeyard_db_repos::issues::get_issue(&state.pool, repository.id, number)
        .await
        .map_err(|_| storage_unavailable())?
        .ok_or_else(|| {
            (
                StatusCode::NOT_FOUND,
                Json(ApiError::new("not_found", "Issue not found")),
            )
        })?;

    let comments = codeyard_db_repos::issues::list_issue_comments(
        &state.pool,
        issue.id,
        limit as i64,
        offset as i64,
    )
    .await
    .map_err(|_| storage_unavailable())?;

    let mut responses = Vec::with_capacity(comments.len());
    for entry in comments {
        let author = UserResponse::from_display(&entry.author);
        responses.push(CommentResponse::new(entry.comment, author));
    }

    codeyard_db_repos::issues::get_issue_by_id(&state.pool, issue.id)
        .await
        .map_err(|_| storage_unavailable())?
        .filter(|issue| issue.repo_id == repository.id)
        .ok_or_else(|| {
            (
                StatusCode::NOT_FOUND,
                Json(ApiError::new("not_found", "Issue not found")),
            )
        })?;
    codeyard_api_http::shared::repos::access::check_repo_read_access(
        &state.pool,
        &repository,
        &headers,
    )
    .await?;
    Ok(Json(responses))
}

pub(crate) async fn native_comment(
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
    let draft = submitted_detail(&form, "comment");
    if form
        .get("repository_id")
        .and_then(|id| id.parse::<i64>().ok())
        != Some(repo.record.id)
        || form.get("issue_id").and_then(|id| id.parse::<i64>().ok()) != Some(value.id)
    {
        return render_detail(
            &ctx,
            &repo,
            kind,
            &value,
            &draft,
            Some("The comment target changed. Your draft is preserved; reload before submitting."),
            StatusCode::CONFLICT,
        )
        .await;
    }
    if let Err(message) = codeyard_types::issue_input::validate_body(&draft.comment, true) {
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
    let result = codeyard_repositories::issues::comments::create_issue_comment(
        &ctx.state.pool,
        &ctx.state.notification_broadcaster,
        ctx.require_user()?.id,
        &codeyard_database::queries::users::accounts::AccountCredential::Session(
            &codeyard_api_http::session::extract_session_id(&ctx.headers).unwrap_or_default(),
        ),
        owner,
        name,
        number,
        codeyard_repositories::issues::comments::CreateCommentRequest {
            body: draft.comment.clone(),
            repository_id: Some(repo.record.id),
            issue_id: Some(value.id),
        },
    )
    .await
    .map_err(crate::issues::operation_error)
    .map(|c| c.id);
    match result {
        Ok(id) => {
            let position =
                codeyard_db_repos::issues::issue_comment_position(&ctx.state.pool, id).await;
            let base = format!("{}/{}", repo.view.family_url(kind), number);
            let location = comment_link(&base, id, position);
            Ok(ctx.redirect(&location))
        }
        Err(failure) => {
            let (status, message) = safe_api_error(&failure);
            render_detail(&ctx, &repo, kind, &value, &draft, Some(&message), status).await
        }
    }
}

/// Changes the author's comment from the issue page. A preview shows the page again with the text.
pub(crate) async fn native_edit_comment(
    State(state): State<Arc<AppState>>,
    Path((owner, name, number, comment_id)): Path<(String, String, i32, i64)>,
    headers: HeaderMap,
    uri: Uri,
    Form(form): Form<HashMap<String, String>>,
) -> ResultPage<Response> {
    let ctx = PageContext::load(state, headers, &uri).await?;
    authorize_post(&ctx, &form)?;
    let kind = kind(&uri);
    let repo = repository(&ctx, &owner, &name).await?;
    let load = || item(&ctx, &repo, kind, number);
    let mut draft = submitted_detail(&form, "comment");
    draft.editing = Some(comment_id);
    let expected_body = serde_json::from_str::<String>(&draft.expected_body).ok();
    if expected_body.is_none() {
        return render_detail(
            &ctx,
            &repo,
            kind,
            &load().await?,
            &draft,
            Some("The original comment is required. Reload before saving."),
            StatusCode::CONFLICT,
        )
        .await;
    }
    if draft.preview {
        return render_detail(
            &ctx,
            &repo,
            kind,
            &load().await?,
            &draft,
            None,
            StatusCode::OK,
        )
        .await;
    }
    let result = codeyard_repositories::issues::comments::edit_issue_comment(
        &ctx.state.pool,
        &ctx.state.notification_broadcaster,
        ctx.require_user()?.id,
        &codeyard_database::queries::users::accounts::AccountCredential::Session(
            &codeyard_api_http::session::extract_session_id(&ctx.headers).unwrap_or_default(),
        ),
        codeyard_repositories::issues::comments::CommentTarget {
            owner,
            repo: name,
            number,
            comment_id,
        },
        codeyard_repositories::issues::comments::CreateCommentRequest {
            body: draft.comment.clone(),
            ..Default::default()
        },
        expected_body.as_deref(),
    )
    .await
    .map_err(crate::issues::operation_error)
    .map(|c| c.id);
    match result {
        Ok(id) => {
            let position =
                codeyard_db_repos::issues::issue_comment_position(&ctx.state.pool, id).await;
            let base = format!("{}/{}", repo.view.family_url(kind), number);
            Ok(ctx.redirect(&comment_link(&base, id, position)))
        }
        Err(failure) => {
            let (status, message) = safe_api_error(&failure);
            render_detail(
                &ctx,
                &repo,
                kind,
                &load().await?,
                &draft,
                Some(&message),
                status,
            )
            .await
        }
    }
}

/// Deletes a comment from the issue page, after the confirmation dialog.
pub(crate) async fn native_delete_comment(
    State(state): State<Arc<AppState>>,
    Path((owner, name, number, comment_id)): Path<(String, String, i32, i64)>,
    headers: HeaderMap,
    uri: Uri,
    Form(form): Form<HashMap<String, String>>,
) -> ResultPage<Response> {
    let ctx = PageContext::load(state, headers, &uri).await?;
    authorize_post(&ctx, &form)?;
    let kind = kind(&uri);
    let repo = repository(&ctx, &owner, &name).await?;
    let result = codeyard_repositories::issues::comments::delete_issue_comment(
        &ctx.state.pool,
        ctx.require_user()?.id,
        &codeyard_database::queries::users::accounts::AccountCredential::Session(
            &codeyard_api_http::session::extract_session_id(&ctx.headers).unwrap_or_default(),
        ),
        codeyard_repositories::issues::comments::CommentTarget {
            owner,
            repo: name,
            number,
            comment_id,
        },
    )
    .await
    .map_err(crate::issues::operation_error);
    match result {
        Ok(()) => Ok(ctx.redirect(&format!("{}/{}", repo.view.family_url(kind), number))),
        Err(failure) => {
            let (status, message) = safe_api_error(&failure);
            let value = item(&ctx, &repo, kind, number).await?;
            render_detail(
                &ctx,
                &repo,
                kind,
                &value,
                &DetailDraft::default(),
                Some(&message),
                status,
            )
            .await
        }
    }
}
