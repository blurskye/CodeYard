//! Shared issue/PR metadata; all rows come from the REST lane's named operations.
use super::{
    connection, db, missing, node_id, repository, unavailable, url, user, Arguments,
    RequestContext, Result, Value,
};
use serde_json::json;

pub(super) async fn milestone(
    summary: db::models::MilestoneSummary,
    request: &RequestContext,
) -> Result<Value> {
    let row = summary.milestone;
    let repo = repository::by_id(row.repo_id, request).await?;
    Ok(
        json!({"__typename":"Milestone","_repo":row.repo_id,"id":node_id::encode("M",row.id),"databaseId":row.id,
        "closed":row.state=="closed","progressPercentage":if summary.open_items+summary.closed_items==0 {0.0} else {100.0*summary.closed_items as f64/(summary.open_items+summary.closed_items) as f64},
        "creator":match row.creator_id {Some(id)=>user::actor_by_id(id,request).await?,None=>Value::Null},
        "number":row.number,"title":row.title,"description":row.description,"state":row.state.to_uppercase(),
        "createdAt":row.created_at,"updatedAt":row.updated_at,"closedAt":row.closed_at,"dueOn":row.due_on,
        "url":url(&format!("/{}/{}/milestone/{}",repository::owner(&repo,request).await?,repo.name,row.number))}),
    )
}

pub(super) async fn resolve(
    ty: &str,
    field: &str,
    args: &Arguments,
    request: &RequestContext,
    parent: &Value,
    repo: &db::models::Repository,
) -> Result<Value> {
    let id = parent["databaseId"].as_i64().ok_or_else(missing)?;
    let pool = &request.state.pool;
    match field {
        "repository" => Ok(repository::object(repo)),
        "authorAssociation" => Ok(json!(
            crate::backend::repositories::access::author_association(
                pool,
                repo,
                parent["_author"].as_i64().ok_or_else(missing)?
            )
            .await
            .map_err(unavailable)?
        )),
        "milestone" => {
            let milestone = parent["_milestone"].as_i64();
            match milestone {
                Some(id) => match db::milestones::get_milestone_by_id(pool, repo.id, id)
                    .await
                    .map_err(unavailable)?
                {
                    Some(value) => {
                        let mut value = self::milestone(value, request).await?;
                        value["_scope"] = json!(if ty == "PullRequest" {
                            "pull-requests"
                        } else {
                            "issues"
                        });
                        Ok(value)
                    }
                    None => Ok(Value::Null),
                },
                None => Ok(Value::Null),
            }
        }
        "assignees" | "assignedActors" => {
            let item = if ty == "Issue" {
                db::assignees::AssignedItem::Issue(id)
            } else {
                db::assignees::AssignedItem::PullRequest(id)
            };
            let names = db::assignees::list_assignees(pool, item)
                .await
                .map_err(unavailable)?;
            let mut values = vec![];
            for name in names {
                if let Some(row) = db::users::accounts::get_user_by_username(pool, &name)
                    .await
                    .map_err(unavailable)?
                {
                    values.push(user::object(&row, request.auth.user.id));
                }
            }
            connection::slice(args, values)
        }
        "reactionGroups" => {
            reactions(
                if ty == "Issue" {
                    db::reactions::Subject::Issue
                } else {
                    db::reactions::Subject::PullRequest
                },
                id,
                request,
            )
            .await
        }
        "closingIssuesReferences" => {
            connection::validate(args)?;
            let pull = db::pull_requests::records::get_pull_request_by_id(pool, id)
                .await
                .map_err(unavailable)?
                .ok_or_else(missing)?;
            let mut issues = Vec::new();
            // Only the same repository's issues are named, as the closing-keyword reader links them.
            if !args.args.try_get("userLinkedOnly")?.deserialize::<bool>()? {
                let owner = repository::owner(repo, request).await?;
                for number in crate::backend::issues::closing::referenced_numbers(
                    &pull.body,
                    &owner,
                    &repo.name,
                    &super::url(""),
                ) {
                    if let Some(issue) = db::issues::get_issue(pool, repo.id, number)
                        .await
                        .map_err(unavailable)?
                    {
                        request.authorize(repo.id, "issues", false).await?;
                        issues.push(super::issue::object(&issue));
                    }
                }
            }
            connection::slice(args, issues)
        }
        // Codeyard does not store manually linked PR/issue relations.
        "subIssues"
        | "blockedBy"
        | "blocking"
        | "closedByPullRequestsReferences"
        | "projectCards"
        | "projectItems" => connection::slice(args, vec![]),
        "isPinned" => Ok(json!(false)),
        "subIssuesSummary" => Ok(json!({"total":0,"completed":0,"percentCompleted":0})),
        "issueType" | "parent" => Ok(Value::Null),
        _ => Err(async_graphql::Error::new(format!(
            "Codeyard does not support {ty}.{field} yet"
        ))),
    }
}

pub(super) async fn resolve_comment(
    field: &str,
    request: &RequestContext,
    parent: &Value,
) -> Result<Value> {
    if let Some(value) = parent.get(field) {
        return Ok(value.clone());
    }
    let repo = repository::by_id(parent["_repo"].as_i64().ok_or_else(missing)?, request).await?;
    match field {
        "authorAssociation" => Ok(json!(
            crate::backend::repositories::access::author_association(
                &request.state.pool,
                &repo,
                parent["_author"].as_i64().ok_or_else(missing)?
            )
            .await
            .map_err(unavailable)?
        )),
        "reactionGroups" => {
            reactions(
                if parent["_pull"] == true {
                    db::reactions::Subject::PullRequestComment
                } else {
                    db::reactions::Subject::IssueComment
                },
                parent["databaseId"].as_i64().ok_or_else(missing)?,
                request,
            )
            .await
        }
        "url" => Ok(json!(url(&format!(
            "/{}/{}/{}/{}#issuecomment-{}",
            repository::owner(&repo, request).await?,
            repo.name,
            if parent["_pull"] == true {
                "pull"
            } else {
                "issues"
            },
            parent["_number"],
            parent["databaseId"]
        )))),
        _ => Err(async_graphql::Error::new(format!(
            "Codeyard does not support IssueComment.{field} yet"
        ))),
    }
}

pub(super) async fn reactions(
    subject: db::reactions::Subject,
    id: i64,
    request: &RequestContext,
) -> Result<Value> {
    let counts = db::reactions::reaction_counts(
        &request.state.pool,
        subject.name(),
        &[id],
        request.auth.user.id,
    )
    .await
    .map_err(unavailable)?;
    Ok(json!([("+1","THUMBS_UP"),("-1","THUMBS_DOWN"),("laugh","LAUGH"),("hooray","HOORAY"),("confused","CONFUSED"),("heart","HEART"),("rocket","ROCKET"),("eyes","EYES")].into_iter().map(|(stored,content)| {
        let count = counts.iter().find(|row| row.content == stored);
        json!({"content":content,"viewerHasReacted":count.is_some_and(|row|row.mine),"users":{"totalCount":count.map_or(0,|row|row.count)}})
    }).collect::<Vec<_>>()))
}
