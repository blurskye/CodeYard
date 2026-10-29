//! Translate GraphQL IDs into the REST lane's validated metadata batch, not a second write path.
use super::{db, missing, node_id, repository, unavailable, RequestContext, Result, Value};
use crate::backend::{api::github, db::assignees::AssignedItem, repositories::access};
use serde_json::json;

pub(super) fn ids(value: &Value, expected: &str) -> Result<Vec<i64>> {
    let values = match value {
        Value::Array(v) => v.iter().collect::<Vec<_>>(),
        Value::Null => vec![],
        _ => vec![value],
    };
    values
        .into_iter()
        .map(|v| {
            node_id::decode(v.as_str().ok_or_else(missing)?)
                .filter(|(kind, _)| *kind == expected)
                .map(|(_, id)| id)
                .ok_or_else(missing)
        })
        .collect()
}
pub(super) async fn prepare(
    input: &Value,
    repo: &db::models::Repository,
    request: &RequestContext,
) -> Result<github::issues::Metadata> {
    let mut translated = json!({});
    let changed = ["labelIds", "assigneeIds", "milestoneId"]
        .iter()
        .any(|key| input.get(*key).is_some());
    if changed
        && !access::can_triage(&request.state.pool, &request.auth.user, repo)
            .await
            .map_err(unavailable)?
    {
        return Err(super::token_denied());
    }
    if let Some(value) = input.get("labelIds").filter(|v| !v.is_null()) {
        let mut names = vec![];
        for id in ids(value, "L")? {
            let row = db::labels::get_label_by_id(&request.state.pool, id)
                .await
                .map_err(unavailable)?
                .filter(|row| row.repo_id == repo.id)
                .ok_or_else(missing)?;
            names.push(row.name);
        }
        translated["labels"] = json!(names);
    }
    if let Some(value) = input.get("assigneeIds").filter(|v| !v.is_null()) {
        let mut names = vec![];
        for id in ids(value, "U")? {
            let row = db::users::accounts::get_user_by_id(&request.state.pool, id)
                .await
                .map_err(unavailable)?
                .ok_or_else(missing)?;
            names.push(row.username);
        }
        translated["assignees"] = json!(names);
    }
    if let Some(value) = input.get("milestoneId") {
        translated["milestone"] = if value.is_null() {
            Value::Null
        } else {
            let (kind, id) =
                node_id::decode(value.as_str().ok_or_else(missing)?).ok_or_else(missing)?;
            if kind != "M" {
                return Err(missing());
            }
            json!(
                db::milestones::get_milestone_by_id(&request.state.pool, repo.id, id)
                    .await
                    .map_err(unavailable)?
                    .ok_or_else(missing)?
                    .milestone
                    .number
            )
        };
    }
    github::issues::metadata(&request.state.pool, repo, &request.auth.user, &translated)
        .await
        .map_err(github::Error::graphql)
}
pub(super) async fn apply(
    repo: &db::models::Repository,
    item: AssignedItem,
    number: i32,
    title: &str,
    metadata: github::issues::Metadata,
    request: &RequestContext,
) -> Result<()> {
    let owner = repository::owner(repo, request).await?;
    github::issues::apply_metadata(
        &request.state,
        &request.auth.user,
        repo,
        item,
        (&owner, number, title),
        metadata,
    )
    .await
    .map_err(github::Error::graphql)
}
