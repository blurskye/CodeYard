use crate::missing;
use crate::mutations::text;
use crate::unavailable;
use crate::RequestContext;
use crate::Result;
use codeyard_api_github::node_id;
use codeyard_db_repos::reactions::Subject;
use codeyard_db_repos::reactions::Target;
use serde_json::json;
use serde_json::Value;

pub(super) async fn resolve(field: &str, input: &Value, request: &RequestContext) -> Result<Value> {
    let encoded = text(input, "subjectId")?;
    let subject = Box::pin(crate::node(encoded, request)).await?;
    let repo =
        crate::repository::by_id(subject["_repo"].as_i64().ok_or_else(missing)?, request).await?;
    let (kind, id) = node_id::decode(encoded).ok_or_else(missing)?;
    let pool = &request.state.pool;
    let (subject_kind, subject_id, item) = match kind {
        "I" => (Subject::Issue, id, id),
        "PR" => (Subject::PullRequest, id, id),
        "RE" => (Subject::Release, id, id),
        "CC" => (Subject::CommitComment, id, id),
        "IC" if id % 2 == 0 => {
            let id = id / 2;
            let row = codeyard_db_repos::issues::get_issue_comment(pool, id)
                .await
                .map_err(unavailable)?
                .ok_or_else(missing)?;
            (Subject::IssueComment, id, row.issue_id)
        }
        "IC" | "PC" | "PRRC" => {
            let id = if kind == "IC" { id / 2 } else { id };
            let row =
                codeyard_db_repos::pull_requests::reviews::get_pull_request_comment_by_id(pool, id)
                    .await
                    .map_err(unavailable)?
                    .ok_or_else(missing)?;
            (Subject::PullRequestComment, id, row.pull_request_id)
        }
        "PRR" => {
            let row =
                codeyard_db_repos::pull_requests::reviews::get_pull_request_review_by_id(pool, id)
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
        let (row, _) = codeyard_db_repos::reactions::create_reaction(
            pool,
            actor,
            &request.auth.credential(),
            target,
            content,
        )
        .await
        .map_err(|e| codeyard_api_github::reactions::write_error(e).graphql())?;
        crate::node_objects::database("REA", row.id, request).await?
    } else if let Some(row) =
        codeyard_db_repos::reactions::find_reaction(pool, actor, target, content)
            .await
            .map_err(unavailable)?
    {
        let object = crate::node_objects::database("REA", row.id, request).await?;
        codeyard_db_repos::reactions::delete_reaction(
            pool,
            actor,
            &request.auth.credential(),
            target,
            row.id,
        )
        .await
        .map_err(|e| codeyard_api_github::reactions::write_error(e).graphql())?;
        object
    } else {
        Value::Null
    };
    Ok(
        json!({"clientMutationId":input["clientMutationId"],"reaction":reaction,"subject":subject,"reactionGroups":crate::collaboration::reactions(subject_kind,subject_id,request).await?}),
    )
}
