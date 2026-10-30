//! Translate GraphQL IDs into the REST lane's validated metadata batch, not a second write path.
use super::{db, missing, node_id, repository, unavailable, RequestContext, Result, Value};
use crate::backend::{api::github, db::assignees::AssignedItem, repositories::access};
use serde_json::json;

pub(super) fn list(value: &Value) -> &[Value] {
    match value {
        Value::Array(values) => values,
        Value::Null => &[],
        value => std::slice::from_ref(value),
    }
}

pub(super) fn ids(value: &Value, expected: &str) -> Result<Vec<i64>> {
    list(value)
        .iter()
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
        let mut selected = ids(value, "L")?;
        selected.sort_unstable();
        selected.dedup();
        if selected.len() > db::labels::MAX_ISSUE_LABELS {
            return Err(async_graphql::Error::new("At most 100 labels are allowed"));
        }
        let mut names = vec![];
        for id in selected {
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
        let mut selected = ids(value, "U")?;
        selected.sort_unstable();
        selected.dedup();
        if selected.len() > db::assignees::MAX_ASSIGNEES as usize {
            return Err(async_graphql::Error::new("At most 10 assignees are allowed"));
        }
        let users = db::users::accounts::get_users_by_ids(&request.state.pool, &selected)
            .await.map_err(unavailable)?;
        if users.len() != selected.len() { return Err(missing()); }
        translated["assignees"] = json!(users.into_iter().map(|user| user.username).collect::<Vec<_>>());
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
        &request.auth,
        repo,
        item,
        (&owner, number, title),
        metadata,
    )
    .await
    .map_err(github::Error::graphql)
}
