//! Git data uses the same repository store and status rows as the REST adapters.

use crate::arguments::Arguments;
use crate::connection::Page;
use crate::missing;
use crate::unavailable;
use crate::url;
use crate::RequestContext;
use crate::Result;
use codeyard_app::repositories::store::repo_store;
use codeyard_git::CommitInfo;
use codeyard_model::node_id;
use serde_json::json;
use serde_json::Value;

pub(super) fn charge_commit(row: &CommitInfo, request: &RequestContext) -> Result<()> {
    request.charge(
        row.message
            .len()
            .saturating_mul(4)
            .saturating_add(row.author_name.len())
            .saturating_add(row.author_email.len())
            .saturating_add(row.committer_name.len())
            .saturating_add(row.committer_email.len())
            .saturating_add(row.parents.len().saturating_mul(64)),
    )
}

pub(super) fn commit(
    repo: i64,
    row: &CommitInfo,
    scope: &str,
    request: &RequestContext,
) -> Result<Value> {
    charge_commit(row, request)?;
    let date = |time| chrono::DateTime::from_timestamp(time, 0).ok_or_else(|| unavailable(()));
    let authored = date(row.timestamp)?;
    let committed = date(row.committer_timestamp)?;
    let zone = |minutes: i32| {
        minutes
            .checked_mul(60)
            .and_then(chrono::FixedOffset::east_opt)
            .ok_or_else(|| unavailable(()))
    };
    let author_date = authored.with_timezone(&zone(row.author_offset_minutes)?);
    let committer_date = committed.with_timezone(&zone(row.committer_offset_minutes)?);
    let actor = |name: &str, email: &str, date: chrono::DateTime<chrono::FixedOffset>| json!({"__typename":"GitActor","_repo":repo,"_scope":scope,"_email":email,"name":name,"email":email,"date":date});
    let author = actor(&row.author_name, &row.author_email, author_date);
    let mut authors = vec![author.clone()];
    if let Ok(trailers) = git2::message_trailers_strs(&row.message) {
        for (_, value) in trailers
            .iter()
            .filter(|(key, _)| key.eq_ignore_ascii_case("Co-authored-by"))
        {
            if let Some((name, email)) = value.rsplit_once('<').and_then(|(name, rest)| {
                rest.strip_suffix('>')
                    .map(|email| (name.trim(), email.trim()))
            }) {
                if !name.is_empty()
                    && !email.is_empty()
                    && !authors.iter().any(|actor| {
                        actor["email"]
                            .as_str()
                            .is_some_and(|existing| existing.eq_ignore_ascii_case(email))
                    })
                {
                    authors.push(actor(name, email, author_date));
                }
            }
        }
    }
    Ok(
        json!({"__typename":"Commit","_repo":repo,"_scope":scope,"_parents":row.parents,
        "id":node_id::git_object("C",repo,&row.sha),"oid":row.sha,"abbreviatedOid":codeyard_webui::components::short_sha(&row.sha),
        "message":row.message,"messageHeadline":row.message.lines().next().unwrap_or(""),"messageBody":row.message.split_once('\n').map(|(_,s)|s.trim_start_matches('\n')).unwrap_or(""),
        "authoredDate":authored,"committedDate":committed,
        "author":author,"_authors":authors,
        "committer":actor(&row.committer_name,&row.committer_email,committer_date)}),
    )
}

pub(super) async fn load(
    repo: &codeyard_db_core::models::Repository,
    oid: &str,
    scope: &str,
    request: &RequestContext,
) -> Result<Value> {
    let row = repo_store(repo, request.state.cluster.as_ref())
        .commit(oid)
        .await
        .map_err(unavailable)?;
    commit(repo.id, &row, scope, request)
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
    let repo =
        crate::repository::by_id(parent["_repo"].as_i64().ok_or_else(missing)?, request).await?;
    let oid = parent["oid"]
        .as_str()
        .or_else(|| parent["_oid"].as_str())
        .ok_or_else(missing)?;
    match (ty, field) {
        ("Commit", "repository") => Ok(crate::repository::object(&repo)),
        ("Commit", "url") => Ok(json!(url(&format!(
            "/{}/{}/commit/{oid}",
            crate::repository::owner(&repo, request).await?,
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
            crate::connection::slice(
                args,
                value["_authors"]
                    .as_array()
                    .cloned()
                    .unwrap_or_else(|| vec![value["author"].clone()]),
            )
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
        ("Commit", "comments") => crate::commit_comment::list(repo.id, oid, args, request).await,
        ("Commit", "statusCheckRollup") => crate::checks::rollup(repo.id, oid, request).await,
        ("Commit", "status") => {
            request.authorize(repo.id, "statuses", false).await?;
            let rows = codeyard_db_repos::repositories::protection::list_commit_statuses(
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
                json!({"__typename":"Status","_repo":repo.id,"_scope":"statuses","_oid":oid,"id":node_id::git("STAT",&format!("{}/{oid}",repo.id)),"state":state}),
            )
        }
        ("StatusCheckRollup", "contexts") => crate::checks::contexts(args, request, parent).await,
        ("Status", "contexts") => {
            request.authorize(repo.id, "statuses", false).await?;
            let rows = codeyard_db_repos::repositories::protection::list_commit_statuses(
                &request.state.pool,
                repo.id,
                oid,
            )
            .await
            .map_err(unavailable)?;
            Ok(json!(rows
                .iter()
                .map(|row| status(repo.id, row))
                .collect::<Vec<_>>()))
        }
        ("StatusContext" | "CheckRun", "isRequired") => {
            let pull = if let Some(id) = args.args.get("pullRequestId").filter(|id| !id.is_null()) {
                let (kind, id) = node_id::decode(id.string()?).ok_or_else(missing)?;
                if kind != "PR" {
                    return Err(missing());
                }
                codeyard_db_repos::pull_requests::records::get_pull_request_by_id(
                    &request.state.pool,
                    id,
                )
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
                codeyard_db_repos::pull_requests::records::get_pull_request(
                    &request.state.pool,
                    repo.id,
                    number,
                )
                .await
                .map_err(unavailable)?
                .ok_or_else(missing)?
            } else {
                return Ok(json!(false));
            };
            crate::repository::by_id(pull.repo_id, request).await?;
            request
                .authorize(pull.repo_id, "pull-requests", false)
                .await?;
            if pull.repo_id != repo.id {
                return Ok(json!(false));
            }
            let rules = codeyard_app::repositories::protection::matching_rules(
                &request.state.pool,
                &repo,
                &pull.base_branch,
            )
            .await
            .map_err(unavailable)?;
            Ok(json!(rules.iter().any(|rule| rule.require_status_checks
                && rule.required_status_checks.iter().any(|name| Some(
                    name.as_str()
                )
                    == parent[if ty == "CheckRun" {
                        "name"
                    } else {
                        "context"
                    }]
                    .as_str()))))
        }
        ("StatusContext", "creator") => match parent["_creator"].as_i64() {
            Some(id) => crate::user::actor_by_id(id, request).await,
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

pub(super) fn status(repo: i64, row: &codeyard_db_core::models::CommitStatus) -> Value {
    json!({"__typename":"StatusContext","_repo":repo,"_scope":"statuses","_oid":row.sha,"_creator":row.creator_id,
        "id":node_id::encode("CS",row.id),"context":row.context,"state":row.state.to_uppercase(),"description":row.description,"targetUrl":row.target_url,"createdAt":row.created_at})
}
