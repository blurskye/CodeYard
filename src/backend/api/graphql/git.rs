//! Git data uses the same repository store and status rows as the REST adapters.
use super::{
    connection::{self, Page},
    db, missing, node_id, repository, unavailable, url, user, Arguments, RequestContext, Result,
    Value,
};
use crate::backend::repositories::store::{repo_store, CommitInfo};
use serde_json::json;

pub(super) fn commit(repo: i64, row: &CommitInfo, scope: &str) -> Result<Value> {
    let date = |time| chrono::DateTime::from_timestamp(time, 0).ok_or_else(|| unavailable(()));
    let authored = date(row.timestamp)?;
    let committed = date(row.committer_timestamp)?;
    Ok(
        json!({"__typename":"Commit","_repo":repo,"_scope":scope,"_parents":row.parents,
        "id":node_id::git_object("C",repo,&row.sha),"oid":row.sha,"abbreviatedOid":row.sha.chars().take(7).collect::<String>(),
        "message":row.message,"messageHeadline":row.message.lines().next().unwrap_or(""),"messageBody":row.message.split_once('\n').map(|(_,s)|s.trim_start_matches('\n')).unwrap_or(""),
        "authoredDate":authored,"committedDate":committed,
        "author":{"name":row.author_name,"email":row.author_email,"date":authored,"user":null},
        "committer":{"name":row.committer_name,"email":row.committer_email,"date":committed,"user":null}}),
    )
}

pub(super) async fn load(
    repo: &db::models::Repository,
    oid: &str,
    scope: &str,
    request: &RequestContext,
) -> Result<Value> {
    let row = repo_store(repo, request.state.cluster.as_ref())
        .commit(oid)
        .await
        .map_err(unavailable)?;
    commit(repo.id, &row, scope)
}

