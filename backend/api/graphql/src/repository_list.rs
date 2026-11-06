//! Owner-scoped repository connections used by the CLI's list and search paths.
use crate::arg;
use crate::arguments::Arguments;
use crate::missing;
use crate::unavailable;
use crate::RequestContext;
use crate::Result;
use codeyard_db_repos::repositories::github;
use codeyard_db_repos::repositories::github::RepositoryList;
use codeyard_db_repos::repositories::github::RepositoryListQuery;
use codeyard_identity::repository_access::AccessError;
use serde_json::json;
use serde_json::Value;

pub(super) async fn organizations(
    ctx: &Arguments,
    request: &RequestContext,
    parent: &Value,
) -> Result<Value> {
    request.require_oauth_scope("read:org")?;
    crate::connection::validate(ctx)?;
    let id = parent["databaseId"].as_i64().ok_or_else(missing)?;
    let config = codeyard_db_core::cells::pointers::Config::default();
    let mut budget =
        codeyard_db_core::cells::pointers::Budget::new(config).map_err(unavailable)?;
    let mut nodes = Vec::new();
    let mut after = 0;
    loop {
        let rows = codeyard_db_orgs::organizations::discovery::list_organizations(
            &request.state.pool,
            Some(id),
            id == request.auth.user.id,
            after,
            0,
            config.page_size as i64,
        )
        .await
        .map_err(unavailable)?;
        let done = rows.len() < config.page_size;
        for org in rows {
            after = org.id;
            let node = crate::user::organization(&org);
            budget.add(&node).map_err(unavailable)?;
            request.charge(serde_json::to_vec(&node).map_err(unavailable)?.len())?;
            nodes.push(node);
        }
        if done {
            break;
        }
    }
    if id == request.auth.user.id {
        if let Some(remote) = codeyard_app::api_shared::github::pointers::remote(
            &request.state.pool,
            &request.auth,
            &request.headers,
            codeyard_db_core::cells::pointers::List::GraphqlOrganizations,
            Vec::new(),
            config.maximum_scan,
        )
        .await
        .map_err(unavailable)?
        {
            for mut node in remote {
                let (_, id) = node["id"]
                    .as_str()
                    .and_then(codeyard_model::node_id::decode)
                    .ok_or_else(missing)?;
                node["databaseId"] = json!(id);
                node["_membership"] = json!(true);
                budget.add(&node).map_err(unavailable)?;
                request.charge(serde_json::to_vec(&node).map_err(unavailable)?.len())?;
                nodes.push(node);
            }
            codeyard_app::api_shared::github::pointers::recheck(
                &request.state.pool,
                &request.auth,
            )
            .await
            .map_err(unavailable)?;
        }
    }
    nodes.sort_by_key(|node| node["databaseId"].as_i64());
    nodes.dedup_by_key(|node| node["databaseId"].as_i64());
    if let Some(order) = ctx.args.get("orderBy").filter(|value| !value.is_null()) {
        let order = order.object()?;
        match order.try_get("field")?.enum_name()? {
            "LOGIN" => nodes.sort_by_cached_key(|node| {
                (
                    node["login"].as_str().unwrap_or("").to_lowercase(),
                    node["databaseId"].as_i64(),
                )
            }),
            "CREATED_AT" => nodes.sort_by_cached_key(|node| {
                (
                    node["createdAt"]
                        .as_str()
                        .and_then(|date| chrono::DateTime::parse_from_rfc3339(date).ok()),
                    node["databaseId"].as_i64(),
                )
            }),
            _ => return Err(async_graphql::Error::new("Unsupported organization order")),
        }
        if order.try_get("direction")?.enum_name()? == "DESC" {
            nodes.reverse();
        }
    }
    crate::connection::slice(ctx, nodes)
}

fn query<'a>(
    ctx: &'a Arguments,
    request: &RequestContext,
    list: RepositoryList,
) -> Result<RepositoryListQuery<'a>> {
    for name in [
        "affiliations",
        "hasIssuesEnabled",
        "isArchived",
        "isLocked",
        "visibility",
    ] {
        if ctx.args.get(name).is_some_and(|v| !v.is_null()) {
            return Err(async_graphql::Error::new(format!(
                "Codeyard does not support repository filter {name} yet"
            )));
        }
    }
    if let Some(value) = ctx.args.get("ownerAffiliations").filter(|v| !v.is_null()) {
        let values: Value = value.deserialize()?;
        let organization = matches!(list, RepositoryList::Organization(_));
        if crate::metadata_mutations::list(&values).iter().any(|v| {
            v.as_str() != Some("OWNER") && !(organization && v.as_str() == Some("COLLABORATOR"))
        }) {
            return Err(async_graphql::Error::new(
                "Codeyard only supports OWNER ownerAffiliations",
            ));
        }
    }
    let privacy = ctx
        .args
        .get("privacy")
        .filter(|v| !v.is_null())
        .map(|v| v.enum_name())
        .transpose()?;
    let fork = ctx
        .args
        .get("isFork")
        .filter(|v| !v.is_null())
        .map(|v| v.deserialize::<bool>())
        .transpose()?;
    let (sort, descending) = if let Some(order) = ctx.args.get("orderBy").filter(|v| !v.is_null()) {
        let order = order.object()?;
        let sort = match order.try_get("field")?.enum_name()? {
            "CREATED_AT" => "created",
            "UPDATED_AT" => "updated",
            "PUSHED_AT" => "pushed",
            "NAME" => "name",
            "STARGAZERS" => "stars",
            _ => return Err(async_graphql::Error::new("Unsupported repository order")),
        };
        (sort, order.try_get("direction")?.enum_name()? == "DESC")
    } else {
        ("created", true)
    };
    Ok(RepositoryListQuery {
        viewer: Some(request.auth.user.id),
        list,
        since: 0,
        visibility: match privacy {
            Some("PUBLIC") => "public",
            Some("PRIVATE") => "private",
            _ => "all",
        },
        kind: match fork {
            Some(true) => "forks",
            Some(false) => "sources",
            None => "all",
        },
        sort,
        descending,
        limit: i64::MAX,
        offset: 0,
    })
}

