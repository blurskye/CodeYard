//! Pull request Git reads and review presentation from the shared REST operations.
pub(super) async fn review_request(
    pull: &codeyard_db_core::models::PullRequest,
    reviewer: &codeyard_db_core::models::User,
    request: &RequestContext,
) -> Result<Value> {
    Ok(
        json!({"__typename":"ReviewRequest","_repo":pull.repo_id,"_scope":"pull-requests","id":crate::backend::api::graphql::node_id::git("RR",&format!("{}/{}",pull.id,reviewer.id)),"databaseId":null,"pullRequest":crate::backend::api::graphql::issue::pull_object(pull),"requestedReviewer":user::actor_by_id(reviewer.id,request).await?}),
    )
}

use crate::backend::api::graphql::db;

use crate::backend::api::graphql::missing;

use crate::backend::api::graphql::unavailable;

use crate::backend::api::graphql::Arguments;
use crate::backend::api::graphql::RequestContext;
use crate::backend::api::graphql::Result;
use crate::backend::api::graphql::Value;

use crate::backend::repositories::store::repo_store;
use serde_json::json;

async fn mergeable(
    repo: &codeyard_db_core::models::Repository,
    pull: &codeyard_db_core::models::PullRequest,
    request: &RequestContext,
) -> Result<Option<(bool, String)>> {
    use crate::backend::repositories::store::MergeMethod;
    use crate::backend::repositories::store::MergeRequest;
    use codeyard_git::error::ErrorKind;
    let store = repo_store(repo, request.state.cluster.as_ref());
    let base = match store
        .commit_from_primary(&format!("refs/heads/{}", pull.base_branch))
        .await
    {
        Ok(base) => base,
        Err(error) if error.kind == ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(unavailable(error)),
    };
    crate::backend::api::graphql::git::charge_commit(&base, request)?;
    let key = (repo.id, base.sha.clone(), pull.head_sha.clone());
    let mut cache = request.mergeability.lock().await;
    if let Some(value) = cache.get(&key) {
        return Ok(Some((*value, base.sha)));
    }
    let value = match store
        .prepare_merge(&MergeRequest {
            head_branch: pull.head_branch.clone(),
            base_branch: pull.base_branch.clone(),
            head: pull.head_sha.clone(),
            base: base.sha.clone(),
            message: "Check pull request mergeability".into(),
            squash_body: None,
            name: "Codeyard".into(),
            email: "noreply@localhost".into(),
            method: MergeMethod::Merge,
            author_name: None,
            author_email: None,
        })
        .await
    {
        Ok(_) => true,
        Err(error) if error.is_response_too_large() => return Err(unavailable(error)),
        Err(error) if matches!(error.kind, ErrorKind::Conflict | ErrorKind::Invalid) => false,
        Err(error) if error.kind == ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(unavailable(error)),
    };
    cache.insert(key, value);
    Ok(Some((value, base.sha)))
}

