//! REST node families reconstructed only after repository and family-scope authorization.
use crate::missing;
use crate::unavailable;
use crate::url;
use crate::RequestContext;
use crate::Result;
use codeyard_identity::repository_access as access;
use codeyard_model::node_id;
use serde_json::json;
use serde_json::Value;

pub(super) async fn review(
    row: &codeyard_db_core::models::PullRequestReview,
    request: &RequestContext,
) -> Result<Value> {
    let pull = codeyard_db_repos::pull_requests::records::get_pull_request_by_id(
        &request.state.pool,
        row.pull_request_id,
    )
    .await
    .map_err(unavailable)?
    .ok_or_else(missing)?;
    let repo = crate::repository::by_id(pull.repo_id, request).await?;
    request.authorize(repo.id, "pull-requests", false).await?;
    if row.state == "pending"
        && request.private_viewer(repo.id, "pull-requests").await? != Some(row.author_id)
    {
        return Err(missing());
    }
    Ok(
        json!({"__typename":"PullRequestReview","_repo":repo.id,"_scope":"pull-requests","_review":row.id,
        "id":node_id::scoped("PRR",repo.id,row.id),"databaseId":row.id,"body":row.body.as_deref().unwrap_or(""),"state":row.state.to_uppercase(),
        "createdAt":row.created_at,"submittedAt":if row.state=="pending" {None} else {Some(row.created_at)},
        "author":crate::user::retained_author(row.author_id,request).await?,"authorAssociation":access::author_association(&request.state.pool,&repo,row.author_id).await.map_err(unavailable)?,
        "commit":row.commit_sha.as_ref().map(|oid|json!({"__typename":"Commit","oid":oid,"_repo":repo.id,"_scope":"pull-requests"})),
        "pullRequest":crate::issue::pull_object(&pull),"url":url(&format!("/{}/{}/pulls/{}?review={}#review-{}",crate::repository::owner(&repo,request).await?,repo.name,pull.number,row.id,row.id))}),
    )
}

pub(super) async fn review_comment(
    row: &codeyard_db_core::models::PullRequestComment,
    request: &RequestContext,
) -> Result<Value> {
    let pull = codeyard_db_repos::pull_requests::records::get_pull_request_by_id(
        &request.state.pool,
        row.pull_request_id,
    )
    .await
    .map_err(unavailable)?
    .ok_or_else(missing)?;
    let repo = crate::repository::by_id(pull.repo_id, request).await?;
    request.authorize(repo.id, "pull-requests", false).await?;
    if let Some(id) = row.review_id {
        let review = codeyard_db_repos::pull_requests::reviews::get_pull_request_review_by_id(
            &request.state.pool,
            id,
        )
        .await
        .map_err(unavailable)?
        .ok_or_else(missing)?;
        if review.state == "pending"
            && request.private_viewer(repo.id, "pull-requests").await? != Some(review.author_id)
        {
            return Err(missing());
        }
    }
    Ok(
        json!({"__typename":"PullRequestReviewComment","_repo":repo.id,"_scope":"pull-requests",
        "id":node_id::scoped("PRRC",repo.id,row.id),"databaseId":row.id,"body":row.body,"path":row.path,"line":row.line,
        "createdAt":row.created_at,"updatedAt":row.updated_at,"author":crate::user::retained_author(row.author_id,request).await?,
        "pullRequest":crate::issue::pull_object(&pull),"outdated":row.commit_sha.as_ref().is_some_and(|s|s!=&pull.head_sha),
        "isMinimized":false,"minimizedReason":null,"viewerDidAuthor":row.author_id==request.auth.user.id,
        "url":url(&codeyard_repos_pulls::repositories::pull_requests::comments::pull_request_comment_link(&crate::repository::owner(&repo,request).await?,&repo.name,&pull,row))}),
    )
}

pub(super) async fn release(
    row: &codeyard_db_core::models::Release,
    request: &RequestContext,
) -> Result<Value> {
    let repo = crate::repository::by_id(row.repo_id, request).await?;
    request.authorize(repo.id, "contents", false).await?;
    if row.is_draft && !crate::release::drafts_visible(&repo, request).await? {
        return Err(missing());
    }
    Ok(
        json!({"__typename":"Release","_repo":repo.id,"_release":row.id,"id":node_id::scoped("RE",repo.id,row.id),"databaseId":row.id,
        "name":row.name,"tagName":row.tag_name,"description":row.body,"immutable":row.is_immutable,"isDraft":row.is_draft,"isPrerelease":row.is_prerelease,"isLatest":row.is_latest,
        "createdAt":row.created_at,"publishedAt":row.published_at,"author":crate::user::retained_author(row.author_id,request).await?,
        "url":url(&format!("/{}/{}/releases/tag/{}",crate::repository::owner(&repo,request).await?,repo.name,codeyard_webui::encode_component(&row.tag_name)))}),
    )
}

