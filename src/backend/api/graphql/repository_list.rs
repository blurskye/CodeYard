//! Owner-scoped repository connections used by the CLI's list and search paths.
use super::{
    arg, connection, db, missing, repository, unavailable, user, Arguments, RequestContext, Result,
    Value,
};
use crate::backend::repositories::access::{self, AccessError};
use db::repositories::github::{self, RepositoryList, RepositoryListQuery};
use serde_json::json;

pub(super) async fn organizations(
    ctx: &Arguments,
    request: &RequestContext,
    parent: &Value,
) -> Result<Value> {
    request.require_oauth_scope("read:org")?;
    connection::validate(ctx)?;
    let id = parent["databaseId"].as_i64().ok_or_else(missing)?;
    let mut rows = db::organizations::discovery::list_organizations(
        &request.state.pool,
        Some(id),
        id == request.auth.user.id,
        0,
        0,
        i64::MAX,
    )
    .await
    .map_err(unavailable)?;
    request.charge(rows.iter().map(|org| org.name.len() + org.display_name.as_ref().map_or(0, |text| text.len()) + org.description.as_ref().map_or(0, |text| text.len()) + org.billing_email.as_ref().map_or(0, |text| text.len()) + 128).sum())?;
    if let Some(order) = ctx.args.get("orderBy").filter(|value| !value.is_null()) {
        let order = order.object()?;
        match order.try_get("field")?.enum_name()? {
            "LOGIN" => rows.sort_by_cached_key(|org| (org.name.to_lowercase(), org.id)),
            "CREATED_AT" => rows.sort_by_key(|org| (org.created_at, org.id)),
            _ => return Err(async_graphql::Error::new("Unsupported organization order")),
        }
        if order.try_get("direction")?.enum_name()? == "DESC" { rows.reverse(); }
    }
    let page = connection::Page::new(ctx, rows.len() as i64)?;
    let nodes = rows.iter().skip(page.offset as usize).take(page.limit as usize).map(user::organization).collect();
    Ok(page.finish(nodes))
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
        if super::metadata_mutations::list(&values).iter().any(|v| v.as_str() != Some("OWNER")) {
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
    connection::validate(ctx)?;
    if let Some(value) = ctx.args.get("ownerAffiliations").filter(|v| !v.is_null()) {
        if value.deserialize::<Vec<Option<String>>>()?.is_empty() {
            return connection::slice(ctx, vec![]);
        }
    }
    let mut query = query(ctx, request, list)?;
    let total = github::count_repositories(&request.state.pool, query)
        .await
        .map_err(unavailable)?;
    let page = connection::Page::new(ctx, total)?;
    query.limit = page.limit;
    query.offset = page.offset;
    let rows = if page.limit == 0 {
        vec![]
    } else {
        github::list_repositories(&request.state.pool, query)
            .await
            .map_err(unavailable)?
    };
    Ok(page.finish(rows.iter().map(repository::object).collect()))
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
    connection::validate(ctx)?;
    if ctx.args.try_get("type")?.enum_name()? != "REPOSITORY" {
        return Err(async_graphql::Error::new("Unsupported search type"));
    }
    let text = arg(ctx, "query")?;
    // GitHub's sort:field-direction qualifier is ordered by the catalog, not filtered as a term.
    let mut sort = None;
    let mut ascending = false;
    let mut terms = Vec::new();
    for word in db::discussion_search::tokens(&text).map_err(async_graphql::Error::new)? {
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
    let search =
        db::catalog_search::Search::parse(db::catalog_search::Kind::Repositories, &terms.join(" "))
            .map_err(async_graphql::Error::new)?;
    search
        .order(sort.as_deref(), ascending)
        .map_err(async_graphql::Error::new)?;
    let pool = &request.state.pool;
    let viewer = Some(request.auth.user.id);
    let _permit = crate::backend::api::http::middleware::wait_for_static_slot(
        &crate::backend::api::github::search::SEARCHES,
    )
    .await
    .ok_or_else(|| async_graphql::Error::new("Search is busy. Try again shortly."))?;
    let (search, _) = if search.needs_git() {
        crate::backend::repositories::catalog_search::prepare(
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
    let options = |offset, limit| db::catalog_search::Options {
        repository: None,
        sort: sort.clone(),
        ascending,
        offset,
        limit,
    };
    // Only the retrievable window is capped; repositoryCount reports all native matches.
    let total = db::catalog_search::search(pool, viewer, &search, &options(0, 1))
        .await
        .map_err(unavailable)?
        .total;
    let page = connection::Page::new(ctx, total.min(1000))?;
    let hits = if page.limit > 0 {
        db::catalog_search::search(pool, viewer, &search, &options(page.offset, page.limit))
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
        match access::check_read(pool, id, viewer).await {
            Ok(repo) => nodes.push(repository::object(&repo)),
            Err(AccessError::NotFound) => {}
            Err(_) => return Err(unavailable(())),
        }
    }
    let mut value = page.finish(nodes);
    value["repositoryCount"] = json!(total);
    Ok(value)
}
