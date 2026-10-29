use super::{
    arg, connection, db, issue, missing, node_id, unavailable, url, user, Arguments,
    RequestContext, Result, Value,
};
use async_graphql::Error;
use serde_json::json;

pub(super) fn object(repo: &db::models::Repository) -> Value {
    json!({"__typename":"Repository","id":node_id::encode("R",repo.id),"databaseId":repo.id,
        "name":repo.name,"description":repo.description,"isPrivate":repo.is_private,"isFork":repo.is_fork,
        "isArchived":repo.is_archived,"visibility":if repo.is_private {"PRIVATE"} else {"PUBLIC"},
        "createdAt":repo.created_at,"updatedAt":repo.updated_at,
        "stargazerCount":repo.stars_count,"forkCount":repo.forks_count,
        "homepageUrl":repo.website,"isInOrganization":repo.owner_kind=="org","hasIssuesEnabled":true,
        "hasProjectsEnabled":true,"autoMergeAllowed":true,"deleteBranchOnMerge":false,
        "mergeCommitAllowed":repo.allow_merge_commit,"squashMergeAllowed":repo.allow_squash_merge,
        "rebaseMergeAllowed":repo.allow_rebase_merge})
}

pub(super) async fn optional_authority(id: i64, scope: &str, request: &RequestContext) -> Result<bool> {
    match request.authorize(id, scope, false).await {
        Ok(()) => Ok(true),
        Err(error) if matches!(error.extensions.as_ref().and_then(|e| e.get("type")),
            Some(async_graphql::Value::String(kind)) if kind == "NOT_FOUND" || kind == "FORBIDDEN") => Ok(false),
        Err(error) => Err(error),
    }
}

