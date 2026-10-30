//! Review drafts, publication and reviewer requests retain the REST lane's native authority checks.
use super::{
    db, missing,
    mutations::{pull_error, target, text},
    node_objects, repository, unavailable, RequestContext, Result, Value,
};
use crate::backend::repositories::pull_requests::{self, reviews};
use serde_json::json;
pub(super) const FIELDS: &[&str] = &[
    "addPullRequestReview",
    "submitPullRequestReview",
    "updatePullRequestReview",
    "deletePullRequestReview",
    "dismissPullRequestReview",
    "requestReviews",
];
fn event(input: &Value) -> Result<Option<String>> {
    Ok(match input["event"].as_str() {
        None => None,
        Some("APPROVE") => Some("approved".into()),
        Some("REQUEST_CHANGES") => Some("changes_requested".into()),
        Some("COMMENT") => Some("commented".into()),
        _ => return Err(missing()),
    })
}
pub(super) async fn resolve(field: &str, input: &Value, request: &RequestContext) -> Result<Value> {
    let allowed: &[&str] = match field {
        "addPullRequestReview" => &["pullRequestId", "body", "commitOID", "event"],
        "submitPullRequestReview" => &["pullRequestId", "pullRequestReviewId", "body", "event"],
        "updatePullRequestReview" => &["pullRequestReviewId", "body"],
        "deletePullRequestReview" => &["pullRequestReviewId"],
        "dismissPullRequestReview" => &["pullRequestReviewId", "message"],
        "requestReviews" => &["pullRequestId", "userIds", "union"],
        _ => return Err(missing()),
    };
    if let Some(key) = input
        .as_object()
        .into_iter()
        .flat_map(|v| v.keys())
        .find(|key| {
            key.as_str() != "clientMutationId"
                && !allowed.contains(&key.as_str())
                && !input[*key].is_null()
                && input[*key].as_array().is_none_or(|v| !v.is_empty())
        })
    {
        return Err(async_graphql::Error::new(format!(
            "Codeyard does not support {field}.{key} yet"
        )));
    }
    let pool = &request.state.pool;
    let actor = request.auth.user.id;
    let existing = if input["pullRequestReviewId"].is_null() {
        None
    } else {
        Some(
            db::pull_requests::reviews::get_pull_request_review_by_id(
                pool,
                target(input, "pullRequestReviewId", "PRR")?,
            )
            .await
            .map_err(unavailable)?
            .ok_or_else(missing)?,
        )
    };
    let pull_id = if let Some(row) = &existing {
        if !input["pullRequestId"].is_null()
            && target(input, "pullRequestId", "PR")? != row.pull_request_id
        {
            return Err(missing());
        }
        row.pull_request_id
    } else {
        target(input, "pullRequestId", "PR")?
    };
    let pull = db::pull_requests::records::get_pull_request_by_id(pool, pull_id)
        .await
        .map_err(unavailable)?
        .ok_or_else(missing)?;
    let repo = repository::by_id(pull.repo_id, request).await?;
    request.authorize(repo.id, "pull-requests", true).await?;
    let owner = repository::owner(&repo, request).await?;
    let mut payload = json!({"clientMutationId":input["clientMutationId"]});
    if field == "requestReviews" {
        let ids = super::metadata_mutations::ids(&input["userIds"], "U")?;
        let mut names = vec![];
        for id in ids {
            names.push(
                db::users::accounts::get_user_by_id(pool, id)
                    .await
                    .map_err(unavailable)?
                    .ok_or_else(missing)?
                    .username,
            );
        }
        for name in &names {
            reviews::request_reviewer(
                pool,
                &request.state.notification_broadcaster,
                actor,
                owner.clone(),
                repo.name.clone(),
                pull.number,
                reviews::RequestReviewerRequest {
                    username: name.clone(),
                },
            )
            .await
            .map_err(pull_error)?;
        }
        if input["union"].as_bool() != Some(true) {
            for row in db::pull_requests::reviews::list_pull_request_reviewers(pool, pull.id)
                .await
                .map_err(unavailable)?
            {
                if !names.iter().any(|n| n.eq_ignore_ascii_case(&row.username)) {
                    reviews::remove_requested_reviewer(
                        pool,
                        actor,
                        owner.clone(),
                        repo.name.clone(),
                        pull.number,
                        row.username,
                    )
                    .await
                    .map_err(pull_error)?;
                }
            }
        }
        payload["pullRequest"] = super::issue::pull_object(&pull);
        payload["actor"] = super::user::actor_object(&request.auth.user, actor);
        payload["requestedReviewersEdge"] = Value::Null;
        return Ok(payload);
    }
    let existing = if existing.is_none() && field == "submitPullRequestReview" {
        db::pull_requests::reviews::get_pending_review(pool, pull.id, actor)
            .await
            .map_err(unavailable)?
    } else {
        existing
    };
    if existing
        .as_ref()
        .is_some_and(|r| r.state == "pending" && r.author_id != actor)
    {
        return Err(missing());
    }
    let review = match field {
        "addPullRequestReview" | "submitPullRequestReview" => {
            let body = input["body"]
                .as_str()
                .map(str::to_owned)
                .or_else(|| existing.as_ref().and_then(|r| r.body.clone()));
            crate::backend::issues::validate_body(body.as_deref().unwrap_or(""), false)
                .map_err(async_graphql::Error::new)?;
            let commit = input
                .get("commitOID")
                .filter(|v| !v.is_null())
                .map(|v| {
                    v.as_str().ok_or_else(|| {
                        async_graphql::Error::new("commitOID must be a Git object ID")
                    })
                })
                .transpose()?
                // A pending review is submitted on the commit it was drafted on, so a stale draft is refused below.
                .or_else(|| {
                    (field == "submitPullRequestReview")
                        .then(|| existing.as_ref().and_then(|r| r.commit_sha.as_deref()))
                        .flatten()
                })
                .unwrap_or(&pull.head_sha);
            if !crate::backend::git::is_full_commit_id(commit)
                || !commit.eq_ignore_ascii_case(&pull.head_sha)
            {
                return Err(async_graphql::Error::new(
                    "Review the current pull request head",
                ));
            }
            if field == "submitPullRequestReview" && existing.is_none() {
                return Err(missing());
            }
            if let Some(state) = event(input)? {
                reviews::create_pull_request_review(
                    pool,
                    request.state.cluster.as_ref(),
                    &request.state.notification_broadcaster,
                    actor,
                    (owner, repo.name.clone(), pull.number),
                    reviews::CreatePullRequestReviewRequest {
                        state: Some(state),
                        body,
                        commit_sha: Some(commit.to_owned()),
                        review_id: existing.as_ref().map(|r| r.id),
                    },
                )
                .await
                .map_err(pull_error)?
            } else {
                db::pull_requests::reviews::create_pending_review(
                    pool,
                    pull.id,
                    actor,
                    body.as_deref(),
                    commit,
                )
                .await
                .map_err(|e| pull_error(pull_requests::write_error(e)))?
            }
        }
        "updatePullRequestReview" => reviews::edit_review(
            pool,
            &request.state.notification_broadcaster,
            actor,
            &repo,
            &pull,
            existing.as_ref().ok_or_else(missing)?.id,
            text(input, "body")?,
        )
        .await
        .map_err(pull_error)?,
        "dismissPullRequestReview" => reviews::dismiss_pull_request_review(
            pool,
            actor,
            owner,
            repo.name.clone(),
            pull.number,
            existing.as_ref().ok_or_else(missing)?.id,
            reviews::DismissPullRequestReviewRequest {
                reason: Some(text(input, "message")?.to_owned()),
            },
        )
        .await
        .map_err(pull_error)?,
        "deletePullRequestReview" => db::pull_requests::reviews::delete_pending_review_by_id(
            pool,
            pull.id,
            actor,
            existing.as_ref().ok_or_else(missing)?.id,
        )
        .await
        .map_err(|e| pull_error(pull_requests::write_error(e)))?
        .ok_or_else(missing)?,
        _ => return Err(missing()),
    };
    let object = node_objects::review(&review, request).await?;
    payload["pullRequestReview"] = object;
    Ok(payload)
}
