use super::super::super::{auth, repository, Error, Result};
use super::super::{comments, document as pull_document, target};
use super::document;
use crate::backend::{
    app::AppState,
    db,
    repositories::{
        access,
        pull_requests::{self, reviews as domain},
    },
};
use axum::{
    body::Bytes,
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{delete, post, put, MethodRouter},
    Json,
};
use serde::Deserialize;
use serde_json::Value;
use std::sync::Arc;

fn failure(error: pull_requests::OperationError) -> Error {
    match error {
        pull_requests::OperationError::Invalid { message, .. }
        | pull_requests::OperationError::Conflict { message, .. } => Error::invalid_simple(message),
        other => super::super::mutations::operation_error(other),
    }
}
fn state_for(event: Option<&str>) -> Result<Option<String>> {
    Ok(match event {
        None => None,
        Some("APPROVE") => Some("approved".into()),
        Some("REQUEST_CHANGES") => Some("changes_requested".into()),
        Some("COMMENT") => Some("commented".into()),
        _ => return Err(Error::invalid_simple("Invalid review event")),
    })
}
#[derive(Default, Deserialize)]
struct Create {
    body: Option<String>,
    event: Option<String>,
    commit_id: Option<String>,
    #[serde(default)]
    comments: Vec<comments::Create>,
}

async fn create(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, name, number)): Path<(String, String, i32)>,
    body: Bytes,
) -> Result<Json<Value>> {
    let (repo, viewer) = repository(&state, &headers, &owner, &name, true).await?;
    let authentication = viewer.ok_or_else(auth::required)?;
    let actor = &authentication.user;
    let (_, pull) = target(&state, &headers, &owner, &name, number).await?;
    let input: Create = if body.is_empty() {
        Create::default()
    } else {
        serde_json::from_slice(&body).map_err(|_| Error::invalid_simple("Invalid review"))?
    };
    let review_state = state_for(input.event.as_deref())?;
    crate::backend::issues::validate_body(input.body.as_deref().unwrap_or(""), false)
        .map_err(Error::invalid_simple)?;
    if input.comments.len() > 200 {
        return Err(Error::invalid_simple("A review holds at most 200 comments"));
    }
    let commit = input.commit_id.unwrap_or_else(|| pull.head_sha.clone());
    if !crate::backend::git::is_full_commit_id(&commit)
        || !commit.eq_ignore_ascii_case(&pull.head_sha)
    {
        return Err(Error::invalid_simple(
            "Review the current pull request head",
        ));
    }
    let mut prepared = Vec::new();
    for mut comment in input.comments {
        crate::backend::issues::validate_body(&comment.body, true)
            .map_err(Error::invalid_simple)?;
        comment.commit_id = Some(commit.clone());
        prepared.push(
            comments::prepare(&state, &headers, &repo, &pull, comment, true)
                .await
                .map_err(|e| Error::invalid_simple(e.1))?,
        );
    }
    if review_state.is_some() && prepared.is_empty() {
        let review = domain::create_pull_request_review(
            &state.pool,
            state.cluster.as_ref(),
            &state.notification_broadcaster,
            actor.id,
            &authentication.credential(),
            (owner.clone(), name, number),
            domain::CreatePullRequestReviewRequest {
                state: review_state,
                body: input.body,
                commit_sha: Some(commit),
                review_id: None,
            },
        )
        .await
        .map_err(failure)?;
        return Ok(Json(document(&state, &owner, &repo, &pull, &review).await?));
    }
    let draft = db::pull_requests::reviews::create_pending_review(
        &state.pool,
        pull.id,
        actor.id,
        input.body.as_deref(),
        &commit,
    )
    .await
    .map_err(|e| failure(pull_requests::write_error(e)))?;
    for mut request in prepared {
        request.review_id = Some(draft.id);
        if let Err(error) = pull_requests::comments::create_pull_request_comment(
            &state.pool,
            state.cluster.as_ref(),
            &state.notification_broadcaster,
            actor.id,
            owner.clone(),
            name.clone(),
            number,
            request,
        )
        .await
        {
            db::pull_requests::reviews::delete_pending_review_by_id(
                &state.pool,
                pull.id,
                actor.id,
                draft.id,
            )
            .await
            .map_err(Error::unavailable)?;
            return Err(failure(error));
        }
    }
    let review = if let Some(review_state) = review_state {
        match domain::create_pull_request_review(
            &state.pool,
            state.cluster.as_ref(),
            &state.notification_broadcaster,
            actor.id,
            &authentication.credential(),
            (owner.clone(), name, number),
            domain::CreatePullRequestReviewRequest {
                state: Some(review_state),
                body: input.body,
                commit_sha: Some(commit),
                review_id: Some(draft.id),
            },
        )
        .await
        {
            Ok(review) => review,
            Err(error) => {
                // A refused submission leaves no pending review, so the request can be retried.
                db::pull_requests::reviews::delete_pending_review_by_id(
                    &state.pool,
                    pull.id,
                    actor.id,
                    draft.id,
                )
                .await
                .map_err(Error::unavailable)?;
                return Err(failure(error));
            }
        }
    } else {
        draft
    };
    Ok(Json(document(&state, &owner, &repo, &pull, &review).await?))
}

