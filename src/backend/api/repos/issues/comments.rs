use crate::backend::api::http::context::PageContext;
use crate::backend::api::http::error::ApiError;
use crate::backend::api::http::session::get_current_user;
use crate::backend::api::repos::access::check_repo_read_access;
use crate::backend::api::repos::chrome::{json_page, PageQuery};
use crate::backend::api::repos::pulls::pull::*;
use crate::backend::api::user::responses::PublicUserResponse as UserResponse;
use crate::backend::app::AppState;
use crate::backend::db;
use crate::backend::issues::comment_link;
use crate::backend::issues::comments::CreateCommentRequest;
use crate::webui::pages::repos::issues::DetailDraft;
use axum::extract::Form;
use axum::extract::Path;
use axum::extract::Query;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::http::StatusCode;
use axum::http::Uri;
use axum::response::Response;
use axum::Json;
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
    let comment = crate::backend::issues::comments::create_issue_comment(
        &state.pool,
        &state.notification_broadcaster,
        current_actor.id,
        owner,
        repo,
        number,
        req,
    )
    .await
    .map_err(super::operation_error)?;
    let user = current_actor;

    Ok((
        StatusCode::CREATED,
        Json(CommentResponse {
            id: comment.id,
            issue_id: comment.issue_id,
            author_id: comment.author_id,
            author: UserResponse::from(user),
            body: comment.body,
            created_at: comment.created_at.to_rfc3339(),
            updated_at: comment.updated_at.to_rfc3339(),
        }),
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
pub async fn list_issue_comments(
    State(state): State<Arc<AppState>>,
    Path((owner, repo, number)): Path<(String, String, i32)>,
    Query(query): Query<PageQuery>,
    headers: HeaderMap,
) -> Result<Json<Vec<CommentResponse>>, (StatusCode, Json<ApiError>)> {
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
    let (offset, limit) = json_page(query.page, query.per_page)?;

    let issue = db::issues::get_issue(&state.pool, repository.id, number)
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
                Json(ApiError::new("not_found", "Issue not found")),
            )
        })?;

    let comments =
        db::issues::list_issue_comments(&state.pool, issue.id, limit as i64, offset as i64)
            .await
            .map_err(|_| {
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(ApiError::new(
                        "database_error",
                        "Collaboration storage is temporarily unavailable",
                    )),
                )
            })?;

    let mut responses = Vec::with_capacity(comments.len());
    for entry in comments {
        let author = UserResponse::from_display(&entry.author);
        let comment = entry.comment;
        responses.push(CommentResponse {
            id: comment.id,
            issue_id: comment.issue_id,
            author_id: comment.author_id,
            author,
            body: comment.body,
            created_at: comment.created_at.to_rfc3339(),
            updated_at: comment.updated_at.to_rfc3339(),
        });
    }

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
    if let Err(message) = crate::backend::issues::validate_body(&draft.comment, true) {
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
    let result = crate::backend::issues::comments::create_issue_comment(
        &ctx.state.pool,
        &ctx.state.notification_broadcaster,
        ctx.require_user()?.id,
        owner,
        name,
        number,
        crate::backend::issues::comments::CreateCommentRequest {
            body: draft.comment.clone(),
        },
    )
    .await
    .map_err(crate::backend::api::repos::issues::operation_error)
    .map(|c| c.id);
    match result {
        Ok(id) => {
            let position = db::issues::issue_comment_position(&ctx.state.pool, id).await;
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
    let value = item(&ctx, &repo, kind, number).await?;
    let mut draft = submitted_detail(&form, "comment");
    draft.editing = Some(comment_id);
    if draft.preview {
        return render_detail(&ctx, &repo, kind, &value, &draft, None, StatusCode::OK).await;
    }
    let result = crate::backend::issues::comments::edit_issue_comment(
        &ctx.state.pool,
        &ctx.state.notification_broadcaster,
        ctx.require_user()?.id,
        crate::backend::issues::comments::CommentTarget {
            owner,
            repo: name,
            number,
            comment_id,
        },
        crate::backend::issues::comments::CreateCommentRequest {
            body: draft.comment.clone(),
        },
    )
    .await
    .map_err(crate::backend::api::repos::issues::operation_error)
    .map(|c| c.id);
    match result {
        Ok(id) => {
            let position = db::issues::issue_comment_position(&ctx.state.pool, id).await;
            let base = format!("{}/{}", repo.view.family_url(kind), number);
            Ok(ctx.redirect(&comment_link(&base, id, position)))
        }
        Err(failure) => {
            let (status, message) = safe_api_error(&failure);
            render_detail(&ctx, &repo, kind, &value, &draft, Some(&message), status).await
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
    let value = item(&ctx, &repo, kind, number).await?;
    let result = crate::backend::issues::comments::delete_issue_comment(
        &ctx.state.pool,
        ctx.require_user()?.id,
        crate::backend::issues::comments::CommentTarget {
            owner,
            repo: name,
            number,
            comment_id,
        },
    )
    .await
    .map_err(crate::backend::api::repos::issues::operation_error);
    match result {
        Ok(()) => Ok(ctx.redirect(&format!("{}/{}", repo.view.family_url(kind), number))),
        Err(failure) => {
            let (status, message) = safe_api_error(&failure);
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
