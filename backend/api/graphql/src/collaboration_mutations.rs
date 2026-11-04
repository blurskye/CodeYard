//! Metadata writes call the REST lane's native, authorized operations and notification batch.
use crate::missing;
use crate::mutations::target;
use crate::mutations::text;
use crate::unavailable;
use crate::RequestContext;
use crate::Result;
use codeyard_db_repos::assignees::AssignedItem;
use codeyard_model::node_id;
use serde_json::json;
use serde_json::Value;

pub(super) const FIELDS: &[&str] = &[
    "createLabel",
    "updateLabel",
    "deleteLabel",
    "addLabelsToLabelable",
    "removeLabelsFromLabelable",
    "addAssigneesToAssignable",
    "removeAssigneesFromAssignable",
    "lockLockable",
    "unlockLockable",
];

pub(super) async fn resolve(field: &str, input: &Value, request: &RequestContext) -> Result<Value> {
    let allowed: &[&str] = match field {
        "createLabel" => &["repositoryId", "name", "color", "description"],
        "updateLabel" => &["id", "name", "color", "description"],
        "deleteLabel" => &["id"],
        "addLabelsToLabelable" | "removeLabelsFromLabelable" => &["labelableId", "labelIds"],
        "addAssigneesToAssignable" | "removeAssigneesFromAssignable" => {
            &["assignableId", "assigneeIds"]
        }
        "lockLockable" => &["lockableId", "lockReason"],
        "unlockLockable" => &["lockableId"],
        _ => return Err(missing()),
    };
    if let Some(key) = input
        .as_object()
        .into_iter()
        .flat_map(|v| v.keys())
        .find(|key| {
            key.as_str() != "clientMutationId"
                && !allowed.contains(&key.as_str())
                && !input[*key].is_null()
        })
    {
        return Err(async_graphql::Error::new(format!(
            "Codeyard does not support {field}.{key} yet"
        )));
    }
    let mut payload = json!({"clientMutationId":input["clientMutationId"],"actor":crate::user::actor_object(&request.auth.user,request.auth.user.id)});
    let pool = &request.state.pool;
    let actor = request.actor.clone();
    if matches!(field, "createLabel" | "updateLabel" | "deleteLabel") {
        let existing = if field == "createLabel" {
            None
        } else {
            Some(
                codeyard_db_repos::labels::get_label_by_id(pool, target(input, "id", "L")?)
                    .await
                    .map_err(unavailable)?
                    .ok_or_else(missing)?,
            )
        };
        let repo = crate::repository::by_id(
            match &existing {
                Some(row) => row.repo_id,
                None => target(input, "repositoryId", "R")?,
            },
            request,
        )
        .await?;
        request.authorize(repo.id, "issues", true).await?;
        if field == "deleteLabel" {
            codeyard_db_repos::labels::delete_label(
                pool,
                actor,
                repo.id,
                existing.ok_or_else(missing)?.id,
            )
            .await
            .map_err(|e| codeyard_api_github::issues::labels::write_error(e).graphql())?;
            return Ok(payload);
        }
        let name = input["name"]
            .as_str()
            .or_else(|| existing.as_ref().map(|r| r.name.as_str()))
            .ok_or_else(missing)?;
        codeyard_model::issue_input::validate_label(name, input["description"].as_str())
            .map_err(async_graphql::Error::new)?;
        let color = input["color"]
            .as_str()
            .map(codeyard_api_github::issues::labels::color)
            .transpose()
            .map_err(codeyard_model::github_error::Error::graphql)?;
        let row = if let Some(old) = existing {
            codeyard_db_repos::labels::update_label(
                pool,
                actor,
                repo.id,
                old.id,
                input["name"].as_str(),
                color.as_deref(),
                input["description"].as_str().map(Some),
            )
            .await
        } else {
            codeyard_db_repos::labels::create_label(
                pool,
                actor,
                repo.id,
                name,
                color.as_deref().ok_or_else(missing)?,
                input["description"].as_str(),
            )
            .await
        }
        .map_err(|e| codeyard_api_github::issues::labels::write_error(e).graphql())?;
        payload["label"] = crate::repository::label(&row);
        return Ok(payload);
    }
    let key = if field.contains("Labelable") {
        "labelableId"
    } else if field.contains("Assignable") {
        "assignableId"
    } else {
        "lockableId"
    };
    let (kind, id) = node_id::decode(text(input, key)?).ok_or_else(missing)?;
    let (item, value) = match kind {
        "I" => {
            let row = codeyard_db_repos::issues::get_issue_by_id(pool, id)
                .await
                .map_err(unavailable)?
                .ok_or_else(missing)?;
            (AssignedItem::Issue(id), crate::issue::object(&row))
        }
        "PR" => {
            let row = codeyard_db_repos::pull_requests::records::get_pull_request_by_id(pool, id)
                .await
                .map_err(unavailable)?
                .ok_or_else(missing)?;
            (
                AssignedItem::PullRequest(id),
                crate::issue::pull_object(&row),
            )
        }
        _ => return Err(missing()),
    };
    let repo =
        crate::repository::by_id(value["_repo"].as_i64().ok_or_else(missing)?, request).await?;
    request
        .authorize(
            repo.id,
            if kind == "PR" {
                "pull-requests"
            } else {
                "issues"
            },
            true,
        )
        .await?;
    let payload_key = if field.contains("Labelable") {
        "labelable"
    } else if field.contains("Assignable") {
        "assignable"
    } else if field == "lockLockable" {
        "lockedRecord"
    } else {
        "unlockedRecord"
    };
    if field.ends_with("Lockable") {
        let reason = match input["lockReason"].as_str() {
            Some("OFF_TOPIC") => Some("off-topic"),
            Some("TOO_HEATED") => Some("too heated"),
            Some("RESOLVED") => Some("resolved"),
            Some("SPAM") => Some("spam"),
            None => None,
            _ => return Err(missing()),
        };
        codeyard_db_repos::discussions::set_lock(
            pool,
            request.actor.clone(),
            repo.id,
            item,
            field == "lockLockable",
            reason,
        )
        .await
        .map_err(|e| codeyard_api_github::issues::locking::error(e).graphql())?;
    } else {
        let label = field.contains("Labelable");
        let input_key = if label { "labelIds" } else { "assigneeIds" };
        let prefix = if label { "L" } else { "U" };
        let mut changes = crate::metadata_mutations::ids(&input[input_key], prefix)?;
        changes.sort_unstable();
        changes.dedup();
        if changes.len()
            > if label {
                codeyard_db_repos::labels::MAX_ISSUE_LABELS
            } else {
                codeyard_db_repos::assignees::MAX_ASSIGNEES as usize
            }
        {
            return Err(async_graphql::Error::new("Too many metadata IDs"));
        }
        let remove = field.starts_with("remove");
        let authority = request.actor.clone();
        // Preparing checks triage authority, that labels belong to this repository and that new
        // assignees can be assigned. A removed assignee may since have lost access, so removals
        // rely on the write's own triage check instead.
        if label || !remove {
            crate::metadata_mutations::prepare(input, &repo, request).await?;
        }
        // Additions use native atomic batches; removals leave unrelated assignments intact.
        if label && !remove {
            codeyard_api_github::issues::labels::save_labels(
                pool, authority, repo.id, item, &changes, false,
            )
            .await
            .map_err(codeyard_model::github_error::Error::graphql)?;
        } else if label {
            for label_id in changes {
                match item {
                    AssignedItem::Issue(id) => {
                        codeyard_db_repos::labels::remove_issue_label(
                            pool,
                            authority.clone(),
                            repo.id,
                            id,
                            label_id,
                        )
                        .await
                    }
                    AssignedItem::PullRequest(id) => {
                        codeyard_db_repos::labels::remove_pull_request_label(
                            pool,
                            authority.clone(),
                            repo.id,
                            id,
                            label_id,
                        )
                        .await
                    }
                }
                .map_err(|e| codeyard_api_github::issues::labels::write_error(e).graphql())?;
            }
        } else {
            let mut ids = changes;
            ids.sort_unstable();
            ids.dedup();
            let users = codeyard_db_identity::users::accounts::get_users_by_ids(pool, &ids)
                .await
                .map_err(unavailable)?;
            if users.len() != ids.len() {
                return Err(missing());
            }
            let owner = crate::repository::owner(&repo, request).await?;
            let target = codeyard_app::issues::assignments::Target {
                actor: &request.auth.user,
                repository: &repo,
                owner: &owner,
                item,
                number: i32::try_from(value["number"].as_i64().ok_or_else(missing)?)
                    .map_err(|_| missing())?,
                title: value["title"].as_str().ok_or_else(missing)?,
            };
            if remove {
                let names: Vec<_> = users.into_iter().map(|user| user.username).collect();
                codeyard_app::issues::assignments::unassign(pool, &target, authority, &names)
                    .await
                    .map_err(|e| codeyard_api_github::issues::assignment_error(e).graphql())?;
            } else {
                codeyard_app::issues::assignments::assign(
                    pool,
                    &request.state.notification_broadcaster,
                    &target,
                    authority,
                    &users,
                )
                .await
                .map_err(|e| codeyard_api_github::issues::assignment_error(e).graphql())?;
            }
        }
    }
    payload[payload_key] = match item {
        AssignedItem::Issue(id) => crate::issue::object(
            &codeyard_db_repos::issues::get_issue_by_id(pool, id)
                .await
                .map_err(unavailable)?
                .ok_or_else(missing)?,
        ),
        AssignedItem::PullRequest(id) => crate::issue::pull_object(
            &codeyard_db_repos::pull_requests::records::get_pull_request_by_id(pool, id)
                .await
                .map_err(unavailable)?
                .ok_or_else(missing)?,
        ),
    };
    Ok(payload)
}
