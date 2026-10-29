//! Pull request Git reads and review presentation from the shared REST operations.
pub(super) async fn review_request(
    pull: &db::models::PullRequest,
    reviewer: &db::models::User,
    request: &RequestContext,
) -> Result<Value> {
    Ok(
        json!({"__typename":"ReviewRequest","_repo":pull.repo_id,"_scope":"pull-requests","id":super::node_id::git("RR",&format!("{}/{}",pull.id,reviewer.id)),"databaseId":null,"pullRequest":super::issue::pull_object(pull),"requestedReviewer":user::actor_by_id(reviewer.id,request).await?}),
    )
}
use super::{
    connection, db, git, missing, repository, unavailable, user, Arguments, RequestContext, Result,
    Value,
};
use crate::backend::repositories::{access, pull_requests::merge, store::repo_store};
use serde_json::json;

pub(super) async fn resolve(
    field: &str,
    args: &Arguments,
    request: &RequestContext,
    parent: &Value,
    repo: &db::models::Repository,
) -> Result<Value> {
    let pull = db::pull_requests::records::get_pull_request_by_id(
        &request.state.pool,
        parent["databaseId"].as_i64().ok_or_else(missing)?,
    )
    .await
    .map_err(unavailable)?
    .ok_or_else(missing)?;
    let store = repo_store(repo, request.state.cluster.as_ref());
    match field {
        "viewerMergeHeadlineText" | "viewerMergeBodyText" => {
            let method = args
                .args
                .get("mergeType")
                .filter(|value| !value.is_null())
                .map(|value| value.enum_name())
                .transpose()?
                .unwrap_or("MERGE");
            let method = crate::backend::repositories::store::MergeMethod::parse(
                &method.to_ascii_lowercase(),
            )
            .ok_or_else(|| async_graphql::Error::new("Invalid merge method"))?;
            let (headline, body) = merge::default_merge_text(&pull, method);
            Ok(json!(if field == "viewerMergeHeadlineText" {
                headline
            } else {
                body
            }))
        }
        "files" | "additions" | "deletions" | "changedFiles" => {
            let diff = store
                .diff_stats(pull.diff_base(), &pull.head_sha)
                .await
                .map_err(unavailable)?;
            Ok(match field {
                "additions"=>json!(diff.additions),"deletions"=>json!(diff.deletions),"changedFiles"=>json!(diff.changed_files),
                _=>connection::slice(args,diff.files.into_iter().map(|f|json!({"path":f.filename,"additions":f.additions,"deletions":f.deletions,"changeType":match f.status.as_str() {"added"=>"ADDED","removed"|"deleted"=>"DELETED","renamed"=>"RENAMED","copied"=>"COPIED",_=>"MODIFIED"}})).collect())?,
            })
        }
        "commits" => {
            let (count, rows) = store
                .pull_request_commits(pull.diff_base(), &pull.head_sha)
                .await
                .map_err(unavailable)?;
            let page = connection::Page::new(args, count as i64)?;
            let (offset, limit) = (page.offset as usize, page.limit as usize);
            // The store lists the oldest 250 commits. A page past them, such as `last: 1` for
            // the head commit, reads from the newest end of the range instead.
            let window: Vec<_> = if offset + limit <= rows.len() {
                rows.into_iter().skip(offset).take(limit).collect()
            } else {
                let newest = (count as usize).saturating_sub(offset);
                if newest > 250 {
                    return Err(async_graphql::Error::new(
                        "Read commits past the first 250 from the end of the list",
                    ));
                }
                // Newest first, from the head back to the page's offset.
                let mut rows = store
                    .commits_between(pull.diff_base(), &pull.head_sha, newest)
                    .await
                    .map_err(unavailable)?;
                rows.reverse();
                rows.truncate(limit);
                rows
            };
            let nodes = window
                .iter()
                .map(|c| {
                    git::commit(repo.id, c, "pull-requests").map(|commit| json!({"commit":commit}))
                })
                .collect::<Result<Vec<_>>>()?;
            Ok(page.finish(nodes))
        }
        "reviews" | "latestReviews" => {
            connection::validate(args)?;
            let states = args
                .args
                .get("states")
                .filter(|v| !v.is_null())
                .map(|v| v.deserialize::<Vec<String>>())
                .transpose()?
                .unwrap_or_default()
                .into_iter()
                .map(|s| s.to_ascii_lowercase())
                .collect();
            let query = db::pull_requests::reviews::ReviewQuery {
                pull_id: pull.id,
                viewer: Some(request.auth.user.id),
                author: args
                    .args
                    .get("author")
                    .filter(|v| !v.is_null())
                    .map(|v| v.string().map(str::to_owned))
                    .transpose()?,
                states,
                latest: field == "latestReviews",
            };
            let total =
                db::pull_requests::reviews::count_filtered_reviews(&request.state.pool, &query)
                    .await
                    .map_err(unavailable)?;
            let page = connection::Page::new(args, total)?;
            let rows = db::pull_requests::reviews::filter_reviews(
                &request.state.pool,
                &query,
                page.limit,
                page.offset,
            )
            .await
            .map_err(unavailable)?;
            let mut nodes = vec![];
            for row in rows {
                nodes.push(super::node_objects::review(&row, request).await?);
            }
            Ok(page.finish(nodes))
        }
        "reviewRequests" => {
            let reviewers = db::pull_requests::reviews::list_pull_request_reviewers(
                &request.state.pool,
                pull.id,
            )
            .await
            .map_err(unavailable)?;
            let page = connection::Page::new(args, reviewers.len() as i64)?;
            let mut nodes = vec![];
            for row in reviewers
                .into_iter()
                .skip(page.offset as usize)
                .take(page.limit as usize)
            {
                nodes.push(review_request(&pull, &row, request).await?);
            }
            Ok(page.finish(nodes))
        }
        "mergeable" => Ok(json!(match pull.mergeable {
            Some(true) => "MERGEABLE",
            Some(false) => "CONFLICTING",
            None => "UNKNOWN",
        })),
        "mergeStateStatus" => {
            let state = if pull.merged {
                "CLEAN"
            } else if pull.is_draft {
                "DRAFT"
            } else {
                match pull.mergeable {
                    Some(false) => "DIRTY",
                    None => "UNKNOWN",
                    Some(true) => {
                        let method = if repo.allow_merge_commit {
                            crate::backend::repositories::store::MergeMethod::Merge
                        } else if repo.allow_squash_merge {
                            crate::backend::repositories::store::MergeMethod::Squash
                        } else {
                            crate::backend::repositories::store::MergeMethod::Rebase
                        };
                        if merge::page_blocker_messages(
                            &request.state.pool,
                            &request.state.ci_runs,
                            repo,
                            &request.auth.user,
                            &pull,
                            method,
                        )
                        .await
                        .map_err(unavailable)?
                        .is_empty()
                        {
                            "CLEAN"
                        } else {
                            "BLOCKED"
                        }
                    }
                }
            };
            Ok(json!(state))
        }
        "reviewDecision" => Ok(json!(merge::review_decision(
            &request.state.pool,
            &request.state.ci_runs,
            repo,
            &pull
        )
        .await
        .map_err(unavailable)?
        .map(|s| match s {
            merge::ReviewDecision::Approved => "APPROVED",
            merge::ReviewDecision::ChangesRequested => "CHANGES_REQUESTED",
            merge::ReviewDecision::Required => "REVIEW_REQUIRED",
        }))),
        "baseRef" => {
            super::git_objects::reference(
                repo,
                &format!("refs/heads/{}", pull.base_branch),
                request,
            )
            .await
        }
        // Codeyard has no merge queue; ordinary merges still enforce branch protection.
        "isInMergeQueue" | "isMergeQueueEnabled" => Ok(json!(false)),
        "baseRefOid" => Ok(json!(pull.base_sha)),
        "headRefOid" => Ok(json!(pull.head_sha)),
        "fullDatabaseId" => Ok(json!(pull.id.to_string())),
        "isCrossRepository" => Ok(json!(pull.head_repo_id.is_some_and(|id| id != repo.id))),
        "headRepository" | "headRepositoryOwner" => {
            let Some(head_id) = pull.head_repo_id else {
                return Ok(Value::Null);
            };
            request.authorize(head_id, "metadata", false).await?;
            let Some(head) =
                db::repositories::records::get_repository_by_id(&request.state.pool, head_id)
                    .await
                    .map_err(unavailable)?
            else {
                return Ok(Value::Null);
            };
            if !access::can_read(&request.state.pool, Some(&request.auth.user), &head)
                .await
                .map_err(unavailable)?
            {
                return Ok(Value::Null);
            }
            if field == "headRepository" {
                Ok(repository::object(&head))
            } else if head.owner_kind == "org" {
                user::organization_by_id(head.owner_id(), request).await
            } else {
                user::by_id(head.owner_id(), request).await
            }
        }
        "mergedBy" => match pull.merged_by_id {
            Some(id) => user::actor_by_id(id, request).await,
            None => Ok(Value::Null),
        },
        "mergeCommit" => match pull.merge_commit_sha {
            Some(oid) => git::load(repo, &oid, "pull-requests", request).await,
            None => Ok(Value::Null),
        },
        // No speculative merge commit is stored by Codeyard.
        "potentialMergeCommit" => Ok(Value::Null),
        "autoMergeRequest" => {
            let Some(entry) =
                db::pull_requests::auto_merge::get_auto_merge(&request.state.pool, pull.id)
                    .await
                    .map_err(unavailable)?
            else {
                return Ok(Value::Null);
            };
            Ok(
                json!({"__typename":"AutoMergeRequest","_repo":pull.repo_id,"_scope":"pull-requests",
                "authorEmail":entry.author_email,"commitBody":entry.commit_message,
                "commitHeadline":entry.commit_title,"enabledAt":entry.enabled_at,
                "enabledBy":user::actor_by_id(entry.enabled_by, request).await?,
                "mergeMethod":entry.merge_method.to_ascii_uppercase(),
                "pullRequest":super::issue::pull_object(&pull)}),
            )
        }
        _ => super::collaboration::resolve("PullRequest", field, args, request, parent, repo).await,
    }
}
