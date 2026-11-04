use crate::pulls::document as pull_document;
use crate::pulls::reviews::document;
use crate::pulls::target;
use axum::body::Bytes;
use axum::extract::Path;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::response::Response;
use axum::routing::delete;
use axum::routing::post;
use axum::routing::put;
use axum::routing::MethodRouter;
use axum::Json;
use codeyard_app::api_shared::github::repository;
use codeyard_app::app::AppState;
use codeyard_model::github_error::json_parse_error;
use codeyard_model::github_error::Error;
use codeyard_model::github_error::Result;
use serde::Deserialize;
use serde_json::Value;
use std::sync::Arc;

fn failure(error: codeyard_app::repositories::pull_requests::OperationError) -> Error {
    match error {
        codeyard_app::repositories::pull_requests::OperationError::Invalid { message, .. }
        | codeyard_app::repositories::pull_requests::OperationError::Conflict { message, .. } => {
            Error::invalid_simple(message)
        }
        other => crate::pulls::mutations::operation_error(other),
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
    comments: Vec<crate::pulls::comments::Create>,
}

async fn create(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, name, number)): Path<(String, String, i32)>,
    body: Bytes,
) -> Result<Json<Value>> {
    let (repo, viewer) = repository(&state, &headers, &owner, &name, true).await?;
    let auth = viewer.ok_or_else(codeyard_app::api_shared::github::auth::required)?;
    let actor = &auth.user;
    let (_, pull) = target(&state, &headers, &owner, &name, number).await?;
    let input: Create = if body.is_empty() {
        Create::default()
    } else {
        serde_json::from_slice(&body).map_err(json_parse_error)?
    };
    let review_state = state_for(input.event.as_deref())?;
    codeyard_model::issue_input::validate_body(input.body.as_deref().unwrap_or(""), false)
        .map_err(Error::invalid_simple)?;
    if input.comments.len() > 200 {
        return Err(Error::invalid_simple("A review holds at most 200 comments"));
    }
    let commit = input.commit_id.unwrap_or_else(|| pull.head_sha.clone());
    if !codeyard_git::is_full_commit_id(&commit) || !commit.eq_ignore_ascii_case(&pull.head_sha) {
        return Err(Error::invalid_simple(
            "Review the current pull request head",
        ));
    }
    let position_diff = if input
        .comments
        .iter()
        .any(|comment| comment.position.is_some() && comment.line.is_none())
    {
        Some(
            codeyard_app::repositories::store::repo_store(&repo, state.cluster.as_ref())
                .diff(pull.diff_base(), &pull.head_sha)
                .await
                .map_err(codeyard_app::api_shared::github::contents::store_error)?,
        )
    } else {
        None
    };
    let mut prepared = Vec::new();
    for mut comment in input.comments {
        codeyard_model::issue_input::validate_body(&comment.body, true)
            .map_err(Error::invalid_simple)?;
        comment.commit_id = Some(commit.clone());
        prepared.push(
            crate::pulls::comments::prepare_with_diff(
                &state,
                &headers,
                &repo,
                &pull,
                comment,
                true,
                position_diff.as_ref(),
            )
            .await
            .map_err(|e| Error::invalid_simple(e.1))?,
        );
    }
    if review_state.is_some() && prepared.is_empty() {
        let review = codeyard_app::repositories::pull_requests::reviews::create_pull_request_review(
            &state.pool,
            state.cluster.as_ref(),
            &state.notification_broadcaster,
            actor.id,
            &auth.credential(),
            (owner.clone(), name, number),
            codeyard_app::repositories::pull_requests::reviews::CreatePullRequestReviewRequest {
                state: review_state,
                body: input.body,
                commit_sha: Some(commit),
                review_id: None,
                token: Some(auth.token.id),
            },
        )
        .await
        .map_err(failure)?;
        return Ok(Json(document(&state, &owner, &repo, &pull, &review).await?));
    }
    let review = codeyard_app::repositories::pull_requests::reviews::create_pull_request_review_batch(
        &state.pool,
        state.cluster.as_ref(),
        &state.notification_broadcaster,
        actor.id,
        &auth.credential(),
        (owner.clone(), name, number),
        codeyard_app::repositories::pull_requests::reviews::CreatePullRequestReviewBatchRequest {
            review: codeyard_app::repositories::pull_requests::reviews::CreatePullRequestReviewRequest {
                state: review_state,
                body: input.body,
                commit_sha: Some(commit),
                review_id: None,
                token: Some(auth.token.id),
            },
            comments: prepared,
        },
    )
    .await
    .map_err(failure)?;
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
    let actor = viewer
        .ok_or_else(codeyard_app::api_shared::github::auth::required)?
        .user;
    let (_, pull) = target(&state, &headers, &owner, &name, number).await?;
    let input: Body = serde_json::from_slice(&body).map_err(json_parse_error)?;
    let review = codeyard_app::repositories::pull_requests::reviews::edit_review(
        &state.pool,
        &state.notification_broadcaster,
        actor.id,
        &repo,
        &pull,
        id,
        &input.body,
    )
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
    let actor = viewer
        .ok_or_else(codeyard_app::api_shared::github::auth::required)?
        .user;
    let (_, pull) = target(&state, &headers, &owner, &name, number).await?;
    let review =
        codeyard_db_repos::pull_requests::reviews::get_pull_request_review_by_id(&state.pool, id)
            .await
            .map_err(Error::unavailable)?
            .filter(|r| r.pull_request_id == pull.id && r.author_id == actor.id)
            .ok_or_else(Error::missing)?;
    if review.state != "pending" {
        return Err(Error::invalid_simple("Only pending reviews may be deleted"));
    }
    let deleted = codeyard_db_repos::pull_requests::reviews::delete_pending_review_by_id(
        &state.pool,
        pull.id,
        actor.id,
        id,
    )
    .await
    .map_err(|e| failure(codeyard_app::repositories::pull_requests::write_error(e)))?
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
    let auth = viewer.ok_or_else(codeyard_app::api_shared::github::auth::required)?;
    let actor = &auth.user;
    let (_, pull) = target(&state, &headers, &owner, &name, number).await?;
    let input: Submit = serde_json::from_slice(&body).map_err(json_parse_error)?;
    let draft =
        codeyard_db_repos::pull_requests::reviews::get_pull_request_review_by_id(&state.pool, id)
            .await
            .map_err(Error::unavailable)?
            .filter(|r| r.pull_request_id == pull.id && r.author_id == actor.id)
            .ok_or_else(Error::missing)?;
    if draft.state != "pending" {
        return Err(Error::invalid_simple("Review is not pending"));
    }
    let review = codeyard_app::repositories::pull_requests::reviews::create_pull_request_review(
        &state.pool,
        state.cluster.as_ref(),
        &state.notification_broadcaster,
        actor.id,
        &auth.credential(),
        (owner.clone(), name, number),
        codeyard_app::repositories::pull_requests::reviews::CreatePullRequestReviewRequest {
            state: state_for(Some(&input.event))?,
            body: input.body,
            commit_sha: draft.commit_sha,
            review_id: Some(id),
            token: Some(auth.token.id),
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
    let actor = viewer
        .ok_or_else(codeyard_app::api_shared::github::auth::required)?
        .user;
    let (_, pull) = target(&state, &headers, &owner, &name, number).await?;
    if !codeyard_identity::repository_access::can_write(&state.pool, &actor, &repo)
        .await
        .map_err(Error::unavailable)?
    {
        return Err(Error::new(StatusCode::FORBIDDEN, "Repository write denied"));
    }
    let input: Reviewers = serde_json::from_slice(&body).map_err(json_parse_error)?;
    if input.reviewers.is_none() && input.team_reviewers.is_none() {
        return Err(Error::invalid("reviewers or team_reviewers is required"));
    }
    let teams = input.team_reviewers.unwrap_or_default();
    let names = input.reviewers.unwrap_or_default();
    if names.len() + teams.len() > 100 {
        return Err(Error::invalid("At most 100 reviewers per request"));
    }
    for username in &names {
        let user =
            codeyard_db_identity::users::accounts::get_user_by_username(&state.pool, username)
                .await
                .map_err(Error::unavailable)?
                .ok_or_else(|| Error::invalid("Reviewer not found"))?;
        if !remove
            && (user.id == pull.author_id
                || !codeyard_identity::repository_access::can_write(&state.pool, &user, &repo)
                    .await
                    .map_err(Error::unavailable)?)
        {
            return Err(Error::invalid(
                "Reviewer must be a writer other than the author",
            ));
        }
    }
    let eligible = codeyard_db_repos::pull_requests::team_reviewers::list(
        &state.pool,
        repo.id,
        None,
        Some(actor.id),
    )
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
    codeyard_app::repositories::pull_requests::team_reviewers::change(
        &state.pool,
        &state.notification_broadcaster,
        &actor,
        &repo,
        &pull,
        &teams,
        if remove {
            codeyard_db_repos::pull_requests::team_reviewers::ReviewTeamAction::Remove
        } else if rerequest {
            codeyard_db_repos::pull_requests::team_reviewers::ReviewTeamAction::Rerequest
        } else {
            codeyard_db_repos::pull_requests::team_reviewers::ReviewTeamAction::Request
        },
    )
    .await
    .map_err(crate::pulls::mutations::operation_error)?;
    for username in names {
        if remove || rerequest {
            let existing = codeyard_db_repos::pull_requests::reviews::list_pull_request_reviewers(
                &state.pool,
                pull.id,
            )
            .await
            .map_err(Error::unavailable)?;
            if existing
                .iter()
                .any(|u| u.username.eq_ignore_ascii_case(&username))
            {
                codeyard_app::repositories::pull_requests::reviews::remove_requested_reviewer(
                    &state.pool,
                    actor.id,
                    owner.clone(),
                    name.clone(),
                    number,
                    username.clone(),
                )
                .await
                .map_err(crate::pulls::mutations::operation_error)?;
            }
        }
        if !remove {
            codeyard_app::repositories::pull_requests::reviews::request_reviewer(
                &state.pool,
                &state.notification_broadcaster,
                actor.id,
                owner.clone(),
                name.clone(),
                number,
                codeyard_app::repositories::pull_requests::reviews::RequestReviewerRequest {
                    username,
                },
            )
            .await
            .map_err(crate::pulls::mutations::operation_error)?;
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
