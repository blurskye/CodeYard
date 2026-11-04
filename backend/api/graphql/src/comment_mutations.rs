use crate::missing;
use crate::mutations::issue_error;
use crate::mutations::pull_error;
use crate::mutations::text;
use crate::RequestContext;
use crate::Result;
use codeyard_app::issues::comments::CommentTarget;
use codeyard_app::issues::comments::CreateCommentRequest;
use codeyard_model::node_id;
use serde_json::json;
use serde_json::Value;

pub(super) async fn resolve(field: &str, input: &Value, request: &RequestContext) -> Result<Value> {
    let encoded = text(input, "id")?;
    let (kind, id) = node_id::decode(encoded).ok_or_else(missing)?;
    let (pull, id) = match kind {
        "IC" => (id % 2 != 0, id / 2),
        "PC" => (true, id),
        _ => return Err(missing()),
    };
    let parent = Box::pin(crate::node(encoded, request)).await?;
    let repo =
        crate::repository::by_id(parent["_repo"].as_i64().ok_or_else(missing)?, request).await?;
    request
        .authorize(repo.id, if pull { "pull-requests" } else { "issues" }, true)
        .await?;
    let target = CommentTarget {
        owner: crate::repository::owner(&repo, request).await?,
        repo: repo.name.clone(),
        number: parent["_number"].as_i64().ok_or_else(missing)? as i32,
        comment_id: id,
    };
    let pool = &request.state.pool;
    let actor = request.auth.user.id;
    let credential = request.auth.credential();
    let mut payload = json!({"clientMutationId":input["clientMutationId"]});
    if field == "deleteIssueComment" {
        if pull {
            codeyard_app::repositories::pull_requests::comments::delete_pull_request_comment(
                pool, actor, target,
            )
            .await
            .map_err(pull_error)?;
        } else {
            codeyard_app::issues::comments::delete_issue_comment(pool, actor, &credential, target)
                .await
                .map_err(issue_error)?;
        }
    } else {
        let body = text(input, "body")?.to_owned();
        payload["issueComment"] = if pull {
            let row =
                codeyard_app::repositories::pull_requests::comments::edit_pull_request_comment(
                    pool,
                    &request.state.notification_broadcaster,
                    actor,
                    target,
                    body,
                    parent["body"].as_str(),
                )
                .await
                .map_err(pull_error)?;
            crate::issue::pull_comment(&row, request).await?
        } else {
            let row = codeyard_app::issues::comments::edit_issue_comment(
                pool,
                &request.state.notification_broadcaster,
                actor,
                &credential,
                target,
                CreateCommentRequest {
                    body,
                    repository_id: Some(repo.id),
                    issue_id: None,
                },
                parent["body"].as_str(),
            )
            .await
            .map_err(issue_error)?;
            crate::issue::comment(&row, request).await?
        };
    }
    Ok(payload)
}
