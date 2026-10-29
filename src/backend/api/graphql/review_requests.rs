//! Pull review requests project the native user/team records and use the same mutations as REST.
use super::{
    connection, db, issue, missing, node_id, repository, unavailable, url, user, Arguments,
    RequestContext, Result, Value,
};
use crate::backend::repositories::{
    access,
    pull_requests::{self, reviews, team_reviewers},
};
use async_graphql::{Error, ErrorExtensions};
use serde_json::json;

fn operation(error: pull_requests::OperationError) -> Error {
    let kind = match &error {
        pull_requests::OperationError::Missing { .. } => "NOT_FOUND",
        pull_requests::OperationError::Denied { .. }
        | pull_requests::OperationError::Unauthenticated { .. } => "FORBIDDEN",
        pull_requests::OperationError::Unavailable { .. } => "INTERNAL",
        _ => "UNPROCESSABLE",
    };
    Error::new(error.to_string()).extend_with(|_, e| e.set("type", kind))
}

pub(super) async fn team_object(
    team: &db::models::Team,
    request: &RequestContext,
) -> Result<Value> {
    let organization = user::organization_by_id(team.org_id, request).await?;
    let org = organization["login"]
        .as_str()
        .ok_or_else(missing)?
        .to_owned();
    Ok(
        json!({"__typename":"Team","id":node_id::encode("T",team.id),"databaseId":team.id,
        "name":team.name,"slug":team.slug,"description":team.description,"createdAt":team.created_at,
        "updatedAt":team.updated_at,"privacy":if team.privacy=="secret" {"SECRET"} else {"VISIBLE"},
        "combinedSlug":format!("{org}/{}",team.slug),"organization":organization,
        "url":url(&format!("/orgs/{}/teams/{}",crate::webui::encode_component(&org),crate::webui::encode_component(&team.slug)))}),
    )
}

async fn pull(id: i64, request: &RequestContext) -> Result<db::models::PullRequest> {
    let pull = db::pull_requests::records::get_pull_request_by_id(&request.state.pool, id)
        .await
        .map_err(unavailable)?
        .ok_or_else(missing)?;
    // Reviewer requests belong to the base repository, which stays readable after its fork is deleted.
    repository::by_id(pull.repo_id, request).await?;
    request
        .authorize(pull.repo_id, "pull-requests", false)
        .await?;
    Ok(pull)
}

pub(super) async fn nodes(id: i64, request: &RequestContext) -> Result<Vec<Value>> {
    let pool = &request.state.pool;
    let pull = pull(id, request).await?;
    let origins = db::pull_requests::reviews::review_request_origins(pool, id)
        .await
        .map_err(unavailable)?;
    let mut nodes = Vec::new();
    for reviewer in db::pull_requests::reviews::list_pull_request_reviewers(pool, id)
        .await
        .map_err(unavailable)?
    {
        if let Some(origin) = origins
            .iter()
            .find(|origin| origin.user_id == Some(reviewer.id))
        {
            nodes.push(json!({"__typename":"ReviewRequest","id":node_id::review_request(id,false,reviewer.id),
                "databaseId":null,"asCodeOwner":origin.as_code_owner,"pullRequest":issue::pull_object(&pull),
                "requestedReviewer":if reviewer.is_banned { Value::Null } else { user::actor_object(&reviewer,request.auth.user.id) }}));
        }
    }
    for row in db::pull_requests::team_reviewers::list(
        pool,
        pull.repo_id,
        Some(id),
        request.private_viewer(pull.repo_id, "pull-requests").await?,
    )
    .await
    .map_err(unavailable)?
    {
        if let Some(origin) = origins
            .iter()
            .find(|origin| origin.team_id == Some(row.team.id))
        {
            nodes.push(json!({"__typename":"ReviewRequest","id":node_id::review_request(id,true,row.team.id),
                "databaseId":null,"asCodeOwner":origin.as_code_owner,"pullRequest":issue::pull_object(&pull),
                "requestedReviewer":team_object(&row.team,request).await?}));
        }
    }
    Ok(nodes)
}

pub(super) async fn list(ctx: &Arguments, request: &RequestContext, id: i64) -> Result<Value> {
    connection::validate(ctx)?;
    connection::slice(ctx, nodes(id, request).await?)
}

pub(super) async fn node(id: &str, request: &RequestContext) -> Result<Value> {
    let (pull, _, _) = node_id::decode_review_request(id).ok_or_else(missing)?;
    nodes(pull, request)
        .await?
        .into_iter()
        .find(|node| node["id"] == id)
        .ok_or_else(missing)
}

fn ids(input: &Value, field: &str, kind: &str) -> Result<Vec<i64>> {
    // A single ID is a valid list input in GraphQL.
    let mut ids = super::metadata_mutations::ids(&input[field], kind)?;
    ids.sort_unstable();
    ids.dedup();
    Ok(ids)
}

