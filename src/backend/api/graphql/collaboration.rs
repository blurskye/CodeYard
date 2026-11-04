//! Shared issue/PR metadata; all rows come from the REST lane's named operations.

use crate::backend::api::graphql::db;
use crate::backend::api::graphql::missing;
use crate::backend::api::graphql::node_id;

use crate::backend::api::graphql::unavailable;
use crate::backend::api::graphql::url;

use crate::backend::api::graphql::Arguments;
use crate::backend::api::graphql::RequestContext;
use crate::backend::api::graphql::Result;
use crate::backend::api::graphql::Value;
use serde_json::json;

pub(super) async fn milestone(
    summary: codeyard_db_core::models::MilestoneSummary,
    request: &RequestContext,
) -> Result<Value> {
    let row = summary.milestone;
    let repo = crate::backend::api::graphql::repository::by_id(row.repo_id, request).await?;
    Ok(
        json!({"__typename":"Milestone","_repo":row.repo_id,"id":node_id::encode("M",row.id),"databaseId":row.id,
        "closed":row.state=="closed","progressPercentage":if summary.open_items+summary.closed_items==0 {0.0} else {100.0*summary.closed_items as f64/(summary.open_items+summary.closed_items) as f64},
        "creator":match row.creator_id {Some(id)=>crate::backend::api::graphql::user::actor_by_id(id,request).await?,None=>Value::Null},
        "number":row.number,"title":row.title,"description":row.description,"state":row.state.to_uppercase(),
        "createdAt":row.created_at,"updatedAt":row.updated_at,"closedAt":row.closed_at,"dueOn":row.due_on,
        "url":url(&format!("/{}/{}/milestones/{}",crate::backend::api::graphql::repository::owner(&repo,request).await?,repo.name,row.number))}),
    )
}

pub(super) async fn resolve(
    ty: &str,
    field: &str,
    args: &Arguments,
    request: &RequestContext,
    parent: &Value,
    repo: &codeyard_db_core::models::Repository,
) -> Result<Value> {
    let id = parent["databaseId"].as_i64().ok_or_else(missing)?;
    let pool = &request.state.pool;
    match field {
        "repository" => Ok(crate::backend::api::graphql::repository::object(repo)),
        "authorAssociation" => Ok(json!(
            codeyard_identity::repository_access::author_association(
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
                        let mut value =
                            crate::backend::api::graphql::collaboration::milestone(value, request)
                                .await?;
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
                    values.push(crate::backend::api::graphql::user::object(
                        &row,
                        request.auth.user.id,
                    ));
                }
            }
            crate::backend::api::graphql::connection::slice(args, values)
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
            crate::backend::api::graphql::connection::validate(args)?;
            if args.args.try_get("userLinkedOnly")?.deserialize::<bool>()? {
                return crate::backend::api::graphql::connection::slice(args, Vec::new());
            }
            let pull = db::pull_requests::records::get_pull_request_by_id(pool, id)
                .await
                .map_err(unavailable)?
                .ok_or_else(missing)?;
            request
                .authorize(pull.repo_id, "pull-requests", false)
                .await?;
            // As on GitHub, keywords close issues only when the pull request targets the default branch.
            if pull.base_branch != repo.default_branch {
                return crate::backend::api::graphql::connection::slice(args, Vec::new());
            }
            request.charge(pull.body.len() + pull.title.len())?;
            let owner = crate::backend::api::graphql::repository::owner(repo, request).await?;
            let mut groups = std::collections::BTreeMap::<(String, String), Vec<i32>>::new();
            for (owner, name, number) in crate::backend::issues::closing::referenced_targets(
                &pull.body,
                &owner,
                &repo.name,
                &crate::backend::api::graphql::url(""),
            ) {
                request.charge(owner.len() + name.len() + 64)?;
                groups.entry((owner, name)).or_default().push(number);
            }
            let mut targets = Vec::new();
            for ((target_owner, name), numbers) in groups {
                let target = if target_owner.eq_ignore_ascii_case(&owner)
                    && name.eq_ignore_ascii_case(&repo.name)
                {
                    Some(repo.clone())
                } else {
                    db::repositories::records::get_repository(pool, &target_owner, &name)
                        .await
                        .map_err(unavailable)?
                };
                let Some(target) = target else {
                    continue;
                };
                if !crate::backend::api::graphql::repository::optional_authority(
                    target.id, "issues", request,
                )
                .await?
                {
                    continue;
                }
                let viewer = request.private_viewer(target.id, "issues").await?;
                for numbers in numbers.chunks(500) {
                    let rows = db::issues::issue_ids_by_numbers(pool, target.id, viewer, numbers)
                        .await
                        .map_err(unavailable)?;
                    request.charge(rows.len() * 32)?;
                    targets.extend(rows.into_iter().map(|(id, _)| (target.id, id)));
                }
            }
            let page =
                crate::backend::api::graphql::connection::Page::new(args, targets.len() as i64)?;
            let selected: Vec<_> = targets
                .into_iter()
                .skip(page.offset as usize)
                .take(page.limit as usize)
                .collect();
            let mut groups = std::collections::BTreeMap::<i64, Vec<(usize, i64)>>::new();
            for (position, (repo, id)) in selected.iter().copied().enumerate() {
                groups.entry(repo).or_default().push((position, id));
            }
            let mut nodes = vec![Value::Null; selected.len()];
            for (repo, positions) in groups {
                let ids: Vec<_> = positions.iter().map(|(_, id)| *id).collect();
                let rows = db::issues::get_issues_by_ids(pool, repo, &ids)
                    .await
                    .map_err(unavailable)?;
                for row in &rows {
                    request.charge(row.body.len() + row.title.len())?;
                }
                if !crate::backend::api::graphql::repository::optional_authority(
                    repo, "issues", request,
                )
                .await?
                {
                    continue;
                }
                let rows: std::collections::HashMap<_, _> =
                    rows.into_iter().map(|row| (row.id, row)).collect();
                for (position, id) in positions {
                    if let Some(row) = rows.get(&id) {
                        nodes[position] = crate::backend::api::graphql::issue::object(row);
                    }
                }
            }
            Ok(page.finish(nodes))
        }
        // Codeyard does not store manually linked PR/issue relations.
        "subIssues"
        | "blockedBy"
        | "blocking"
        | "closedByPullRequestsReferences"
        | "projectCards"
        | "projectItems" => crate::backend::api::graphql::connection::slice(args, vec![]),
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
    let repo = crate::backend::api::graphql::repository::by_id(
        parent["_repo"].as_i64().ok_or_else(missing)?,
        request,
    )
    .await?;
    match field {
        "authorAssociation" => Ok(json!(
            codeyard_identity::repository_access::author_association(
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
                parent["_comment"].as_i64().ok_or_else(missing)?,
                request,
            )
            .await
        }
        "url" => Ok(json!(url(&format!(
            "/{}/{}/{}/{}?comment={}#comment-{}",
            crate::backend::api::graphql::repository::owner(&repo, request).await?,
            repo.name,
            if parent["_pull"] == true {
                "pull"
            } else {
                "issues"
            },
            parent["_number"],
            parent["_comment"],
            parent["_comment"]
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
