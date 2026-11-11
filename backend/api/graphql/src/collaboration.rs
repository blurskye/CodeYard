//! Shared issue/PR metadata; all rows come from the REST lane's named operations.

use crate::arguments::Arguments;
use crate::missing;
use crate::unavailable;
use crate::url;
use crate::RequestContext;
use crate::Result;
use codeyard_types::node_id;
use serde_json::json;
use serde_json::Value;

pub(super) async fn milestone(
    summary: codeyard_database::models::MilestoneSummary,
    request: &RequestContext,
) -> Result<Value> {
    let row = summary.milestone;
    let repo = crate::repository::by_id(row.repo_id, request).await?;
    Ok(
        json!({"__typename":"Milestone","_repo":row.repo_id,"id":node_id::scoped("M",row.repo_id,row.id),"databaseId":row.id,
        "closed":row.state=="closed","progressPercentage":if summary.open_items+summary.closed_items==0 {0.0} else {100.0*summary.closed_items as f64/(summary.open_items+summary.closed_items) as f64},
        "creator":match row.creator_id {Some(id)=>crate::user::actor_by_id(id,request).await?,None=>Value::Null},
        "number":row.number,"title":row.title,"description":row.description,"state":row.state.to_uppercase(),
        "createdAt":row.created_at,"updatedAt":row.updated_at,"closedAt":row.closed_at,"dueOn":row.due_on,
        "url":url(&format!("/{}/{}/milestones/{}",crate::repository::owner(&repo,request).await?,repo.name,row.number))}),
    )
}

pub(super) async fn resolve(
    ty: &str,
    field: &str,
    args: &Arguments,
    request: &RequestContext,
    parent: &Value,
    repo: &codeyard_database::models::Repository,
) -> Result<Value> {
    let id = parent["databaseId"].as_i64().ok_or_else(missing)?;
    let pool = &request.state.pool;
    match field {
        "repository" => Ok(crate::repository::object(repo)),
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
                Some(id) => {
                    match codeyard_db_repos::milestones::get_milestone_by_id(pool, repo.id, id)
                        .await
                        .map_err(unavailable)?
                    {
                        Some(value) => {
                            let mut value = crate::collaboration::milestone(value, request).await?;
                            value["_scope"] = json!(if ty == "PullRequest" {
                                "pull-requests"
                            } else {
                                "issues"
                            });
                            Ok(value)
                        }
                        None => Ok(Value::Null),
                    }
                }
                None => Ok(Value::Null),
            }
        }
        "assignees" | "assignedActors" => {
            let item = if ty == "Issue" {
                codeyard_db_repos::assignees::AssignedItem::Issue(id)
            } else {
                codeyard_db_repos::assignees::AssignedItem::PullRequest(id)
            };
            let names = codeyard_db_repos::assignees::list_assignees(pool, item)
                .await
                .map_err(unavailable)?;
            let mut values = vec![];
            for name in names {
                if let Some(row) =
                    codeyard_db_identity::users::accounts::get_user_by_username(pool, &name)
                        .await
                        .map_err(unavailable)?
                {
                    values.push(crate::user::object(&row, request.auth.user.id));
                }
            }
            crate::connection::slice(args, values)
        }
        "reactionGroups" => {
            reactions(
                if ty == "Issue" {
                    codeyard_db_repos::reactions::Subject::Issue
                } else {
                    codeyard_db_repos::reactions::Subject::PullRequest
                },
                id,
                request,
            )
            .await
        }
        "closingIssuesReferences" => {
            crate::connection::validate(args)?;
            if args.args.try_get("userLinkedOnly")?.deserialize::<bool>()? {
                return crate::connection::slice(args, Vec::new());
            }
            let pull = codeyard_db_repos::pull_requests::records::get_pull_request_by_id(pool, id)
                .await
                .map_err(unavailable)?
                .ok_or_else(missing)?;
            request
                .authorize(pull.repo_id, "pull-requests", false)
                .await?;
            // As on GitHub, keywords close issues only when the pull request targets the default branch.
            if pull.base_branch != repo.default_branch {
                return crate::connection::slice(args, Vec::new());
            }
            request.charge(pull.body.len() + pull.title.len())?;
            let owner = crate::repository::owner(repo, request).await?;
            let mut groups = std::collections::BTreeMap::<(String, String), Vec<i32>>::new();
            for (owner, name, number) in codeyard_repositories::issues::closing::referenced_targets(
                &pull.body,
                &owner,
                &repo.name,
                &crate::url(""),
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
                    codeyard_db_repos::repositories::records::get_repository(
                        pool,
                        &target_owner,
                        &name,
                    )
                    .await
                    .map_err(unavailable)?
                };
                let Some(target) = target else {
                    continue;
                };
                if !crate::repository::optional_authority(target.id, "issues", request).await? {
                    continue;
                }
                let viewer = request.private_viewer(target.id, "issues").await?;
                for numbers in numbers.chunks(500) {
                    let rows = codeyard_db_repos::issues::issue_ids_by_numbers(
                        pool, target.id, viewer, numbers,
                    )
                    .await
                    .map_err(unavailable)?;
                    request.charge(rows.len() * 32)?;
                    targets.extend(rows.into_iter().map(|(id, _)| (target.id, id)));
                }
            }
            let page = crate::connection::Page::new(args, targets.len() as i64)?;
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
                let rows = codeyard_db_repos::issues::get_issues_by_ids(pool, repo, &ids)
                    .await
                    .map_err(unavailable)?;
                for row in &rows {
                    request.charge(row.body.len() + row.title.len())?;
                }
                if !crate::repository::optional_authority(repo, "issues", request).await? {
                    continue;
                }
                let rows: std::collections::HashMap<_, _> =
                    rows.into_iter().map(|row| (row.id, row)).collect();
                for (position, id) in positions {
                    if let Some(row) = rows.get(&id) {
                        nodes[position] = crate::issue::object(row);
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
        | "projectItems" => crate::connection::slice(args, vec![]),
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
    let repo =
        crate::repository::by_id(parent["_repo"].as_i64().ok_or_else(missing)?, request).await?;
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
                    codeyard_db_repos::reactions::Subject::PullRequestComment
                } else {
                    codeyard_db_repos::reactions::Subject::IssueComment
                },
                parent["_comment"].as_i64().ok_or_else(missing)?,
                request,
            )
            .await
        }
        "url" => Ok(json!(url(&format!(
            "/{}/{}/{}/{}?comment={}#comment-{}",
            crate::repository::owner(&repo, request).await?,
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
    subject: codeyard_db_repos::reactions::Subject,
    id: i64,
    request: &RequestContext,
) -> Result<Value> {
    let counts = codeyard_db_repos::reactions::reaction_counts(
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