pub(super) async fn shared_read_scope(id: i64, scopes: &[&'static str], request: &RequestContext) -> Result<&'static str> {
    for scope in scopes {
        if optional_authority(id, scope, request).await? { return Ok(*scope); }
    }
    Err(missing())
}

pub(super) async fn by_id(id: i64, request: &RequestContext) -> Result<db::models::Repository> {
    let repo = db::repositories::records::get_repository_by_id(&request.state.pool, id)
        .await
        .map_err(unavailable)?
        .ok_or_else(missing)?;
    check(repo, request).await
}

async fn check(
    repo: db::models::Repository,
    request: &RequestContext,
) -> Result<db::models::Repository> {
    request.authorize(repo.id, "metadata", false).await?;
    if !crate::backend::repositories::access::can_read(
        &request.state.pool,
        Some(&request.auth.user),
        &repo,
    )
    .await
    .map_err(unavailable)?
    {
        return Err(missing());
    }
    Ok(repo)
}

pub(super) async fn owner(
    repo: &db::models::Repository,
    request: &RequestContext,
) -> Result<String> {
    db::repositories::records::repository_owner_name(&request.state.pool, repo.id)
        .await
        .map_err(unavailable)?
        .ok_or_else(missing)
}

pub(super) async fn query(ctx: &Arguments, request: &RequestContext) -> Result<Value> {
    let repo = db::repositories::records::get_repository(
        &request.state.pool,
        &arg(ctx, "owner")?,
        &arg(ctx, "name")?,
    )
    .await
    .map_err(unavailable)?
    .ok_or_else(missing)?;
    Ok(object(&check(repo, request).await?))
}

pub(super) fn label(label: &db::models::Label) -> Value {
    json!({"__typename":"Label","_repo":label.repo_id,"id":node_id::encode("L",label.id),"name":label.name,"color":label.color.trim_start_matches('#'),"description":label.description,"createdAt":label.created_at,"updatedAt":label.updated_at.unwrap_or(label.created_at),"isDefault":false})
}

pub(super) fn label_connection(ctx: &Arguments, mut labels: Vec<db::models::Label>, scope: &str) -> Result<Value> {
    let order = ctx.args.get("orderBy").filter(|value| !value.is_null()).map(|value| value.object()).transpose()?;
    let name = order.as_ref().map(|order| order.try_get("field")?.enum_name()).transpose()? == Some("NAME");
    labels.sort_by(|a, b| {
        if name { a.name.cmp(&b.name).then(a.id.cmp(&b.id)) }
        else { a.created_at.cmp(&b.created_at).then(a.id.cmp(&b.id)) }
    });
    if order.as_ref().map(|order| order.try_get("direction")?.enum_name()).transpose()? == Some("DESC") {
        labels.reverse();
    }
    connection::slice(ctx, labels.iter().map(|row| {
        let mut value = label(row);
        value["_scope"] = json!(scope);
        value
    }).collect())
}

pub(super) async fn label_url(parent: &Value, request: &RequestContext) -> Result<Value> {
    let repo = by_id(parent["_repo"].as_i64().ok_or_else(missing)?, request).await?;
    let name = parent["name"].as_str().ok_or_else(missing)?;
    let mut link = ::url::Url::parse(&url(&format!(
        "/{}/{}/labels/",
        owner(&repo, request).await?,
        repo.name
    )))
    .map_err(unavailable)?;
    link.path_segments_mut()
        .map_err(unavailable)?
        .pop_if_empty()
        .push(name);
    Ok(json!(link.as_str()))
}

pub(super) async fn resolve(
    field: &str,
    ctx: &Arguments,
    request: &RequestContext,
    parent: &Value,
) -> Result<Value> {
    if let Some(value) = parent.get(field) {
        return Ok(value.clone());
    }
    let id = parent["databaseId"].as_i64().ok_or_else(missing)?;
    let repo = by_id(id, request).await?;
    let pool = &request.state.pool;
    match field {
        "hasWikiEnabled" => Ok(json!(db::repositories::records::repository_wiki(
            pool, repo.id
        )
        .await
        .map_err(unavailable)?)),
        "isTemplate" => Ok(json!(repo.is_template)),
        "pushedAt" => Ok(json!(db::repositories::github::repository_pushed_at(
            pool, id
        )
        .await
        .map_err(unavailable)?)),
        "owner" => {
            if repo.owner_kind == "org" {
                user::organization_by_id(repo.owner_id(), request).await
            } else {
                user::by_id(repo.owner_id(), request).await
            }
        }
        "nameWithOwner" | "url" | "sshUrl" => {
            let name = format!("{}/{}", owner(&repo, request).await?, repo.name);
            Ok(json!(match field {
                "nameWithOwner" => name,
                "url" => url(&format!("/{name}")),
                _ => format!(
                    "{}/{name}.git",
                    super::super::http::config::ssh_clone_base().trim_end_matches('/')
                ),
            }))
        }
        "discussion" | "discussions" | "discussionCategories" | "hasDiscussionsEnabled" => {
            super::discussions::repository_field(field, ctx, request, id).await
        }
        "issueTypes" => issue::metadata::repository(ctx, request, &repo).await,
        "parent" => match repo.forked_from_id {
            Some(id) => {
                let Some(parent) = db::repositories::records::get_repository_by_id(pool, id)
                    .await
                    .map_err(unavailable)?
                else {
                    return Ok(Value::Null);
                };
                if !crate::backend::repositories::access::can_read(
                    pool,
                    Some(&request.auth.user),
                    &parent,
                )
                .await
                .map_err(unavailable)?
                {
                    return Ok(Value::Null);
                }
                if !optional_authority(id, "metadata", request).await? {
                    return Ok(Value::Null);
                }
                Ok(object(&parent))
            }
            None => Ok(Value::Null),
        },
        "viewerPermission" | "viewerCanAdminister" => {
            let level =
                crate::backend::repositories::access::user_access(pool, &request.auth.user, &repo)
                    .await
                    .map_err(unavailable)?;
            Ok(if field == "viewerCanAdminister" {
                json!(level.can_admin())
            } else {
                json!(match level {
                    db::models::AccessLevel::Owner | db::models::AccessLevel::Admin => "ADMIN",
                    db::models::AccessLevel::Maintain => "MAINTAIN",
                    db::models::AccessLevel::Write => "WRITE",
                    db::models::AccessLevel::Triage => "TRIAGE",
                    _ => "READ",
                })
            })
        }
        "repositoryTopics" => {
            let names = db::repositories::topics::repository_topics(pool, &[id])
                .await
                .map_err(unavailable)?
                .remove(&id)
                .unwrap_or_default();
            connection::slice(
                ctx,
                names
                    .iter()
                    .map(|name| json!({"topic":{"name":name}}))
                    .collect(),
            )
        }
        "watchers" => {
            let page = connection::Page::new(ctx, i64::from(repo.watchers_count))?;
            let watchers = db::repositories::engagement::list_repo_watchers(
                &request.state.pool,
                repo.id,
                page.limit,
                page.offset,
            )
            .await
            .map_err(unavailable)?;
            Ok(page.finish(
                watchers
                    .iter()
                    .map(|watcher| user::object(watcher, request.auth.user.id))
                    .collect(),
            ))
        }
        "forks" => super::repository_list::forks(ctx, request, repo.id).await,
        "milestone" | "milestones" => super::milestone::resolve(field, ctx, &repo, request).await,
        "release" | "releases" | "latestRelease" => {
            super::release::resolve(field, ctx, request, &repo).await
        }
        "ref" | "refs" | "object" => super::git_objects::resolve(field, ctx, &repo, request).await,
        "defaultBranchRef" => {
            let (refs, default) = crate::backend::repositories::reads::refs_and_default(
                &repo,
                request.state.cluster.as_ref(),
            )
            .await
            .map_err(unavailable)?;
            let reference = format!("refs/heads/{default}");
            Ok(refs.get(&reference).map(|oid| json!({"__typename":"Ref","_repo":id,"id":node_id::reference(id, &reference),"prefix":"refs/heads/","name":default,"target":{"__typename":"Commit","oid":oid,"_repo":id}})).unwrap_or(Value::Null))
        }
        "issue" | "issueOrPullRequest" => {
            let number = ctx.args.try_get("number")?.i64()? as i32;
            if let Some(item) = db::issues::get_issue(pool, id, number)
                .await
                .map_err(unavailable)?
            {
                request.authorize(id, "issues", false).await?;
                return Ok(issue::object(&item));
            }
            if field == "issueOrPullRequest" {
                request.authorize(id, "pull-requests", false).await?;
                return Ok(
                    db::pull_requests::records::get_pull_request(pool, id, number)
                        .await
                        .map_err(unavailable)?
                        .as_ref()
                        .map(issue::pull_object)
                        .unwrap_or(Value::Null),
                );
            }
            Ok(Value::Null)
        }
        "pullRequest" => {
            let number = ctx.args.try_get("number")?.i64()? as i32;
            Ok(
                db::pull_requests::records::get_pull_request(pool, id, number)
                    .await
                    .map_err(unavailable)?
                    .map(|item| issue::pull_object(&item))
                    .unwrap_or(Value::Null),
            )
        }
        "issues" | "pullRequests" => issue::list(field, ctx, request, id).await,
        "labels" => {
            connection::validate(ctx)?;
            let query = ctx
                .args
                .get("query")
                .filter(|v| !v.is_null())
                .map(|v| v.string())
                .transpose()?
                .unwrap_or("");
            let order = ctx.args.try_get("orderBy")?.object()?;
            let sort = match (
                order.try_get("field")?.enum_name()?,
                order.try_get("direction")?.enum_name()?,
            ) {
                ("CREATED_AT", "ASC") => "oldest",
                ("CREATED_AT", "DESC") => "newest",
                ("NAME", "ASC") => "name",
                ("NAME", "DESC") => "name-desc",
                _ => return Err(Error::new("Codeyard does not support this label order yet")),
            };
            let count = db::labels::count_repo_labels(pool, id, query)
                .await
                .map_err(unavailable)?;
            let page = connection::Page::new(ctx, count)?;
            let labels = if page.limit == 0 {
                Vec::new()
            } else {
                db::labels::list_repo_labels_page(pool, id, query, sort, page.limit, page.offset)
                    .await
                    .map_err(unavailable)?
            };
            Ok(page.finish(labels.iter().map(label).collect()))
        }
        _ => super::repository_metadata::resolve(field, ctx, request, &repo).await,
    }
}