pub(super) async fn asset(
    repo: i64,
    row: &codeyard_db_core::models::ReleaseAsset,
    request: &RequestContext,
) -> Result<Value> {
    let repo = crate::repository::by_id(repo, request).await?;
    let release =
        codeyard_db_repos::releases::get_release(&request.state.pool, repo.id, row.release_id)
            .await
            .map_err(unavailable)?
            .ok_or_else(missing)?;
    crate::node_objects::release(&release.release, request).await?;
    let doc = codeyard_webhooks::payloads::release_asset(
        row,
        &release.release,
        &crate::repository::owner(&repo, request).await?,
        &repo.name,
        None,
        "",
    );
    Ok(
        json!({"__typename":"ReleaseAsset","_repo":repo.id,"id":node_id::scoped("RA",repo.id,row.id),"name":row.name,"contentType":row.content_type,
        "size":row.size,"downloadCount":row.download_count,"createdAt":row.created_at,"updatedAt":row.updated_at,"downloadUrl":doc["browser_download_url"]}),
    )
}

pub(super) async fn database(kind: &str, id: i64, request: &RequestContext) -> Result<Value> {
    let pool = &request.state.pool;
    let repo_id = codeyard_db_repos::graphql::node_owner(pool, kind, id)
        .await
        .map_err(unavailable)?
        .ok_or_else(missing)?;
    let repo = crate::repository::by_id(repo_id, request).await?;
    let scope = if kind == "M" {
        crate::repository::shared_read_scope(repo.id, &["issues", "pull-requests"], request).await?
    } else {
        "contents"
    };
    request
        .authorize(
            repo.id,
            match kind {
                "M" => scope,
                "IE" | "REA" => "metadata",
                "PRR" | "PRRC" | "S" => "pull-requests",
                "CS" => "statuses",
                "ART" => "actions",
                _ => "contents",
            },
            false,
        )
        .await?;
    match kind {
        "CC" => {
            crate::commit_comment::object(
                &codeyard_db_repos::repositories::commit_comments::get_comment(pool, repo.id, id)
                    .await
                    .map_err(unavailable)?
                    .ok_or_else(missing)?,
                request,
            )
            .await
        }
        "M" => {
            let mut value = crate::collaboration::milestone(
                codeyard_db_repos::milestones::get_milestone_by_id(pool, repo.id, id)
                    .await
                    .map_err(unavailable)?
                    .ok_or_else(missing)?,
                request,
            )
            .await?;
            value["_scope"] = json!(scope);
            Ok(value)
        }
        "PRR" => {
            review(
                &codeyard_db_repos::pull_requests::reviews::get_pull_request_review_by_id(pool, id)
                    .await
                    .map_err(unavailable)?
                    .ok_or_else(missing)?,
                request,
            )
            .await
        }
        "PRRC" => {
            review_comment(
                &codeyard_db_repos::pull_requests::reviews::get_visible_pull_request_comment(
                    pool,
                    id,
                    request
                        .private_viewer(repo.id, "pull-requests")
                        .await?
                        .unwrap_or(-1),
                )
                .await
                .map_err(unavailable)?
                .ok_or_else(missing)?,
                request,
            )
            .await
        }
        "RE" => {
            release(
                &codeyard_db_repos::releases::get_release(pool, repo.id, id)
                    .await
                    .map_err(unavailable)?
                    .ok_or_else(missing)?
                    .release,
                request,
            )
            .await
        }
        "RA" => {
            asset(
                repo.id,
                &codeyard_db_repos::releases::get_repository_release_asset(pool, repo.id, id)
                    .await
                    .map_err(unavailable)?
                    .ok_or_else(missing)?,
                request,
            )
            .await
        }
        "CS" => Ok(crate::git::status(
            repo.id,
            &codeyard_db_repos::graphql::status(pool, id)
                .await
                .map_err(unavailable)?
                .ok_or_else(missing)?,
        )),
        "REA" => {
            let row = codeyard_db_repos::graphql::reaction(pool, id)
                .await
                .map_err(unavailable)?
                .ok_or_else(missing)?;
            let (kind, subject_id) = match row.subject_kind.as_str() {
                "issue" => ("I", row.subject_id),
                "pull_request" => ("PR", row.subject_id),
                "issue_comment" => (
                    "IC",
                    codeyard_model::webhook_ids::conversation_comment_id(row.subject_id, false),
                ),
                "pr_comment" => {
                    let comment =
                        codeyard_db_repos::pull_requests::reviews::get_pull_request_comment_by_id(
                            pool,
                            row.subject_id,
                        )
                        .await
                        .map_err(unavailable)?
                        .ok_or_else(missing)?;
                    if comment.path.is_none() && comment.review_id.is_none() {
                        (
                            "IC",
                            codeyard_model::webhook_ids::conversation_comment_id(
                                row.subject_id,
                                true,
                            ),
                        )
                    } else {
                        ("PRRC", row.subject_id)
                    }
                }
                "pr_review" => ("PRR", row.subject_id),
                "release" => ("RE", row.subject_id),
                "commit_comment" => ("CC", row.subject_id),
                _ => return Err(missing()),
            };
            let subject =
                Box::pin(crate::node(&node_id::encode(kind, subject_id), request)).await?;
            Ok(
                json!({"__typename":"Reaction","_repo":repo.id,"_scope":if matches!(kind,"PR"|"PRR"|"PRRC") || subject["_pull"] == true {"pull-requests"} else if matches!(kind,"RE"|"CC") {"contents"} else {"issues"},"id":node_id::scoped("REA",repo.id,row.id),"databaseId":row.id,
                "content":match row.content.as_str(){"+1"=>"THUMBS_UP","-1"=>"THUMBS_DOWN","laugh"=>"LAUGH","hooray"=>"HOORAY","confused"=>"CONFUSED","heart"=>"HEART","rocket"=>"ROCKET","eyes"=>"EYES",_=>return Err(unavailable(()))},"createdAt":row.created_at,"user":crate::user::by_id(row.user_id,request).await?,"reactable":subject}),
            )
        }
        "IE" => {
            let row = codeyard_db_repos::timeline::get_event(pool, repo.id, id)
                .await
                .map_err(unavailable)?
                .ok_or_else(missing)?;
            request
                .authorize(
                    repo.id,
                    if row.subject_kind == "pull_request" {
                        "pull-requests"
                    } else {
                        "issues"
                    },
                    false,
                )
                .await?;
            let _parent = Box::pin(crate::node(
                &node_id::encode(
                    if row.subject_kind == "pull_request" {
                        "PR"
                    } else {
                        "I"
                    },
                    row.subject_id,
                ),
                request,
            ))
            .await?;
            let ty = match row.kind.as_str() {
                "merged" => "MergedEvent",
                "ready_for_review" => "ReadyForReviewEvent",
                "closed" => "ClosedEvent",
                "reopened" => "ReopenedEvent",
                "renamed" => "RenamedTitleEvent",
                "labeled" => "LabeledEvent",
                "unlabeled" => "UnlabeledEvent",
                "assigned" => "AssignedEvent",
                "unassigned" => "UnassignedEvent",
                "milestoned" => "MilestonedEvent",
                "demilestoned" => "DemilestonedEvent",
                "locked" => "LockedEvent",
                "unlocked" => "UnlockedEvent",
                _ => {
                    return Err(async_graphql::Error::new(
                        "This Codeyard event has no matching GitHub GraphQL event type",
                    ))
                }
            };
            Ok(
                json!({"__typename":ty,"_repo":repo.id,"_scope":if row.subject_kind=="pull_request" {"pull-requests"} else {"issues"},"id":node_id::scoped("IE",repo.id,row.id),"createdAt":row.created_at,
                "actor":match row.actor_id {Some(id)=>crate::user::actor_by_id(id,request).await?,None=>Value::Null},"previousTitle":row.previous,"currentTitle":row.detail}),
            )
        }
        "S" | "ART" => Err(async_graphql::Error::new(
            "Codeyard stacks and Actions artifacts have no object type in GitHub's GraphQL schema",
        )),
        _ => Err(missing()),
    }
}
