//! Pull review requests project the native user/team records and use the same mutations as REST.
use crate::backend::api::graphql::mutations::pull_error as operation;

use crate::backend::api::graphql::db;

use crate::backend::api::graphql::missing;
use crate::backend::api::graphql::node_id;

use crate::backend::api::graphql::unavailable;
use crate::backend::api::graphql::url;

use crate::backend::api::graphql::Arguments;
use crate::backend::api::graphql::RequestContext;
use crate::backend::api::graphql::Result;
use crate::backend::api::graphql::Value;

use async_graphql::Error;
use serde_json::json;

pub(super) async fn team_object(
    team: &codeyard_db_core::models::Team,
    request: &RequestContext,
) -> Result<Value> {
    let organization =
        crate::backend::api::graphql::user::organization_by_id(team.org_id, request).await?;
    let org = organization["login"]
        .as_str()
        .ok_or_else(missing)?
        .to_owned();
    Ok(
        json!({"__typename":"Team","id":node_id::encode("T",team.id),"databaseId":team.id,
        "name":team.name,"slug":team.slug,"description":team.description,"createdAt":team.created_at,
        "updatedAt":team.updated_at,"privacy":if team.privacy=="secret" {"SECRET"} else {"VISIBLE"},
        "combinedSlug":format!("{org}/{}",team.slug),"organization":organization,
        "url":url(&format!("/orgs/{}/teams/{}",codeyard_webui::encode_component(&org),codeyard_webui::encode_component(&team.slug)))}),
    )
}

async fn pull(id: i64, request: &RequestContext) -> Result<codeyard_db_core::models::PullRequest> {
    let pull = db::pull_requests::records::get_pull_request_by_id(&request.state.pool, id)
        .await
        .map_err(unavailable)?
        .ok_or_else(missing)?;
    // Reviewer requests belong to the base repository, which stays readable after its fork is deleted.
    crate::backend::api::graphql::repository::by_id(pull.repo_id, request).await?;
    request
        .authorize(pull.repo_id, "pull-requests", false)
        .await?;
    request.charge(pull.body.len() + pull.title.len())?;
    Ok(pull)
}

async fn window(
    pull: &codeyard_db_core::models::PullRequest,
    request: &RequestContext,
) -> Result<db::pull_requests::team_reviewers::RequestWindow> {
    Ok(db::pull_requests::team_reviewers::RequestWindow {
        pull: pull.id,
        repo: pull.repo_id,
        viewer: request
            .private_viewer(pull.repo_id, "pull-requests")
            .await?,
        target: None,
        offset: 0,
        limit: 0,
        count: true,
    })
}

async fn objects(
    pull: &codeyard_db_core::models::PullRequest,
    origins: &[db::pull_requests::reviews::ReviewRequestOrigin],
    viewer: Option<i64>,
    request: &RequestContext,
) -> Result<Vec<Value>> {
    request.charge(origins.len() * 64)?;
    let pool = &request.state.pool;
    let ids: Vec<_> = origins.iter().filter_map(|origin| origin.user_id).collect();
    let users: std::collections::HashMap<_, _> = if ids.is_empty() {
        std::collections::HashMap::new()
    } else {
        db::users::accounts::get_users_by_ids(pool, &ids)
            .await
            .map_err(unavailable)?
            .into_iter()
            .map(|user| (user.id, user))
            .collect()
    };
    let ids: Vec<_> = origins.iter().filter_map(|origin| origin.team_id).collect();
    let teams: std::collections::HashMap<_, _> = if ids.is_empty() {
        std::collections::HashMap::new()
    } else {
        db::pull_requests::team_reviewers::by_ids(pool, pull.repo_id, viewer, &ids)
            .await
            .map_err(unavailable)?
            .into_iter()
            .map(|team| (team.id, team))
            .collect()
    };
    let mut nodes = Vec::new();
    for origin in origins {
        let (team, id, reviewer) = match (origin.user_id, origin.team_id) {
            (Some(id), None) => (
                false,
                id,
                users
                    .get(&id)
                    .filter(|user| !user.is_banned)
                    .map_or(Value::Null, |user| {
                        crate::backend::api::graphql::user::actor_object(user, request.auth.user.id)
                    }),
            ),
            (None, Some(id)) => (
                true,
                id,
                match teams.get(&id) {
                    Some(team) => team_object(team, request).await?,
                    None => Value::Null,
                },
            ),
            _ => return Err(missing()),
        };
        let node = json!({"__typename":"ReviewRequest","_repo":pull.repo_id,"_scope":"pull-requests","_pull":pull.id,"id":node_id::review_request(pull.id,team,id),
            "databaseId":null,"asCodeOwner":origin.as_code_owner,"requestedReviewer":reviewer});
        request.charge_value(&node)?;
        nodes.push(node);
    }
    if request
        .private_viewer(pull.repo_id, "pull-requests")
        .await?
        != viewer
    {
        return Err(missing());
    }
    Ok(nodes)
}