fn owner_list(parent: &Value) -> Result<RepositoryList> {
    let id = parent["databaseId"].as_i64().ok_or_else(missing)?;
    Ok(if parent["__typename"] == "Organization" {
        RepositoryList::Organization(id)
    } else {
        RepositoryList::OwnedUser(id)
    })
}

async fn connection(
    ctx: &Arguments,
    request: &RequestContext,
    list: RepositoryList,
) -> Result<Value> {
    crate::connection::validate(ctx)?;
    if let Some(value) = ctx.args.get("ownerAffiliations").filter(|v| !v.is_null()) {
        let values = value.deserialize::<Vec<Option<String>>>()?;
        // An organization is never a collaborator, so GitHub's default [OWNER, COLLABORATOR]
        // lists what it owns and [COLLABORATOR] alone lists nothing.
        if !values.iter().any(|v| v.as_deref() == Some("OWNER"))
            && (values.is_empty() || matches!(list, RepositoryList::Organization(_)))
        {
            return crate::connection::slice(ctx, vec![]);
        }
    }
    let mut query = query(ctx, request, list)?;
    let total = github::count_repositories(&request.state.pool, query)
        .await
        .map_err(unavailable)?;
    let page = crate::connection::Page::new(ctx, total)?;
    query.limit = page.limit;
    query.offset = page.offset;
    let rows = if page.limit == 0 {
        vec![]
    } else {
        github::list_repositories(&request.state.pool, query)
            .await
            .map_err(unavailable)?
    };
    Ok(page.finish(rows.iter().map(crate::repository::object).collect()))
}

pub(super) async fn forks(ctx: &Arguments, request: &RequestContext, repo: i64) -> Result<Value> {
    connection(ctx, request, RepositoryList::Forks(repo)).await
}

pub(super) async fn list(
    ctx: &Arguments,
    request: &RequestContext,
    parent: &Value,
) -> Result<Value> {
    request.require_oauth_scope("repo")?;
    connection(ctx, request, owner_list(parent)?).await
}

pub(super) async fn search(ctx: &Arguments, request: &RequestContext) -> Result<Value> {
    crate::connection::validate(ctx)?;
    if ctx.args.try_get("type")?.enum_name()? != "REPOSITORY" {
        return Err(async_graphql::Error::new("Unsupported search type"));
    }
    let text = arg(ctx, "query")?;
    // GitHub's sort:field-direction qualifier is ordered by the catalog, not filtered as a term.
    let mut sort = None;
    let mut ascending = false;
    let mut terms = Vec::new();
    for word in
        codeyard_db_repos::discussion_search::tokens(&text).map_err(async_graphql::Error::new)?
    {
        match word.strip_prefix("sort:") {
            Some(value) => {
                let (field, direction) = value
                    .rsplit_once('-')
                    .filter(|(_, direction)| matches!(*direction, "asc" | "desc"))
                    .unwrap_or((value, "desc"));
                ascending = direction == "asc";
                sort = Some(field.to_owned());
            }
            None => terms.push(word),
        }
    }
    let search = codeyard_db_repos::catalog_search::Search::parse(
        codeyard_db_repos::catalog_search::Kind::Repositories,
        &terms.join(" "),
    )
    .map_err(async_graphql::Error::new)?
    .with_classic_policy(Some(request.auth.token.id));
    search
        .order(sort.as_deref(), ascending)
        .map_err(async_graphql::Error::new)?;
    let pool = &request.state.pool;
    let viewer = Some(request.auth.user.id);
    let _permit =
        codeyard_api_http::middleware::wait_for_static_slot(&codeyard_api_github::search::SEARCHES)
            .await
            .ok_or_else(|| async_graphql::Error::new("Search is busy. Try again shortly."))?;
    let (search, _) = if search.needs_git() {
        codeyard_repos_pulls::repositories::catalog_search::prepare(
            pool,
            request.state.cluster.as_ref(),
            viewer,
            search,
        )
        .await
        .map_err(unavailable)?
    } else {
        (search, false)
    };
    let options = |offset, limit| codeyard_db_repos::catalog_search::Options {
        repository: None,
        sort: sort.clone(),
        ascending,
        offset,
        limit,
    };
    // Only the retrievable window is capped; repositoryCount reports all native matches.
    let total = codeyard_db_repos::catalog_search::search(pool, viewer, &search, &options(0, 1))
        .await
        .map_err(unavailable)?
        .total;
    let page = crate::connection::Page::new(ctx, total.min(1000))?;
    let hits = if page.limit > 0 {
        codeyard_db_repos::catalog_search::search(
            pool,
            viewer,
            &search,
            &options(page.offset, page.limit),
        )
        .await
        .map_err(unavailable)?
        .items
    } else {
        Vec::new()
    };
    let mut nodes = Vec::new();
    for hit in hits {
        let Some(id) = hit.repository_id else {
            continue;
        };
        match codeyard_identity::repository_access::check_read(pool, id, viewer).await {
            Ok(repo) => nodes.push(crate::repository::object(&repo)),
            Err(AccessError::NotFound) => {}
            Err(_) => return Err(unavailable(())),
        }
    }
    let mut value = page.finish(nodes);
    value["repositoryCount"] = json!(total);
    Ok(value)
}
