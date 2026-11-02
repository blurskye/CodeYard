use crate::backend::api::http::context::PageContext;
use crate::backend::api::http::error::ApiError;
use crate::backend::api::http::session::get_current_user;
use crate::backend::api::repos::access::{check_repo_admin_access, find_repository};
use crate::backend::api::repos::discussion::*;
use crate::backend::api::repos::forms::authorize_post;
use crate::backend::api::repos::issues::assignees::assigned_item;
use crate::backend::api::repos::labels::label_write_error;
use crate::backend::api::repos::labels::LabelResponse;
use crate::backend::app::AppState;
use crate::backend::db;
use crate::backend::db::assignees::AssignedItem;
use crate::webui::pages::repos::issues::DetailDraft;
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
    let actor = get_current_user(&state.pool, &headers).await?;
    let repository = find_repository(&state.pool, &owner, &repo).await?;
    check_repo_admin_access(&state.pool, &repository, &actor).await?;
    let issue = db::issues::get_issue(&state.pool, repository.id, number)
        .await
        .map_err(label_write_error)?
        .ok_or_else(|| {
            (
                StatusCode::NOT_FOUND,
                Json(ApiError::new("not_found", "Issue not found")),
            )
        })?;

    let labels = add_labels(
        &state.pool,
        actor.id,
        repository.id,
        AssignedItem::Issue(issue.id),
        &req.labels,
    )
    .await
    .map_err(label_write_error)?;

    Ok(Json(labels.into_iter().map(LabelResponse::from).collect()))
}

pub async fn remove_issue_label(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, repo, number, label_id)): Path<(String, String, i32, i64)>,
) -> Result<StatusCode, (StatusCode, Json<ApiError>)> {
    let actor = get_current_user(&state.pool, &headers).await?;
    let repository = find_repository(&state.pool, &owner, &repo).await?;
    check_repo_admin_access(&state.pool, &repository, &actor).await?;
    let issue = db::issues::get_issue(&state.pool, repository.id, number)
        .await
        .map_err(label_write_error)?
        .ok_or_else(|| {
            (
                StatusCode::NOT_FOUND,
                Json(ApiError::new("not_found", "Issue not found")),
            )
        })?;

    remove_label(
        &state.pool,
        actor.id,
        repository.id,
        AssignedItem::Issue(issue.id),
        label_id,
    )
    .await
    .map_err(label_write_error)?;

    Ok(StatusCode::NO_CONTENT)
}

/// Adds labels to an issue or pull request and records a "labeled" event for each one that was
/// not on it yet. Returns every label the item has afterwards.
async fn add_labels(
    pool: &db::DbPool,
    actor_id: i64,
    repo_id: i64,
    subject: AssignedItem,
    ids: &[i64],
) -> anyhow::Result<Vec<db::models::Label>> {
    let before = item_labels(pool, subject).await?;
    let labels = match subject {
        AssignedItem::Issue(item_id) => {
            db::labels::add_issue_labels(pool, actor_id, repo_id, item_id, ids).await?
        }
        AssignedItem::PullRequest(item_id) => {
            db::labels::add_pull_request_labels(pool, actor_id, repo_id, item_id, ids).await?
        }
    };
    for label in labels
        .iter()
        .filter(|label| ids.contains(&label.id))
        .filter(|label| !before.iter().any(|old| old.id == label.id))
    {
        db::timeline::record_for(pool, repo_id, subject, actor_id, "labeled", &label.name).await;
    }
    Ok(labels)
}

/// Removes a label from an issue or pull request and records an "unlabeled" event when it was on
/// it. Returns whether it was.
async fn remove_label(
    pool: &db::DbPool,
    actor_id: i64,
    repo_id: i64,
    subject: AssignedItem,
    label_id: i64,
) -> anyhow::Result<bool> {
    let before = item_labels(pool, subject).await?;
    let removed = match subject {
        AssignedItem::Issue(item_id) => {
            db::labels::remove_issue_label(pool, actor_id, repo_id, item_id, label_id).await?
        }
        AssignedItem::PullRequest(item_id) => {
            db::labels::remove_pull_request_label(pool, actor_id, repo_id, item_id, label_id)
                .await?
        }
    };
    if let (true, Some(label)) = (removed, before.iter().find(|label| label.id == label_id)) {
        db::timeline::record_for(pool, repo_id, subject, actor_id, "unlabeled", &label.name).await;
    }
    Ok(removed)
}

async fn item_labels(
    pool: &db::DbPool,
    subject: AssignedItem,
) -> anyhow::Result<Vec<db::models::Label>> {
    match subject {
        AssignedItem::Issue(item_id) => db::labels::get_issue_labels(pool, item_id).await,
        AssignedItem::PullRequest(item_id) => {
            db::labels::get_pull_request_labels(pool, item_id).await
        }
    }
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
    let applied = add_labels(&ctx.state.pool, actor, repo.record.id, subject, &[id])
        .await
        .map_err(label_write_error);
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
    let removed = remove_label(&ctx.state.pool, actor, repo.record.id, subject, label_id)
        .await
        .map_err(label_write_error);
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
