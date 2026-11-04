use crate::backend::api::graphql::db;
use crate::backend::api::graphql::missing;
use crate::backend::api::graphql::mutations::text;
use crate::backend::api::graphql::node_id;

use crate::backend::api::graphql::unavailable;
use crate::backend::api::graphql::RequestContext;
use crate::backend::api::graphql::Result;
use crate::backend::api::graphql::Value;

use crate::backend::db::reactions::Subject;
use crate::backend::db::reactions::Target;
use serde_json::json;
pub(super) async fn resolve(field: &str, input: &Value, request: &RequestContext) -> Result<Value> {
    let encoded = text(input, "subjectId")?;
    let subject = Box::pin(crate::backend::api::graphql::node(encoded, request)).await?;
    let repo = crate::backend::api::graphql::repository::by_id(
        subject["_repo"].as_i64().ok_or_else(missing)?,
        request,
    )
    .await?;
    let (kind, id) = node_id::decode(encoded).ok_or_else(missing)?;
    let pool = &request.state.pool;
    let (subject_kind, subject_id, item) = match kind {
        "I" => (Subject::Issue, id, id),
        "PR" => (Subject::PullRequest, id, id),
        "RE" => (Subject::Release, id, id),
        "CC" => (Subject::CommitComment, id, id),
        "IC" if id % 2 == 0 => {
            let id = id / 2;
            let row = db::issues::get_issue_comment(pool, id)
                .await
                .map_err(unavailable)?
                .ok_or_else(missing)?;
            (Subject::IssueComment, id, row.issue_id)
        }
        "IC" | "PC" | "PRRC" => {
            let id = if kind == "IC" { id / 2 } else { id };
            let row = db::pull_requests::reviews::get_pull_request_comment_by_id(pool, id)
                .await
                .map_err(unavailable)?
                .ok_or_else(missing)?;
            (Subject::PullRequestComment, id, row.pull_request_id)
        }
        "PRR" => {
            let row = db::pull_requests::reviews::get_pull_request_review_by_id(pool, id)
                .await
                .map_err(unavailable)?
                .ok_or_else(missing)?;
            (Subject::PullRequestReview, id, row.pull_request_id)
        }
        _ => return Err(missing()),
    };
    request
        .authorize(
            repo.id,
            match subject_kind {
                Subject::Issue | Subject::IssueComment => "issues",
                Subject::Release | Subject::CommitComment => "contents",
                _ => "pull-requests",
            },
            true,
        )
        .await?;
    let content = match text(input, "content")? {
        "THUMBS_UP" => "+1",
        "THUMBS_DOWN" => "-1",
        "LAUGH" => "laugh",
        "HOORAY" => "hooray",
        "CONFUSED" => "confused",
        "HEART" => "heart",
        "ROCKET" => "rocket",
        "EYES" => "eyes",
        _ => return Err(missing()),
    };
    let target = Target {
        repo_id: repo.id,
        item_id: item,
        subject: subject_kind,
        subject_id,
    };
    let actor = request.auth.user.id;
    let reaction = if field == "addReaction" {
        let (row, _) = db::reactions::create_reaction(
            pool,
            actor,
            &request.auth.credential(),
            target,
            content,
        )
        .await
        .map_err(|e| crate::backend::api::github::reactions::write_error(e).graphql())?;
        crate::backend::api::graphql::node_objects::database("REA", row.id, request).await?
    } else if let Some(row) = db::reactions::find_reaction(pool, actor, target, content)
        .await
        .map_err(unavailable)?
    {
        let object =
            crate::backend::api::graphql::node_objects::database("REA", row.id, request).await?;
        db::reactions::delete_reaction(pool, actor, &request.auth.credential(), target, row.id)
            .await
            .map_err(|e| crate::backend::api::github::reactions::write_error(e).graphql())?;
        object
    } else {
        Value::Null
    };
    Ok(
        json!({"clientMutationId":input["clientMutationId"],"reaction":reaction,"subject":subject,"reactionGroups":crate::backend::api::graphql::collaboration::reactions(subject_kind,subject_id,request).await?}),
    )
}
