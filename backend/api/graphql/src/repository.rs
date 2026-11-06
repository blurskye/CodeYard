use crate::arg;
use crate::arguments::Arguments;
use crate::missing;
use crate::unavailable;
use crate::url;
use crate::RequestContext;
use crate::Result;
use async_graphql::Error;
use codeyard_model::node_id;
use serde_json::json;
use serde_json::Value;

pub(super) fn object(repo: &codeyard_db_core::models::Repository) -> Value {
    json!({"__typename":"Repository","id":node_id::encode("R",repo.id),"databaseId":repo.id,
        "name":repo.name,"description":repo.description,"isPrivate":repo.is_private,"isFork":repo.is_fork,
        "isArchived":repo.is_archived,
        "visibility":codeyard_webhooks::payloads::visibility(repo).to_ascii_uppercase(),
        "createdAt":repo.created_at,"updatedAt":repo.updated_at,
        "stargazerCount":repo.stars_count,
        "homepageUrl":repo.website,"isInOrganization":repo.owner_kind=="org",
        "mergeCommitAllowed":repo.allow_merge_commit,"squashMergeAllowed":repo.allow_squash_merge,
        "rebaseMergeAllowed":repo.allow_rebase_merge})
}

pub(super) async fn foreign_object(
    source: &codeyard_db_core::cells::repositories::Source,
    request: &RequestContext,
) -> Result<Value> {
    let repo = &source.repository;
    let owner = source.owner.name();
    let mut value = object(repo);
    let full_name = format!("{owner}/{}", repo.name);
    value["nameWithOwner"] = json!(full_name);
    value["url"] = json!(url(&codeyard_webui::repository_path(owner, &repo.name)));
    let ssh = codeyard_model::http_config::ssh_clone_base();
    value["sshUrl"] = json!(if ssh.trim().is_empty() {
        String::new()
    } else {
        format!("{}/{full_name}.git", ssh.trim_end_matches('/'))
    });
    value["forkCount"] = json!(repo.forks_count);
    value["owner"] = match &source.owner {
        codeyard_db_core::cells::repositories::Owner::User(user) => {
            crate::user::object(user, request.auth.user.id)
        }
        codeyard_db_core::cells::repositories::Owner::Organization(org) => {
            crate::user::organization(org)
        }
    };
    value["defaultBranchRef"] = match &source.default_branch_oid {
        Some(oid) => {
            crate::git_objects::reference_at(
                repo,
                &format!("refs/heads/{}", repo.default_branch),
                oid,
                request,
            )
            .await?
        }
        None => Value::Null,
    };
    Ok(value)
}

pub(super) async fn optional_authority(
    id: i64,
    scope: &str,
    request: &RequestContext,
) -> Result<bool> {
    match request.authorize(id, scope, false).await {
        Ok(()) => Ok(true),
        Err(error)
            if crate::is_not_found(&error) || crate::error_kind(&error) == Some("FORBIDDEN") =>
        {
            Ok(false)
        }
        Err(error) => Err(error),
    }
}

pub(super) async fn shared_read_scope(
    id: i64,
    scopes: &[&'static str],
    request: &RequestContext,
) -> Result<&'static str> {
    for scope in scopes {
        if optional_authority(id, scope, request).await? {
            return Ok(*scope);
        }
    }
    Err(missing())
}

pub(super) async fn by_id(
    id: i64,
    request: &RequestContext,
) -> Result<codeyard_db_core::models::Repository> {
    let repo =
        codeyard_db_repos::repositories::records::get_repository_by_id(&request.state.pool, id)
            .await
            .map_err(unavailable)?
            .ok_or_else(missing)?;
    check(repo, request).await
}