#[derive(Deserialize)]
struct Body {
    body: String,
}
async fn update(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, name, number, id)): Path<(String, String, i32, i64)>,
    body: Bytes,
) -> Result<Json<Value>> {
    let (repo, viewer) = repository(&state, &headers, &owner, &name, true).await?;
    let actor = viewer.ok_or_else(auth::required)?.user;
    let (_, pull) = target(&state, &headers, &owner, &name, number).await?;
    let input: Body =
        serde_json::from_slice(&body).map_err(|_| Error::invalid_simple("body is required"))?;
    let review = domain::edit_review(&state.pool, actor.id, &repo, &pull, id, &input.body)
        .await
        .map_err(failure)?;
    Ok(Json(document(&state, &owner, &repo, &pull, &review).await?))
}
async fn remove(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, name, number, id)): Path<(String, String, i32, i64)>,
) -> Result<Json<Value>> {
    let (repo, viewer) = repository(&state, &headers, &owner, &name, true).await?;
    let actor = viewer.ok_or_else(auth::required)?.user;
    let (_, pull) = target(&state, &headers, &owner, &name, number).await?;
    let review = db::pull_requests::reviews::get_pull_request_review_by_id(&state.pool, id)
        .await
        .map_err(Error::unavailable)?
        .filter(|r| r.pull_request_id == pull.id && r.author_id == actor.id)
        .ok_or_else(Error::missing)?;
    if review.state != "pending" {
        return Err(Error::invalid_simple("Only pending reviews may be deleted"));
    }
    let deleted =
        db::pull_requests::reviews::delete_pending_review_by_id(&state.pool, pull.id, actor.id, id)
            .await
            .map_err(|e| failure(pull_requests::write_error(e)))?
            .ok_or_else(Error::missing)?;
    Ok(Json(
        document(&state, &owner, &repo, &pull, &deleted).await?,
    ))
}
#[derive(Deserialize)]
struct Submit {
    event: String,
    body: Option<String>,
}
async fn submit(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, name, number, id)): Path<(String, String, i32, i64)>,
    body: Bytes,
) -> Result<Json<Value>> {
    let (repo, viewer) = repository(&state, &headers, &owner, &name, true).await?;
    let authentication = viewer.ok_or_else(auth::required)?;
    let actor = &authentication.user;
    let (_, pull) = target(&state, &headers, &owner, &name, number).await?;
    let input: Submit =
        serde_json::from_slice(&body).map_err(|_| Error::invalid_simple("event is required"))?;
    let draft = db::pull_requests::reviews::get_pull_request_review_by_id(&state.pool, id)
        .await
        .map_err(Error::unavailable)?
        .filter(|r| r.pull_request_id == pull.id && r.author_id == actor.id)
        .ok_or_else(Error::missing)?;
    if draft.state != "pending" {
        return Err(Error::invalid_simple("Review is not pending"));
    }
    let review = domain::create_pull_request_review(
        &state.pool,
        state.cluster.as_ref(),
        &state.notification_broadcaster,
        actor.id,
        &authentication.credential(),
        (owner.clone(), name, number),
        domain::CreatePullRequestReviewRequest {
            state: state_for(Some(&input.event))?,
            body: input.body.or(draft.body),
            commit_sha: draft.commit_sha,
            review_id: Some(id),
        },
    )
    .await
    .map_err(failure)?;
    Ok(Json(document(&state, &owner, &repo, &pull, &review).await?))
}