pub(super) async fn resolve(
    field: &str,
    args: &Arguments,
    request: &RequestContext,
    parent: &Value,
    repo: &codeyard_db_core::models::Repository,
) -> Result<Value> {
    let mut pull = db::pull_requests::records::get_pull_request_by_id(
        &request.state.pool,
        parent["databaseId"].as_i64().ok_or_else(missing)?,
    )
    .await
    .map_err(unavailable)?
    .ok_or_else(missing)?;
    let store = repo_store(repo, request.state.cluster.as_ref());
    if matches!(field, "mergeable" | "mergeStateStatus")
        && pull.state == "open"
        && (field == "mergeable" || !pull.is_draft)
    {
        if let Some((value, base)) = mergeable(repo, &pull, request).await? {
            pull.mergeable = Some(value);
            pull.base_sha = base;
        }
    }
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
            let (headline, body) = crate::backend::repositories::pull_requests::merge::default_merge_text(&pull, method);
            Ok(json!(if field == "viewerMergeHeadlineText" {
                headline
            } else {
                body
            }))
        }
        "files" | "additions" | "deletions" | "changedFiles" => {
            let mut diffs = request.diffs.lock().await;
            let key = (repo.id, pull.diff_base().to_owned(), pull.head_sha.clone());
            if !diffs.contains_key(&key) {
                let diff = store
                    .diff_stats(&key.1, &key.2)
                    .await
                    .map_err(unavailable)?;
                request.charge(
                    diff.files
                        .iter()
                        .map(|file| file.filename.len() + 128)
                        .sum(),
                )?;
                diffs.insert(key.clone(), std::sync::Arc::new(diff));
            }
            let diff = diffs[&key].clone();
            drop(diffs);
            Ok(match field {
                "additions"=>json!(diff.additions),"deletions"=>json!(diff.deletions),"changedFiles"=>json!(diff.changed_files),
                _=>crate::backend::api::graphql::connection::slice(args,diff.files.iter().map(|f|json!({"path":f.filename,"additions":f.additions,"deletions":f.deletions,"changeType":match f.status.as_str() {"added"=>"ADDED","removed"|"deleted"=>"DELETED","renamed"=>"RENAMED","copied"=>"COPIED","type_changed"=>"CHANGED",_=>"MODIFIED"}})).collect())?,
            })
        }
        "commits" => {
            let (count, _) = store
                .pull_request_commits(pull.diff_base(), &pull.head_sha, 0, 0)
                .await
                .map_err(unavailable)?;
            let page = crate::backend::api::graphql::connection::Page::new(args, count as i64)?;
            let (offset, limit) = (page.offset as usize, page.limit as usize);
            // The store pages within the oldest 250 commits. A page past them, such as
            // `last: 1` for the head commit, reads from the newest end instead.
            let window: Vec<_> = if offset.saturating_add(limit) <= 250 {
                let rows = store
                    .pull_request_commits(pull.diff_base(), &pull.head_sha, offset, limit)
                    .await
                    .map_err(unavailable)?
                    .1;
                for row in &rows {
                    crate::backend::api::graphql::git::charge_commit(row, request)?;
                }
                rows
            } else {
                let newest_offset = (count as usize).saturating_sub(offset.saturating_add(limit));
                let mut rows = store
                    .commits_between_page(pull.diff_base(), &pull.head_sha, newest_offset, limit)
                    .await
                    .map_err(unavailable)?;
                for row in &rows {
                    crate::backend::api::graphql::git::charge_commit(row, request)?;
                }
                rows.reverse();
                rows
            };
            let nodes = window
                .iter()
                .map(|c| {
                    crate::backend::api::graphql::git::commit(repo.id, c, "pull-requests", request)
                        .map(|commit| json!({"commit":commit}))
                })
                .collect::<Result<Vec<_>>>()?;
            Ok(page.finish(nodes))
        }
        "reviews" | "latestReviews" => {
            crate::backend::api::graphql::connection::validate(args)?;
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
                viewer: request.private_viewer(repo.id, "pull-requests").await?,
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
            let page = crate::backend::api::graphql::connection::Page::new(args, total)?;
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
                nodes.push(crate::backend::api::graphql::node_objects::review(&row, request).await?);
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
            let page = crate::backend::api::graphql::connection::Page::new(args, reviewers.len() as i64)?;
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
                        if crate::backend::repositories::pull_requests::merge::page_blocker_messages(
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
        "reviewDecision" => Ok(json!(crate::backend::repositories::pull_requests::merge::review_decision(
            &request.state.pool,
            &request.state.ci_runs,
            repo,
            &pull
        )
        .await
        .map_err(unavailable)?
        .map(|s| match s {
            crate::backend::repositories::pull_requests::merge::ReviewDecision::Approved => "APPROVED",
            crate::backend::repositories::pull_requests::merge::ReviewDecision::ChangesRequested => "CHANGES_REQUESTED",
            crate::backend::repositories::pull_requests::merge::ReviewDecision::Required => "REVIEW_REQUIRED",
        }))),
        "baseRef" => {
            crate::backend::api::graphql::git_objects::reference(
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
        "isCrossRepository" => Ok(json!(pull.head_repo_id != Some(repo.id))),
        "headRepository" | "headRepositoryOwner" => {
            let Some(head_id) = pull.head_repo_id else {
                return Ok(Value::Null);
            };
            if !crate::backend::api::graphql::repository::optional_authority(head_id, "metadata", request).await? {
                return Ok(Value::Null);
            }
            let Some(head) =
                db::repositories::records::get_repository_by_id(&request.state.pool, head_id)
                    .await
                    .map_err(unavailable)?
            else {
                return Ok(Value::Null);
            };
            if !codeyard_identity::repository_access::can_read(&request.state.pool, Some(&request.auth.user), &head)
                .await
                .map_err(unavailable)?
            {
                return Ok(Value::Null);
            }
            if field == "headRepository" {
                Ok(crate::backend::api::graphql::repository::object(&head))
            } else if head.owner_kind == "org" {
                crate::backend::api::graphql::user::organization_by_id(head.owner_id(), request).await
            } else {
                crate::backend::api::graphql::user::by_id(head.owner_id(), request).await
            }
        }
        "mergedBy" => match pull.merged_by_id {
            Some(id) => crate::backend::api::graphql::user::actor_by_id(id, request).await,
            None => Ok(Value::Null),
        },
        "mergeCommit" => match pull.merge_commit_sha {
            Some(oid) => crate::backend::api::graphql::git::load(repo, &oid, "pull-requests", request).await,
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
            let entry = entry.request;
            Ok(
                json!({"__typename":"AutoMergeRequest","_repo":pull.repo_id,"_scope":"pull-requests",
                "authorEmail":entry.author_email,"commitBody":entry.commit_message,
                "commitHeadline":entry.commit_title,"enabledAt":entry.enabled_at,
                "enabledBy":user::actor_by_id(entry.enabled_by, request).await?,
                "mergeMethod":entry.merge_method.to_ascii_uppercase(),
                "pullRequest":crate::backend::api::graphql::issue::pull_object(&pull)}),
            )
        }
        _ => crate::backend::api::graphql::collaboration::resolve("PullRequest", field, args, request, parent, repo).await,
    }
}
