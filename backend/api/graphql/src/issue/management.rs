use super::*;
use crate::arguments::Arguments;
use crate::issue::object;
use crate::issue::pull_object;
use crate::missing;
use crate::mutations::issue_error as operation;
use crate::unavailable;
use crate::url;
use crate::RequestContext;
use crate::Result;
use codeyard_model::node_id;

pub(super) fn error(error: anyhow::Error) -> Error {
    match error.downcast_ref::<codeyard_db_repos::issues::management::Refusal>() {
        Some(refusal) => Error::new(refusal.to_string()),
        None => unavailable(error),
    }
}

pub(super) async fn linked(
    branch: codeyard_db_repos::issues::management::LinkedBranch,
    request: &RequestContext,
) -> Result<Option<Value>> {
    linked_at(branch, request, &mut std::collections::HashMap::new()).await
}

type LinkedSnapshots = std::collections::HashMap<
    i64,
    Option<(
        codeyard_db_core::models::Repository,
        std::collections::HashMap<String, String>,
    )>,
>;

async fn linked_target(
    branch: &codeyard_db_repos::issues::management::LinkedBranch,
    request: &RequestContext,
    snapshots: &mut LinkedSnapshots,
) -> Result<Option<String>> {
    if !snapshots.contains_key(&branch.repo_id) {
        snapshots.insert(branch.repo_id, None);
        if crate::repository::optional_authority(branch.repo_id, "contents", request).await? {
            if let Some(repo) = codeyard_db_repos::repositories::records::get_repository_by_id(
                &request.state.pool,
                branch.repo_id,
            )
            .await
            .map_err(unavailable)?
            {
                let refs = codeyard_app::repositories::store::repo_store(
                    &repo,
                    request.state.cluster.as_ref(),
                )
                .refs()
                .await
                .map_err(unavailable)?;
                request.charge(
                    refs.iter()
                        .map(|(name, oid)| name.len() + oid.len() + 64)
                        .sum(),
                )?;
                snapshots.insert(repo.id, Some((repo, refs)));
            }
        }
    }
    Ok(snapshots[&branch.repo_id]
        .as_ref()
        .and_then(|(_, refs)| refs.get(&format!("refs/heads/{}", branch.name)))
        .cloned())
}

async fn linked_at(
    branch: codeyard_db_repos::issues::management::LinkedBranch,
    request: &RequestContext,
    snapshots: &mut LinkedSnapshots,
) -> Result<Option<Value>> {
    let Some(oid) = linked_target(&branch, request, snapshots).await? else {
        return Ok(None);
    };
    let (repo, _) = snapshots[&branch.repo_id].as_ref().ok_or_else(missing)?;
    let reference = crate::git_objects::reference_at(
        repo,
        &format!("refs/heads/{}", branch.name),
        &oid,
        request,
    )
    .await?;
    Ok(Some(
        json!({"__typename":"LinkedBranch","_repo":repo.id,"_scope":"contents","id":node_id::encode("LB",branch.id),"ref":reference}),
    ))
}

