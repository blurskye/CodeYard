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

async fn roster(id: i64, request: &RequestContext) -> Result<(db::models::PullRequest, Vec<db::pull_requests::reviews::ReviewRequestOrigin>, std::collections::HashMap<i64, db::models::Team>)> {
    let pool = &request.state.pool;
    let pull = pull(id, request).await?;
    let teams: std::collections::HashMap<_, _> = db::pull_requests::team_reviewers::list(pool, pull.repo_id, Some(id), request.private_viewer(pull.repo_id, "pull-requests").await?)
        .await.map_err(unavailable)?.into_iter().map(|row| (row.team.id, row.team)).collect();
    let mut origins = db::pull_requests::reviews::review_request_origins(pool, id).await.map_err(unavailable)?;
    origins.retain(|origin| origin.user_id.is_some() || origin.team_id.is_some_and(|id| teams.contains_key(&id)));
    Ok((pull, origins, teams))
}

async fn objects(pull: &db::models::PullRequest, origins: &[db::pull_requests::reviews::ReviewRequestOrigin], teams: &std::collections::HashMap<i64, db::models::Team>, request: &RequestContext) -> Result<Vec<Value>> {
    let ids: Vec<_> = origins.iter().filter_map(|origin| origin.user_id).collect();
    let users: std::collections::HashMap<_, _> = db::users::accounts::get_users_by_ids(&request.state.pool, &ids).await.map_err(unavailable)?.into_iter().map(|user| (user.id, user)).collect();
    let mut nodes = Vec::new();
    for origin in origins {
        let (team, id, reviewer) = match (origin.user_id, origin.team_id) {
            (Some(id), None) => (false, id, users.get(&id).filter(|user| !user.is_banned).map_or(Value::Null, |user| user::actor_object(user, request.auth.user.id))),
            (None, Some(id)) => (true, id, team_object(teams.get(&id).ok_or_else(missing)?, request).await?),
            _ => return Err(missing()),
        };
        nodes.push(json!({"__typename":"ReviewRequest","_repo":pull.repo_id,"_scope":"pull-requests","id":node_id::review_request(pull.id,team,id),
            "databaseId":null,"asCodeOwner":origin.as_code_owner,"pullRequest":issue::pull_object(pull),"requestedReviewer":reviewer}));
    }
    Ok(nodes)
}

pub(super) async fn nodes(id: i64, request: &RequestContext) -> Result<Vec<Value>> {
    let (pull, origins, teams) = roster(id, request).await?;
    let mut nodes = Vec::new();
    for page in origins.chunks(100) { nodes.extend(objects(&pull, page, &teams, request).await?); }
    Ok(nodes)
}

pub(super) async fn list(ctx: &Arguments, request: &RequestContext, id: i64) -> Result<Value> {
    connection::validate(ctx)?;
    let (pull, origins, teams) = roster(id, request).await?;
    let page = connection::Page::new(ctx, origins.len() as i64)?;
    let start = page.offset as usize;
    let end = (start + page.limit as usize).min(origins.len());
    Ok(page.finish(objects(&pull, &origins[start..end], &teams, request).await?))
}

pub(super) async fn node(id: &str, request: &RequestContext) -> Result<Value> {
    let (pull, team, target) = node_id::decode_review_request(id).ok_or_else(missing)?;
    let (pull, origins, teams) = roster(pull, request).await?;
    let origin = origins.iter().find(|origin| if team { origin.team_id == Some(target) } else { origin.user_id == Some(target) }).ok_or_else(missing)?;
    Ok(objects(&pull, std::slice::from_ref(origin), &teams, request).await?.remove(0))
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
