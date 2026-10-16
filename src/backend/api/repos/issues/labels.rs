use crate::backend::api::context::PageContext;
use crate::backend::api::error::ApiError;
use crate::backend::api::repos::access::check_repo_admin_access;
use crate::backend::api::repos::issues::assignees::assigned_item;
use crate::backend::api::repos::labels::label_write_error;
use crate::backend::api::repos::labels::LabelResponse;
use crate::backend::api::repos::pulls::pull::*;
use crate::backend::api::session::get_current_user;
use crate::backend::app::AppState;
use crate::backend::db;
use crate::backend::db::assignees::AssignedItem;
use crate::webui::pages::collaboration::DetailDraft;
use axum::extract::Form;
use axum::extract::Path;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::http::StatusCode;
use axum::http::Uri;
use axum::response::Response;
use axum::Json;
use serde::Deserialize;
use std::collections::HashMap;
use std::sync::Arc;
#[derive(Deserialize)]
pub struct AddLabelsRequest {
    pub labels: Vec<i64>,
}

pub async fn add_issue_labels(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, repo, number)): Path<(String, String, i32)>,
    Json(req): Json<AddLabelsRequest>,
) -> Result<Json<Vec<LabelResponse>>, (StatusCode, Json<ApiError>)> {
    let repository = db::repositories::records::get_repository(&state.pool, &owner, &repo)
        .await
        .map_err(label_write_error)?
        .ok_or_else(|| {
            (
                StatusCode::NOT_FOUND,
                Json(ApiError::new("not_found", "Repository not found")),
            )
        })?;

    check_repo_admin_access(&state.pool, &repository, &headers).await?;
    let actor = get_current_user(&state.pool, &headers).await?;
    let issue = db::issues::get_issue(&state.pool, repository.id, number)
        .await
        .map_err(label_write_error)?
        .ok_or_else(|| {
            (
                StatusCode::NOT_FOUND,
                Json(ApiError::new("not_found", "Issue not found")),
            )
        })?;

    let labels =
        db::labels::add_issue_labels(&state.pool, actor.id, repository.id, issue.id, &req.labels)
            .await
            .map_err(label_write_error)?;

    Ok(Json(labels.into_iter().map(LabelResponse::from).collect()))
}

pub async fn remove_issue_label(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, repo, number, label_id)): Path<(String, String, i32, i64)>,
) -> Result<StatusCode, (StatusCode, Json<ApiError>)> {
    let repository = db::repositories::records::get_repository(&state.pool, &owner, &repo)
        .await
        .map_err(label_write_error)?
        .ok_or_else(|| {
            (
                StatusCode::NOT_FOUND,
                Json(ApiError::new("not_found", "Repository not found")),
            )
        })?;

    check_repo_admin_access(&state.pool, &repository, &headers).await?;
    let actor = get_current_user(&state.pool, &headers).await?;
    let issue = db::issues::get_issue(&state.pool, repository.id, number)
        .await
        .map_err(label_write_error)?
        .ok_or_else(|| {
            (
                StatusCode::NOT_FOUND,
                Json(ApiError::new("not_found", "Issue not found")),
            )
        })?;

    db::labels::remove_issue_label(&state.pool, actor.id, repository.id, issue.id, label_id)
        .await
        .map_err(label_write_error)?;

    Ok(StatusCode::NO_CONTENT)
}

pub(crate) async fn native_add_label(
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
    let draft = submitted_detail(&form, "label");
    let labels = db::labels::list_repo_labels(&ctx.state.pool, repo.record.id)
        .await
        .map_err(|_| unavailable(&ctx))?;
    let selected = draft
        .label
        .parse::<i64>()
        .ok()
        .filter(|id| labels.iter().any(|l| l.id == *id));
    let Some(id) = selected else {
        return render_detail(
            &ctx,
            &repo,
            kind,
            &value,
            &draft,
            Some("Choose a label belonging to this repository."),
            StatusCode::UNPROCESSABLE_ENTITY,
        )
        .await;
    };
    let subject = assigned_item(&ctx, &repo, kind, number).await?;
    let actor = ctx.require_user()?.id;
    let applied = match subject {
        AssignedItem::Issue(item_id) => {
            db::labels::add_issue_labels(&ctx.state.pool, actor, repo.record.id, item_id, &[id])
                .await
        }
        AssignedItem::PullRequest(item_id) => {
            db::labels::add_pull_request_labels(
                &ctx.state.pool,
                actor,
                repo.record.id,
                item_id,
                &[id],
            )
            .await
        }
    }
    .map_err(crate::backend::api::repos::labels::label_write_error);
    match applied {
        Ok(applied) if applied.iter().any(|label| label.id == id) => {
            Ok(ctx.redirect(&format!("{}/{}", repo.view.family_url(kind), number)))
        }
        Ok(_) => {
            render_detail(
                &ctx,
                &repo,
                kind,
                &value,
                &draft,
                Some("The label could not be confirmed. Reload and choose an available label."),
                StatusCode::CONFLICT,
            )
            .await
        }
        Err(failure) => {
            let (status, message) = safe_api_error(&failure);
            render_detail(&ctx, &repo, kind, &value, &draft, Some(&message), status).await
        }
    }
}

pub(crate) async fn native_remove_label(
    State(state): State<Arc<AppState>>,
    Path((owner, name, number, label_id)): Path<(String, String, i32, i64)>,
    headers: HeaderMap,
    uri: Uri,
    Form(form): Form<HashMap<String, String>>,
) -> ResultPage<Response> {
    let ctx = PageContext::load(state, headers, &uri).await?;
    authorize_post(&ctx, &form)?;
    let kind = kind(&uri);
    let repo = repository(&ctx, &owner, &name).await?;
    let value = item(&ctx, &repo, kind, number).await?;
    if !value.labels.iter().any(|l| l.id == label_id) {
        return Err(ctx.error(
            StatusCode::CONFLICT,
            "This label is no longer applied to the item.",
        ));
    }
    let subject = assigned_item(&ctx, &repo, kind, number).await?;
    let actor = ctx.require_user()?.id;
    let removed = match subject {
        AssignedItem::Issue(item_id) => {
            db::labels::remove_issue_label(
                &ctx.state.pool,
                actor,
                repo.record.id,
                item_id,
                label_id,
            )
            .await
        }
        AssignedItem::PullRequest(item_id) => {
            db::labels::remove_pull_request_label(
                &ctx.state.pool,
                actor,
                repo.record.id,
                item_id,
                label_id,
            )
            .await
        }
    }
    .map_err(crate::backend::api::repos::labels::label_write_error);
    match removed {
        Ok(_) => Ok(ctx.redirect(&format!("{}/{}", repo.view.family_url(kind), number))),
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