async fn check(
    repo: codeyard_db_core::models::Repository,
    request: &RequestContext,
) -> Result<codeyard_db_core::models::Repository> {
    request.authorize(repo.id, "metadata", false).await?;
    if !codeyard_identity::repository_access::can_read(
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
    repo: &codeyard_db_core::models::Repository,
    request: &RequestContext,
) -> Result<String> {
    codeyard_db_repos::repositories::records::repository_owner_name(&request.state.pool, repo.id)
        .await
        .map_err(unavailable)?
        .ok_or_else(missing)
}

pub(super) async fn query(ctx: &Arguments, request: &RequestContext) -> Result<Value> {
    let (owner, name) = (arg(ctx, "owner")?, arg(ctx, "name")?);
    // gh matches GitHub's wording, so a hidden repository reads exactly like a missing one.
    let unresolved = || {
        codeyard_model::graphql_error::error(
            format!("Could not resolve to a Repository with the name '{owner}/{name}'."),
            "NOT_FOUND",
        )
    };
    let repo = codeyard_db_repos::repositories::records::get_repository(
        &request.state.pool,
        &owner,
        &name,
    )
    .await
    .map_err(unavailable)?
    .ok_or_else(unresolved)?;
    match check(repo, request).await {
        Ok(repo) => Ok(object(&repo)),
        Err(error) if crate::is_not_found(&error) => Err(unresolved()),
        Err(error) => Err(error),
    }
}

pub(super) fn label(label: &codeyard_db_core::models::Label) -> Value {
    json!({"__typename":"Label","_repo":label.repo_id,"id":node_id::scoped("L",label.repo_id,label.id),"name":label.name,"color":label.color.trim_start_matches('#'),"description":label.description,"createdAt":label.created_at,"updatedAt":label.updated_at.unwrap_or(label.created_at),"isDefault":false})
}

pub(super) fn label_connection(
    ctx: &Arguments,
    mut labels: Vec<codeyard_db_core::models::Label>,
    scope: &str,
) -> Result<Value> {
    let order = ctx
        .args
        .get("orderBy")
        .filter(|value| !value.is_null())
        .map(|value| value.object())
        .transpose()?;
    let name = order
        .as_ref()
        .map(|order| order.try_get("field")?.enum_name())
        .transpose()?
        == Some("NAME");
    labels.sort_by(|a, b| {
        if name {
            a.name.cmp(&b.name).then(a.id.cmp(&b.id))
        } else {
            a.created_at.cmp(&b.created_at).then(a.id.cmp(&b.id))
        }
    });
    if order
        .as_ref()
        .map(|order| order.try_get("direction")?.enum_name())
        .transpose()?
        == Some("DESC")
    {
        labels.reverse();
    }
    crate::connection::slice(
        ctx,
        labels
            .iter()
            .map(|row| {
                let mut value = label(row);
                value["_scope"] = json!(scope);
                value
            })
            .collect(),
    )
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
        "forkCount" => Ok(json!(
            codeyard_db_repos::repositories::records::network_fork_count(pool, repo.id)
                .await
                .map_err(unavailable)?
        )),
        "hasWikiEnabled"
        | "hasIssuesEnabled"
        | "hasProjectsEnabled"
        | "forkingAllowed"
        | "autoMergeAllowed"
        | "deleteBranchOnMerge"
        | "allowUpdateBranch"
        | "squashMergeCommitTitle"
        | "squashMergeCommitMessage" => {
            let features =
                codeyard_db_repos::repositories::records::repository_features(pool, repo.id)
                    .await
                    .map_err(unavailable)?;
            Ok(match field {
                "hasWikiEnabled" => json!(features.wiki),
                "hasIssuesEnabled" => json!(features.has_issues),
                "hasProjectsEnabled" => json!(features.has_projects),
                "forkingAllowed" => json!(features.allow_forking),
                "autoMergeAllowed" => json!(features.allow_auto_merge),
                "deleteBranchOnMerge" => json!(features.delete_branch_on_merge),
                "allowUpdateBranch" => json!(features.allow_update_branch),
                "squashMergeCommitTitle" => json!(features.squash_merge_commit_title),
                _ => json!(features.squash_merge_commit_message),
            })
        }
        "isTemplate" => Ok(json!(repo.is_template)),
        "pushedAt" => Ok(json!(
            codeyard_db_repos::repositories::github::repository_pushed_at(pool, id)
                .await
                .map_err(unavailable)?
        )),
        "owner" => {
            if repo.owner_kind == "org" {
                crate::user::organization_by_id(repo.owner_id(), request).await
            } else {
                crate::user::by_id(repo.owner_id(), request).await
            }
        }
        "nameWithOwner" | "url" | "sshUrl" => {
            let name = format!("{}/{}", owner(&repo, request).await?, repo.name);
            let ssh = codeyard_model::http_config::ssh_clone_base();
            Ok(json!(match field {
                "nameWithOwner" => name,
                "url" => url(&format!("/{name}")),
                // sshUrl is non-null in the schema; with SSH off there is no remote to
                // advertise, and a bare "/owner/repo.git" would be a local path.
                _ if ssh.trim().is_empty() => String::new(),
                _ => format!("{}/{name}.git", ssh.trim_end_matches('/')),
            }))
        }
        "discussion" | "discussions" | "discussionCategories" | "hasDiscussionsEnabled" => {
            crate::discussions::repository_field(field, ctx, request, id).await
        }
        "issueTypes" => crate::issue::metadata::repository(ctx, request, &repo).await,
        "parent" => match repo.forked_from_id {
            Some(id) => {
                let Some(parent) =
                    codeyard_db_repos::repositories::records::get_repository_by_id(pool, id)
                        .await
                        .map_err(unavailable)?
                else {
                    return Ok(Value::Null);
                };
                if parent.is_stub {
                    let Some(source) = codeyard_app::cells::foreign_repository(
                        &request.state,
                        &request.actor,
                        &parent,
                    )
                    .await
                    .map_err(unavailable)?
                    else {
                        return Ok(Value::Null);
                    };
                    return foreign_object(&source, request).await;
                }
                if !codeyard_identity::repository_access::can_read(
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
                codeyard_identity::repository_access::user_access(pool, &request.auth.user, &repo)
                    .await
                    .map_err(unavailable)?;
            Ok(if field == "viewerCanAdminister" {
                json!(level.can_admin())
            } else {
                json!(match level {
                    codeyard_db_core::models::AccessLevel::Owner
                    | codeyard_db_core::models::AccessLevel::Admin => "ADMIN",
                    codeyard_db_core::models::AccessLevel::Maintain => "MAINTAIN",
                    codeyard_db_core::models::AccessLevel::Write => "WRITE",
                    codeyard_db_core::models::AccessLevel::Triage => "TRIAGE",
                    _ => "READ",
                })
            })
        }
        "repositoryTopics" => {
            let names = codeyard_db_repos::repositories::topics::repository_topics(pool, &[id])
                .await
                .map_err(unavailable)?
                .remove(&id)
                .unwrap_or_default();
            crate::connection::slice(
                ctx,
                names
                    .iter()
                    .map(|name| json!({"topic":{"name":name}}))
                    .collect(),
            )
        }
        "watchers" => {
            let page = crate::connection::Page::new(ctx, i64::from(repo.watchers_count))?;
            let watchers = codeyard_db_repos::repositories::engagement::list_repo_watchers(
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
                    .map(|watcher| crate::user::object(watcher, request.auth.user.id))
                    .collect(),
            ))
        }
        "forks" => crate::repository_list::forks(ctx, request, repo.id).await,
        "milestone" | "milestones" => crate::milestone::resolve(field, ctx, &repo, request).await,
        "release" | "releases" | "latestRelease" => {
            crate::release::resolve(field, ctx, request, &repo).await
        }
        "ref" | "refs" | "object" => crate::git_objects::resolve(field, ctx, &repo, request).await,
        "defaultBranchRef" => {
            let (refs, default) = codeyard_repos_core::repositories::reads::refs_and_default(
                &repo,
                request.state.cluster.as_ref(),
            )
            .await
            .map_err(unavailable)?;
            let reference = format!("refs/heads/{default}");
            request.charge(
                refs.iter()
                    .map(|(name, oid)| name.len() + oid.len() + 64)
                    .sum(),
            )?;
            match refs.get(&reference) {
                Some(oid) => {
                    crate::git_objects::reference_at(&repo, &reference, oid, request).await
                }
                None => Ok(Value::Null),
            }
        }
        "issue" | "issueOrPullRequest" => {
            let number = ctx.args.try_get("number")?.i64()? as i32;
            if let Some(item) = codeyard_db_repos::issues::get_issue(pool, id, number)
                .await
                .map_err(unavailable)?
            {
                request.authorize(id, "issues", false).await?;
                return Ok(crate::issue::object(&item));
            }
            if field == "issueOrPullRequest" {
                request.authorize(id, "pull-requests", false).await?;
                return Ok(codeyard_db_repos::pull_requests::records::get_pull_request(
                    pool, id, number,
                )
                .await
                .map_err(unavailable)?
                .as_ref()
                .map(crate::issue::pull_object)
                .unwrap_or(Value::Null));
            }
            Ok(Value::Null)
        }
        "pullRequest" => {
            let number = ctx.args.try_get("number")?.i64()? as i32;
            let pull =
                codeyard_db_repos::pull_requests::records::get_pull_request(pool, id, number)
                    .await
                    .map_err(unavailable)?
                    .ok_or_else(|| {
                        codeyard_model::graphql_error::error(
                            format!(
                                "Could not resolve to a PullRequest with the number of {number}."
                            ),
                            "NOT_FOUND",
                        )
                    })?;
            Ok(crate::issue::pull_object(&pull))
        }
        "issues" | "pullRequests" => crate::issue::list(field, ctx, request, id).await,
        "labels" => {
            crate::connection::validate(ctx)?;
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
            let count = codeyard_db_repos::labels::count_repo_labels(pool, id, query)
                .await
                .map_err(unavailable)?;
            let page = crate::connection::Page::new(ctx, count)?;
            let labels = if page.limit == 0 {
                Vec::new()
            } else {
                codeyard_db_repos::labels::list_repo_labels_page(
                    pool,
                    id,
                    query,
                    sort,
                    page.limit,
                    page.offset,
                )
                .await
                .map_err(unavailable)?
            };
            Ok(page.finish(labels.iter().map(label).collect()))
        }
        _ => crate::repository_metadata::resolve(field, ctx, request, &repo).await,
    }
}