pub(super) async fn resolve(
    field: &str,
    ctx: &Arguments,
    request: &RequestContext,
    repo: &codeyard_db_core::models::Repository,
    id: i64,
) -> Result<Value> {
    match field {
        "isPinned" => Ok(json!(codeyard_db_repos::issues::management::pinned(
            &request.state.pool,
            repo.id
        )
        .await
        .map_err(unavailable)?
        .iter()
        .any(|issue| issue.id == id))),
        "closedByPullRequestsReferences" => {
            crate::connection::validate(ctx)?;
            let issue = codeyard_db_repos::issues::get_issue_by_id(&request.state.pool, id)
                .await
                .map_err(unavailable)?
                .ok_or_else(missing)?;
            let owner = crate::repository::owner(repo, request).await?;
            let mut pulls = codeyard_app::issues::closing::references(
                &request.state.pool,
                Some(&request.auth.user),
                &issue,
                &owner,
                &repo.name,
                &url(""),
            )
            .await
            .map_err(unavailable)?;
            if !ctx
                .args
                .try_get("includeClosedPrs")?
                .deserialize::<bool>()?
            {
                pulls.retain(|pull| pull.state == "open" || pull.merged);
            }
            if ctx.args.try_get("userLinkedOnly")?.deserialize::<bool>()? {
                pulls.clear();
            }
            if ctx.args.try_get("orderByState")?.deserialize::<bool>()? {
                pulls.sort_by_key(|pull| pull.state.to_owned());
            }
            for pull in &pulls {
                request
                    .authorize(pull.repo_id, "pull-requests", false)
                    .await?;
            }
            crate::connection::slice(ctx, pulls.iter().map(pull_object).collect())
        }
        "linkedBranches" => {
            crate::connection::validate(ctx)?;
            let mut branches = Vec::new();
            let mut snapshots = LinkedSnapshots::new();
            for branch in
                codeyard_db_repos::issues::management::linked_branches(&request.state.pool, id)
                    .await
                    .map_err(unavailable)?
            {
                request.charge(branch.name.len() + 104)?;
                if linked_target(&branch, request, &mut snapshots)
                    .await?
                    .is_some()
                {
                    branches.push(branch);
                }
            }
            let page = crate::connection::Page::new(ctx, branches.len() as i64)?;
            let mut nodes = Vec::new();
            for branch in branches
                .into_iter()
                .skip(page.offset as usize)
                .take(page.limit as usize)
            {
                if let Some(node) = linked_at(branch, request, &mut snapshots).await? {
                    nodes.push(node);
                }
            }
            Ok(page.finish(nodes))
        }
        _ => Err(Error::new(format!(
            "Codeyard does not support Issue.{field} yet"
        ))),
    }
}

pub(super) async fn mutation(
    field: &str,
    input: &Value,
    request: &RequestContext,
    repo: &codeyard_db_core::models::Repository,
    id: i64,
) -> Result<Value> {
    let pool = &request.state.pool;
    let actor = request.auth.database_actor();
    let mut payload = json!({"clientMutationId":input["clientMutationId"]});
    match field {
        "pinIssue" | "unpinIssue" => {
            codeyard_db_repos::issues::management::pin(
                pool,
                actor,
                repo.id,
                id,
                field == "pinIssue",
            )
            .await
            .map_err(error)?;
            payload["issue"] = object(
                &codeyard_db_repos::issues::get_issue_by_id(pool, id)
                    .await
                    .map_err(unavailable)?
                    .ok_or_else(missing)?,
            );
        }
        "deleteIssue" => {
            codeyard_db_repos::issues::management::delete(pool, actor, repo.id, id)
                .await
                .map_err(error)?;
            payload["repository"] = crate::repository::object(repo);
        }
        "transferIssue" => {
            if input["createLabelsIfMissing"] == true {
                return Err(Error::new(
                    "Creating labels during issue transfer is not supported",
                ));
            }
            let destination = crate::issue::mutations::id(input, "repositoryId", "R")?;
            let target = crate::repository::by_id(destination, request).await?;
            request.authorize(target.id, "issues", true).await?;
            payload["issue"] = object(
                &codeyard_db_repos::issues::management::transfer(
                    pool,
                    actor,
                    repo.id,
                    id,
                    destination,
                )
                .await
                .map_err(error)?,
            );
        }
        "createLinkedBranch" => {
            let issue = codeyard_db_repos::issues::get_issue_by_id(pool, id)
                .await
                .map_err(unavailable)?
                .ok_or_else(missing)?;
            let destination = if input["repositoryId"].is_null() {
                repo.id
            } else {
                crate::issue::mutations::id(input, "repositoryId", "R")?
            };
            let target = crate::repository::by_id(destination, request).await?;
            request.authorize(target.id, "contents", true).await?;
            request.require_oauth_scope("workflow")?;
            let name = input["name"]
                .as_str()
                .map(str::to_owned)
                .unwrap_or_else(|| codeyard_app::issues::branches::default_name(&issue));
            let oid = input["oid"].as_str().ok_or_else(missing)?;
            let branch = codeyard_app::issues::branches::create(
                &request.state,
                actor,
                &issue,
                &target,
                &name,
                oid,
            )
            .await
            .map_err(operation)?;
            payload["linkedBranch"] = linked(branch, request).await?.ok_or_else(missing)?;
            payload["issue"] = object(&issue);
        }
        _ => return Err(missing()),
    }
    Ok(payload)
}