pub(super) async fn resolve(
    ty: &str,
    field: &str,
    args: &Arguments,
    request: &RequestContext,
    parent: &Value,
) -> Result<Value> {
    if let Some(value) = parent.get(field) {
        return Ok(value.clone());
    }
    let repo = repository::by_id(parent["_repo"].as_i64().ok_or_else(missing)?, request).await?;
    let oid = parent["oid"]
        .as_str()
        .or_else(|| parent["_oid"].as_str())
        .ok_or_else(missing)?;
    match (ty, field) {
        ("Commit", "repository") => Ok(repository::object(&repo)),
        ("Commit", "url") => Ok(json!(url(&format!(
            "/{}/{}/commit/{oid}",
            repository::owner(&repo, request).await?,
            repo.name
        )))),
        ("Commit", "authors") => {
            let value = if parent["author"].is_object() {
                parent.clone()
            } else {
                load(
                    &repo,
                    oid,
                    parent["_scope"].as_str().unwrap_or("contents"),
                    request,
                )
                .await?
            };
            connection::slice(args, vec![value["author"].clone()])
        }
        ("Commit", "parents") => {
            let value = if parent["_parents"].is_array() {
                parent.clone()
            } else {
                load(
                    &repo,
                    oid,
                    parent["_scope"].as_str().unwrap_or("contents"),
                    request,
                )
                .await?
            };
            let parents = value["_parents"].as_array().ok_or_else(missing)?;
            let page = Page::new(args, parents.len() as i64)?;
            let mut nodes = vec![];
            for id in parents
                .iter()
                .skip(page.offset as usize)
                .take(page.limit as usize)
            {
                nodes.push(
                    load(
                        &repo,
                        id.as_str().ok_or_else(missing)?,
                        parent["_scope"].as_str().unwrap_or("contents"),
                        request,
                    )
                    .await?,
                );
            }
            Ok(page.finish(nodes))
        }
        ("Commit", "comments") => super::commit_comment::list(repo.id, oid, args, request).await,
        ("Commit", "statusCheckRollup" | "status") => {
            request.authorize(repo.id, "statuses", false).await?;
            let rows = db::repositories::protection::list_commit_statuses(
                &request.state.pool,
                repo.id,
                oid,
            )
            .await
            .map_err(unavailable)?;
            if rows.is_empty() {
                return Ok(Value::Null);
            }
            let state = if rows
                .iter()
                .any(|s| matches!(s.state.as_str(), "error" | "failure"))
            {
                "FAILURE"
            } else if rows.iter().any(|s| s.state == "pending") {
                "PENDING"
            } else {
                "SUCCESS"
            };
            Ok(
                json!({"__typename":if field=="status" {"Status"} else {"StatusCheckRollup"},"_repo":repo.id,"_scope":"statuses","_oid":oid,"id":node_id::git(if field=="status" {"STAT"} else {"SCR"},&format!("{}/{oid}",repo.id)),"state":state}),
            )
        }
        ("StatusCheckRollup", "contexts") | ("Status", "contexts") => {
            request.authorize(repo.id, "statuses", false).await?;
            let rows = db::repositories::protection::list_commit_statuses(
                &request.state.pool,
                repo.id,
                oid,
            )
            .await
            .map_err(unavailable)?;
            let mut counts = std::collections::BTreeMap::<String, i64>::new();
            for row in &rows {
                *counts.entry(row.state.to_uppercase()).or_default() += 1;
            }
            let nodes = rows
                .iter()
                .map(|row| status(repo.id, row))
                .collect::<Vec<_>>();
            if ty == "Status" {
                return Ok(json!(nodes));
            }
            let total = nodes.len();
            let mut value = connection::slice(args, nodes)?;
            value["checkRunCount"] = json!(0);
            value["checkRunCountsByState"] = json!([]);
            value["statusContextCount"] = json!(total);
            value["statusContextCountsByState"] = json!(counts
                .into_iter()
                .map(|(state, count)| json!({"state":state,"count":count}))
                .collect::<Vec<_>>());
            Ok(value)
        }
        ("StatusContext", "isRequired") => {
            let pull = if let Some(id) = args.args.get("pullRequestId").filter(|id| !id.is_null()) {
                let (kind, id) = node_id::decode(id.string()?).ok_or_else(missing)?;
                if kind != "PR" {
                    return Err(missing());
                }
                db::pull_requests::records::get_pull_request_by_id(&request.state.pool, id)
                    .await
                    .map_err(unavailable)?
                    .ok_or_else(missing)?
            } else if let Some(number) = args
                .args
                .get("pullRequestNumber")
                .filter(|number| !number.is_null())
            {
                // A number names a pull request of the status context's own repository.
                let number = i32::try_from(number.i64()?).map_err(|_| missing())?;
                db::pull_requests::records::get_pull_request(&request.state.pool, repo.id, number)
                    .await
                    .map_err(unavailable)?
                    .ok_or_else(missing)?
            } else {
                return Ok(json!(false));
            };
            repository::by_id(pull.repo_id, request).await?;
            request
                .authorize(pull.repo_id, "pull-requests", false)
                .await?;
            if pull.repo_id != repo.id {
                return Ok(json!(false));
            }
            let rules = crate::backend::repositories::protection::matching_rules(
                &request.state.pool,
                &repo,
                &pull.base_branch,
            )
            .await
            .map_err(unavailable)?;
            Ok(json!(rules.iter().any(|rule| rule.require_status_checks
                && rule
                    .required_status_checks
                    .iter()
                    .any(
                        |name| Some(name.as_str()) == parent["context"].as_str()
                    ))))
        }
        ("StatusContext", "creator") => match parent["_creator"].as_i64() {
            Some(id) => user::actor_by_id(id, request).await,
            None => Ok(Value::Null),
        },
        _ if ty == "Commit" => {
            let full = load(
                &repo,
                oid,
                parent["_scope"].as_str().unwrap_or("contents"),
                request,
            )
            .await?;
            full.get(field).cloned().ok_or_else(|| {
                async_graphql::Error::new(format!("Codeyard does not support {ty}.{field} yet"))
            })
        }
        _ => Err(async_graphql::Error::new(format!(
            "Codeyard does not support {ty}.{field} yet"
        ))),
    }
}

pub(super) fn status(repo: i64, row: &db::models::CommitStatus) -> Value {
    json!({"__typename":"StatusContext","_repo":repo,"_scope":"statuses","_oid":row.sha,"_creator":row.creator_id,
        "id":node_id::encode("CS",row.id),"context":row.context,"state":row.state.to_uppercase(),"description":row.description,"targetUrl":row.target_url,"createdAt":row.created_at})
}