#[derive(Default, Deserialize)]
struct Reviewers {
    reviewers: Option<Vec<String>>,
    team_reviewers: Option<Vec<String>>,
}
async fn change_reviewers(
    state: Arc<AppState>,
    headers: HeaderMap,
    namespace: (String, String, i32),
    body: Bytes,
    remove: bool,
    rerequest: bool,
) -> Result<Response> {
    let (owner, name, number) = namespace;
    let (repo, viewer) = repository(&state, &headers, &owner, &name, true).await?;
    let actor = viewer.ok_or_else(auth::required)?.user;
    let (_, pull) = target(&state, &headers, &owner, &name, number).await?;
    if !access::can_write(&state.pool, &actor, &repo)
        .await
        .map_err(Error::unavailable)?
    {
        return Err(Error::new(StatusCode::FORBIDDEN, "Repository write denied"));
    }
    let input: Reviewers =
        serde_json::from_slice(&body).map_err(|_| Error::invalid("Invalid reviewers"))?;
    if input.reviewers.is_none() && input.team_reviewers.is_none() {
        return Err(Error::invalid("reviewers or team_reviewers is required"));
    }
    let teams = input.team_reviewers.unwrap_or_default();
    let names = input.reviewers.unwrap_or_default();
    if names.len() + teams.len() > 100 {
        return Err(Error::invalid("At most 100 reviewers per request"));
    }
    for username in &names {
        let user = db::users::accounts::get_user_by_username(&state.pool, username)
            .await
            .map_err(Error::unavailable)?
            .ok_or_else(|| Error::invalid("Reviewer not found"))?;
        if !remove
            && (user.id == pull.author_id
                || !access::can_write(&state.pool, &user, &repo)
                    .await
                    .map_err(Error::unavailable)?)
        {
            return Err(Error::invalid(
                "Reviewer must be a writer other than the author",
            ));
        }
    }
    let eligible =
        db::pull_requests::team_reviewers::list(&state.pool, repo.id, None, Some(actor.id))
            .await
            .map_err(Error::unavailable)?;
    if !remove
        && teams.iter().any(|slug| {
            !eligible
                .iter()
                .any(|team| team.team.slug.eq_ignore_ascii_case(slug))
        })
    {
        return Err(Error::invalid(
            "Review team not found or lacks write access",
        ));
    }
    crate::backend::repositories::pull_requests::team_reviewers::change(
        &state.pool,
        &state.notification_broadcaster,
        &actor,
        &repo,
        &pull,
        &teams,
        if remove {
            db::pull_requests::team_reviewers::ReviewTeamAction::Remove
        } else if rerequest {
            db::pull_requests::team_reviewers::ReviewTeamAction::Rerequest
        } else {
            db::pull_requests::team_reviewers::ReviewTeamAction::Request
        },
    )
    .await
    .map_err(super::super::mutations::operation_error)?;
    for username in names {
        if remove || rerequest {
            let existing =
                db::pull_requests::reviews::list_pull_request_reviewers(&state.pool, pull.id)
                    .await
                    .map_err(Error::unavailable)?;
            if existing
                .iter()
                .any(|u| u.username.eq_ignore_ascii_case(&username))
            {
                domain::remove_requested_reviewer(
                    &state.pool,
                    actor.id,
                    owner.clone(),
                    name.clone(),
                    number,
                    username.clone(),
                )
                .await
                .map_err(super::super::mutations::operation_error)?;
            }
        }
        if !remove {
            domain::request_reviewer(
                &state.pool,
                &state.notification_broadcaster,
                actor.id,
                owner.clone(),
                name.clone(),
                number,
                domain::RequestReviewerRequest { username },
            )
            .await
            .map_err(super::super::mutations::operation_error)?;
        }
    }
    let value = pull_document(&state, &headers, &repo, &owner, &pull, false).await?;
    Ok((
        if remove {
            StatusCode::OK
        } else {
            StatusCode::CREATED
        },
        Json(value),
    )
        .into_response())
}
async fn request(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(namespace): Path<(String, String, i32)>,
    body: Bytes,
) -> Result<Response> {
    change_reviewers(state, headers, namespace, body, false, false).await
}
async fn unrequest(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(namespace): Path<(String, String, i32)>,
    body: Bytes,
) -> Result<Response> {
    change_reviewers(state, headers, namespace, body, true, false).await
}

async fn rerequest(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(namespace): Path<(String, String, i32)>,
    body: Bytes,
) -> Result<Response> {
    change_reviewers(state, headers, namespace, body, false, true).await
}

pub(super) fn handlers() -> Vec<(&'static str, MethodRouter<Arc<AppState>>)> {
    vec![
        ("pulls/create-review", post(create)),
        ("pulls/update-review", put(update)),
        ("pulls/delete-pending-review", delete(remove)),
        ("pulls/submit-review", post(submit)),
        ("pulls/request-reviewers", post(request)),
        ("pulls/rerequest-reviewers", post(rerequest)),
        ("pulls/remove-requested-reviewers", delete(unrequest)),
    ]
}
