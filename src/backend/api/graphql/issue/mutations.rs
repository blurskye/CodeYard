use super::*;
use crate::backend::db::relationships::{Kind, Mutation};

pub(super) fn id(input: &Value, name: &str, expected: &str) -> Result<i64> {
    super::super::mutations::target(input, name, expected)
}

pub(super) async fn resolve(
    field: &str,
    ctx: &Arguments,
    request: &RequestContext,
) -> Result<Value> {
    let input: Value = ctx.args.try_get("input")?.deserialize()?;
    let target = id(&input, "issueId", "I")?;
    let pool = &request.state.pool;
    let actor = request.auth.user.id;
    let issue = db::issues::get_issue_by_id(pool, target)
        .await
        .map_err(unavailable)?
        .ok_or_else(missing)?;
    let repo = repository::by_id(issue.repo_id, request).await?;
    request.authorize(repo.id, "issues", true).await?;
    if matches!(
        field,
        "pinIssue" | "unpinIssue" | "deleteIssue" | "transferIssue" | "createLinkedBranch"
    ) {
        return management::mutation(field, &input, request, &repo, target).await;
    }
    match field {
        "updateIssueIssueType" => {
            let kind = if input["issueTypeId"].is_null() {
                None
            } else {
                Some(id(&input, "issueTypeId", "IT")?)
            };
            db::issue_attributes::set_type(pool, repo.id, db::users::accounts::AccountActor::Token { user_id: actor, token_id: request.auth.token.id }, target, kind)
                .await
                .map_err(|e| Error::new(e.to_string()))?;
        }
        "addSubIssue" | "removeSubIssue" | "addBlockedBy" | "removeBlockedBy" => {
            let child = field.ends_with("SubIssue");
            let other = id(
                &input,
                if child {
                    "subIssueId"
                } else {
                    "blockingIssueId"
                },
                "I",
            )?;
            let kind = if child {
                Kind::Children
            } else {
                Kind::BlockedBy
            };
            let mutation = if field.starts_with("add") {
                Mutation::Add {
                    kind,
                    other,
                    replace: input["replaceParent"].as_bool().unwrap_or(false),
                }
            } else {
                Mutation::Remove { kind, other }
            };
            db::relationships::change(pool, actor, repo.id, target, mutation)
                .await
                .map_err(|e| Error::new(e.to_string()))?;
        }
        _ => {
            return Err(Error::new(format!(
                "Codeyard does not support Mutation.{field} yet"
            )))
        }
    }
    Ok(json!({"clientMutationId":input["clientMutationId"],"issue":object(&issue)}))
}
