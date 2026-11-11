//! Assignee forms on issue and pull request pages. The database checks the write right and
//! which users may be assigned, so these handlers only resolve names and show the outcome.
use crate::discussion::detail_outcome;
use crate::discussion::item;
use crate::discussion::kind;
use crate::discussion::repository;
use crate::discussion::unavailable;
use crate::discussion::RepositoryData;
use crate::discussion::ResultPage;
use axum::extract::Form;
use axum::extract::Path;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::http::StatusCode;
use axum::http::Uri;
use axum::response::Response;
use axum::Json;
use codeyard_api_http::context::PageContext;
use codeyard_api_http::error::ApiError;
use codeyard_api_http::error::ApiFailure;
use codeyard_api_http::shared::repos::forms::authorize_post;
use codeyard_api_http::shared::repos::forms::field;
use codeyard_app::app::AppState;
use codeyard_db_repos::assignees::AssignedItem;
use codeyard_db_repos::assignees::AssignmentRefusal;
use codeyard_db_repos::assignees::MAX_ASSIGNEES;
use codeyard_webui::pages::repos::issues::Kind;
use std::collections::HashMap;
use std::sync::Arc;

fn authority(ctx: &PageContext) -> ResultPage<codeyard_database::queries::credentials::Actor> {
    Ok(ctx.database_actor(ctx.require_user()?.id))
}

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
        Kind::Issue => {
            codeyard_db_repos::issues::get_issue(&ctx.state.pool, repo.record.id, number)
                .await
                .map_err(|_| unavailable(ctx))?
                .map(|issue| AssignedItem::Issue(issue.id))
                .ok_or_else(|| ctx.error(StatusCode::NOT_FOUND, "Issue not found."))
        }
        Kind::Pull => codeyard_db_repos::pull_requests::records::get_pull_request(
            &ctx.state.pool,
            repo.record.id,
            number,
        )
        .await
        .map_err(|_| unavailable(ctx))?
        .map(|pull| AssignedItem::PullRequest(pull.id))
        .ok_or_else(|| ctx.error(StatusCode::NOT_FOUND, "Pull request not found.")),
    }
}

/// The user id of a username. A name that is no user matches nothing.
pub(crate) async fn user_id(ctx: &PageContext, username: &str) -> ResultPage<Option<i64>> {
    Ok(
        codeyard_db_identity::users::accounts::get_user_by_username(&ctx.state.pool, username)
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
    crate::discussion::require_item_binding(&ctx, &form, repo.record.id, value.id)?;
    let subject = assigned_item(&ctx, &repo, kind, number).await?;
    let username = field(&form, "username");
    let target = codeyard_repositories::issues::assignments::Target {
        actor: ctx.require_user()?,
        repository: &repo.record,
        owner: &owner,
        item: subject,
        number,
        title: &value.title,
    };
    let outcome = match codeyard_db_identity::users::accounts::get_user_by_username(
        &ctx.state.pool,
        username.trim(),
    )
    .await
    .map_err(|_| unavailable(&ctx))?
    {
        Some(assignee) => codeyard_repositories::issues::assignments::assign(
            &ctx.state.pool,
            &ctx.state.notification_broadcaster,
            &target,
            authority(&ctx)?,
            &[assignee],
        )
        .await
        .map(|_| ())
        .map_err(assignment_error),
        None => Err(assignment_error(AssignmentRefusal::Unassignable.into())),
    };
    detail_outcome(&ctx, &repo, kind, &value, number, outcome).await
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
    crate::discussion::require_item_binding(&ctx, &form, repo.record.id, value.id)?;
    let subject = assigned_item(&ctx, &repo, kind, number).await?;
    // A name that is no user, or is not assigned, leaves the item as it is and records nothing.
    let target = codeyard_repositories::issues::assignments::Target {
        actor: ctx.require_user()?,
        repository: &repo.record,
        owner: &owner,
        item: subject,
        number,
        title: &value.title,
    };
    let outcome = codeyard_repositories::issues::assignments::unassign(
        &ctx.state.pool,
        &target,
        authority(&ctx)?,
        &[username],
    )
    .await
    .map(|_| ())
    .map_err(assignment_error);
    detail_outcome(&ctx, &repo, kind, &value, number, outcome).await
}