pub(super) async fn count(id: i64, request: &RequestContext) -> Result<i64> {
    let pull = pull(id, request).await?;
    let window = window(&pull, request).await?;
    let total = db::pull_requests::team_reviewers::request_page(&request.state.pool, &window)
        .await
        .map_err(unavailable)?
        .0;
    if request
        .private_viewer(pull.repo_id, "pull-requests")
        .await?
        != window.viewer
    {
        return Err(missing());
    }
    Ok(total)
}

pub(super) async fn list(ctx: &Arguments, request: &RequestContext, id: i64) -> Result<Value> {
    crate::backend::api::graphql::connection::validate(ctx)?;
    let pull = pull(id, request).await?;
    let mut window = window(&pull, request).await?;
    let total = db::pull_requests::team_reviewers::request_page(&request.state.pool, &window)
        .await
        .map_err(unavailable)?
        .0;
    let page = crate::backend::api::graphql::connection::Page::new(ctx, total)?;
    window.offset = page.offset;
    window.limit = page.limit;
    window.count = false;
    let rows = if page.limit == 0 {
        Vec::new()
    } else {
        db::pull_requests::team_reviewers::request_page(&request.state.pool, &window)
            .await
            .map_err(unavailable)?
            .1
    };
    Ok(page.finish(objects(&pull, &rows, window.viewer, request).await?))
}

pub(super) async fn node(id: &str, request: &RequestContext) -> Result<Value> {
    let (id, team, target) = node_id::decode_review_request(id).ok_or_else(missing)?;
    let pull = pull(id, request).await?;
    let mut window = window(&pull, request).await?;
    window.target = Some((team, target));
    window.count = false;
    window.limit = 1;
    let rows = db::pull_requests::team_reviewers::request_page(&request.state.pool, &window)
        .await
        .map_err(unavailable)?
        .1;
    objects(&pull, &rows, window.viewer, request)
        .await?
        .into_iter()
        .next()
        .ok_or_else(missing)
}

pub(super) async fn pull_projection(id: i64, request: &RequestContext) -> Result<Value> {
    Ok(crate::backend::api::graphql::issue::pull_object(
        &pull(id, request).await?,
    ))
}

pub(super) async fn reviewers(ctx: &Arguments, request: &RequestContext, id: i64) -> Result<Value> {
    let mut value = list(ctx, request, id).await?;
    if let Some(nodes) = value["nodes"].as_array_mut() {
        for node in nodes {
            *node = node["requestedReviewer"].take();
        }
    }
    if let Some(edges) = value["edges"].as_array_mut() {
        for edge in edges {
            edge["node"] = edge["node"]["requestedReviewer"].take();
        }
    }
    Ok(value)
}

fn ids(input: &Value, field: &str, kind: &str) -> Result<Vec<i64>> {
    // A single ID is a valid list input in GraphQL.
    let mut ids = crate::backend::api::graphql::metadata_mutations::ids(&input[field], kind)?;
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
    if !input["botIds"].is_null()
        && input["botIds"]
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
    let repo = crate::backend::api::graphql::repository::by_id(pull.repo_id, request).await?;
    request.authorize(repo.id, "pull-requests", true).await?;
    let pool = &request.state.pool;
    let actor = &request.auth.user;
    if !crate::backend::repositories::access::can_write(pool, actor, &repo)
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
    crate::backend::repositories::pull_requests::team_reviewers::set_requests(
        pool,
        &request.state.notification_broadcaster,
        actor,
        &repo,
        &pull,
        &db::pull_requests::team_reviewers::ReviewRequests {
            users: &user_ids,
            teams: &team_ids,
            union: input["union"].as_bool().unwrap_or(false),
            rerequest: field == "rerequestReviews",
            token: Some(request.auth.token.id),
        },
    )
    .await
    .map_err(operation)?;
    Ok(
        json!({"clientMutationId":input["clientMutationId"],"actor":user::by_id(actor.id,request).await?,
        "pullRequest":issue::pull_object(&pull),"requestedReviewersEdge":null}),
    )
}
