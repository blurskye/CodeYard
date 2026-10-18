//! Assignee forms on issue and pull request pages. The database checks the write right and
//! which users may be assigned, so these handlers only resolve names and show the outcome.
use crate::backend::api::http::context::PageContext;
use crate::backend::api::http::error::ApiError;
use crate::backend::api::repos::pulls::pull::{
    authorize_post, detail_outcome, field, item, kind, repository, unavailable, ApiFailure,
    RepositoryData, ResultPage,
};
use crate::backend::app::AppState;
use crate::backend::db;
use crate::backend::db::assignees::{AssignedItem, AssignmentRefusal, MAX_ASSIGNEES};
use crate::backend::notifications::assignments::{notify_assignees, Assignment};
use crate::webui::pages::repos::issues::Kind;
use axum::extract::{Form, Path, State};
use axum::http::{HeaderMap, StatusCode, Uri};
use axum::response::Response;
use axum::Json;
use std::collections::HashMap;
use std::sync::Arc;

pub(crate) fn assignment_error(error: anyhow::Error) -> ApiFailure {
    let (status, code, message) = match error.downcast_ref::<AssignmentRefusal>() {
        Some(AssignmentRefusal::Denied) => (
            StatusCode::FORBIDDEN,
            "forbidden",
            "You cannot change the assignees of this repository.".to_owned(),
        ),
        Some(AssignmentRefusal::NotFound) => (
            StatusCode::NOT_FOUND,
            "not_found",
            "The requested resource was not found or you do not have access.".to_owned(),
        ),
        Some(AssignmentRefusal::Unassignable) => (
            StatusCode::UNPROCESSABLE_ENTITY,
            "unassignable",
            "Only the repository owner and collaborators can be assigned.".to_owned(),
        ),
        Some(AssignmentRefusal::TooMany) => (
            StatusCode::UNPROCESSABLE_ENTITY,
            "too_many_assignees",
            format!("An issue or pull request can have at most {MAX_ASSIGNEES} assignees."),
        ),
        None => (
            StatusCode::SERVICE_UNAVAILABLE,
            "database_unavailable",
            "Assignee storage unavailable".to_owned(),
        ),
    };
    (status, Json(ApiError::new(code, &message)))
}

/// The issue or pull request a form changes, found in this repository by its number.
pub(crate) async fn assigned_item(
    ctx: &PageContext,
    repo: &RepositoryData,
    kind: Kind,
    number: i32,
) -> ResultPage<AssignedItem> {
    match kind {
        Kind::Issue => db::issues::get_issue(&ctx.state.pool, repo.record.id, number)
            .await
            .map_err(|_| unavailable(ctx))?
            .map(|issue| AssignedItem::Issue(issue.id))
            .ok_or_else(|| ctx.error(StatusCode::NOT_FOUND, "Issue not found.")),
        Kind::Pull => {
            db::pull_requests::records::get_pull_request(&ctx.state.pool, repo.record.id, number)
                .await
                .map_err(|_| unavailable(ctx))?
                .map(|pull| AssignedItem::PullRequest(pull.id))
                .ok_or_else(|| ctx.error(StatusCode::NOT_FOUND, "Pull request not found."))
        }
    }
}

/// The user id of a username. A name that is no user matches nothing.
pub(crate) async fn user_id(ctx: &PageContext, username: &str) -> ResultPage<Option<i64>> {
    Ok(
        db::users::accounts::get_user_by_username(&ctx.state.pool, username)
            .await
            .map_err(|_| unavailable(ctx))?
            .map(|user| user.id),
    )
}

pub(crate) async fn native_assign(
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
    let subject = assigned_item(&ctx, &repo, kind, number).await?;
    let username = field(&form, "username");
    // The assignee, when this request newly assigned them.
    let outcome = match user_id(&ctx, username.trim()).await? {
        Some(assignee) => db::assignees::assign_user(
            &ctx.state.pool,
            ctx.require_user()?.id,
            repo.record.id,
            subject,
            assignee,
        )
        .await
        .map(|added| added.then_some(assignee))
        .map_err(assignment_error),
        None => Err(assignment_error(AssignmentRefusal::Unassignable.into())),
    };
    if outcome.is_ok() {
        db::timeline::record_for(
            &ctx.state.pool,
            repo.record.id,
            subject,
            ctx.require_user()?.id,
            "assigned",
            username.trim(),
        )
        .await;
    }
    if let Ok(Some(assignee)) = &outcome {
        let link = format!(
            "/{owner}/{name}/{}/{number}",
            if kind == Kind::Pull {
                "pulls"
            } else {
                "issues"
            }
        );
        notify_assignees(
            &ctx.state.pool,
            &ctx.state.notification_broadcaster,
            &Assignment {
                actor: ctx.require_user()?,
                repository: &repo.record,
                owner: &owner,
                repo: &name,
                number,
                pull_request: kind == Kind::Pull,
                title: &value.title,
                link: &link,
            },
            &[*assignee],
        )
        .await;
    }
    detail_outcome(&ctx, &repo, kind, &value, number, outcome.map(|_| ())).await
}

pub(crate) async fn native_unassign(
    State(state): State<Arc<AppState>>,
    Path((owner, name, number, username)): Path<(String, String, i32, String)>,
    headers: HeaderMap,
    uri: Uri,
    Form(form): Form<HashMap<String, String>>,
) -> ResultPage<Response> {
    let ctx = PageContext::load(state, headers, &uri).await?;
    authorize_post(&ctx, &form)?;
    let kind = kind(&uri);
    let repo = repository(&ctx, &owner, &name).await?;
    let value = item(&ctx, &repo, kind, number).await?;
    let subject = assigned_item(&ctx, &repo, kind, number).await?;
    // A name that is no user, or is not assigned, leaves the item as it is and records nothing.
    let outcome = db::assignees::unassign_user(
        &ctx.state.pool,
        ctx.require_user()?.id,
        repo.record.id,
        subject,
        &username,
    )
    .await
    .map_err(assignment_error);
    if let Ok(true) = outcome {
        db::timeline::record_for(
            &ctx.state.pool,
            repo.record.id,
            subject,
            ctx.require_user()?.id,
            "unassigned",
            &username,
        )
        .await;
    }
    detail_outcome(&ctx, &repo, kind, &value, number, outcome.map(|_| ())).await
}