pub(super) async fn mutation(
    field: &str,
    ctx: &Arguments,
    request: &RequestContext,
) -> Result<Value> {
    request
        .auth
        .require_write()
        .map_err(|_| Error::new("Resource not accessible by token"))?;
    let input: Value = ctx.args.try_get("input")?.deserialize()?;
    if !input["botIds"].is_null() && input["botIds"]
        .as_array()
        .is_none_or(|bots| !bots.is_empty())
    {
        return Err(Error::new("Bot review requests are not supported"));
    }
    let (_, id) = input["pullRequestId"]
        .as_str()
        .and_then(node_id::decode)
        .filter(|(kind, _)| *kind == "PR")
        .ok_or_else(missing)?;
    let pull = pull(id, request).await?;
    let repo = repository::by_id(pull.repo_id, request).await?;
    request.authorize(repo.id, "pull-requests", true).await?;
    let pool = &request.state.pool;
    let actor = &request.auth.user;
    if !access::can_write(pool, actor, &repo)
        .await
        .map_err(unavailable)?
    {
        return Err(Error::new("Resource not accessible by token"));
    }
    if pull.state != "open" {
        return Err(Error::new("Review requests require an open pull request"));
    }
    let user_ids = ids(&input, "userIds", "U")?;
    let team_ids = ids(&input, "teamIds", "T")?;
    if user_ids.len() + team_ids.len() > 100 {
        return Err(Error::new("At most 100 reviewers are allowed"));
    }
    let eligible = db::pull_requests::team_reviewers::list(pool, repo.id, None, Some(actor.id))
        .await
        .map_err(unavailable)?;
    let mut teams = Vec::new();
    for id in team_ids {
        teams.push(
            eligible
                .iter()
                .find(|row| row.team.id == id)
                .map(|row| row.team.slug.clone())
                .ok_or_else(missing)?,
        );
    }
    let mut users = Vec::new();
    for id in &user_ids {
        let user = db::users::accounts::get_user_by_id(pool, *id)
            .await
            .map_err(unavailable)?
            .ok_or_else(missing)?;
        if user.id == pull.author_id
            || !access::can_write(pool, &user, &repo)
                .await
                .map_err(unavailable)?
        {
            return Err(Error::new(
                "Reviewers require repository write access and cannot be the author",
            ));
        }
        users.push(user);
    }
    let owner = repository::owner(&repo, request).await?;
    let broadcaster = &request.state.notification_broadcaster;
    let rerequest = field == "rerequestReviews";
    let requested = db::pull_requests::reviews::list_pull_request_reviewers(pool, pull.id)
        .await
        .map_err(unavailable)?;
    if !rerequest && !input["union"].as_bool().unwrap_or(false) {
        let old_teams =
            db::pull_requests::team_reviewers::list(pool, repo.id, Some(pull.id), Some(actor.id))
                .await
                .map_err(unavailable)?;
        let removed: Vec<_> = old_teams
            .into_iter()
            .filter(|row| !teams.contains(&row.team.slug))
            .map(|row| row.team.slug)
            .collect();
        for slugs in removed.chunks(100) {
            team_reviewers::change(
                pool,
                broadcaster,
                actor,
                &repo,
                &pull,
                slugs,
                db::pull_requests::team_reviewers::ReviewTeamAction::Remove,
            )
            .await
            .map_err(operation)?;
        }
        for user in &requested {
            if !user_ids.contains(&user.id) {
                reviews::remove_requested_reviewer(
                    pool,
                    actor.id,
                    owner.clone(),
                    repo.name.clone(),
                    pull.number,
                    user.username.clone(),
                )
                .await
                .map_err(operation)?;
            }
        }
    }
    team_reviewers::change(
        pool,
        broadcaster,
        actor,
        &repo,
        &pull,
        &teams,
        if rerequest {
            db::pull_requests::team_reviewers::ReviewTeamAction::Rerequest
        } else {
            db::pull_requests::team_reviewers::ReviewTeamAction::Request
        },
    )
    .await
    .map_err(operation)?;
    for user in users {
        if rerequest && requested.iter().any(|old| old.id == user.id) {
            reviews::remove_requested_reviewer(
                pool,
                actor.id,
                owner.clone(),
                repo.name.clone(),
                pull.number,
                user.username.clone(),
            )
            .await
            .map_err(operation)?;
        }
        reviews::request_reviewer(
            pool,
            broadcaster,
            actor.id,
            owner.clone(),
            repo.name.clone(),
            pull.number,
            reviews::RequestReviewerRequest {
                username: user.username,
            },
        )
        .await
        .map_err(operation)?;
    }
    Ok(
        json!({"clientMutationId":input["clientMutationId"],"actor":user::by_id(actor.id,request).await?,
        "pullRequest":issue::pull_object(&pull),"requestedReviewersEdge":null}),
    )
}
