use super::*;
use crate::backend::db::discussion_search::Search;
use crate::backend::db::discussion_search::Sort;
use crate::backend::db::relationships::Kind;
use crate::backend::db::relationships::Order;

pub(in crate::backend::api::graphql::issue::super) fn issue_type(
    item: &db::issue_attributes::Attribute,
    repo: i64,
) -> Value {
    json!({"__typename":"IssueType","_repo":repo,"_scope":"issues","id":node_id::encode("IT",item.id),"name":item.name,"description":item.description,"color":item.color.as_deref().unwrap_or("gray").to_uppercase()})
}

pub(in crate::backend::api::graphql::issue::super) async fn type_node(
    id: i64,
    request: &RequestContext,
) -> Result<Value> {
    let mut after = 0;
    loop {
        let repositories = db::graphql::issue_type_repositories(
            &request.state.pool,
            id,
            after,
            request.auth.user.id,
        )
        .await
        .map_err(unavailable)?;
        let done = repositories.len() < 100;
        for repo in repositories {
            after = repo;
            if !repository::optional_authority(repo, "issues", request).await? {
                continue;
            }
            if let Some(item) = db::issue_attributes::catalog(
                &request.state.pool,
                repo,
                Some(request.auth.user.id),
                "type",
            )
            .await
            .map_err(unavailable)?
            .into_iter()
            .find(|item| item.id == id)
            {
                return Ok(issue_type(&item, repo));
            }
        }
        if done {
            return Err(missing());
        }
    }
}

pub(super) async fn resolve(
    ty: &str,
    field: &str,
    ctx: &Arguments,
    request: &RequestContext,
    parent: &Value,
    repo: &codeyard_db_core::models::Repository,
    id: i64,
) -> Result<Value> {
    let pool = &request.state.pool;
    match field {
        "issueType" => Ok(db::issue_attributes::assigned_type(pool, repo.id, id)
            .await
            .map_err(unavailable)?
            .as_ref()
            .map(|item| issue_type(item, repo.id))
            .unwrap_or(Value::Null)),
        "subIssuesSummary" => {
            let (mut total, mut completed) = (0_i64, 0_i64);
            for counts in db::relationships::summaries(pool, &[id])
                .await
                .map_err(unavailable)?
            {
                if repository::optional_authority(counts.repo_id, "issues", request).await? {
                    total += counts.total;
                    completed += counts.completed;
                }
            }
            Ok(
                json!({"total":total,"completed":completed,"percentCompleted":if total==0 {0} else {completed*100/total}}),
            )
        }
        "parent" | "subIssues" | "blockedBy" | "blocking" => {
            let kind = match field {
                "parent" => Kind::Parent,
                "subIssues" => Kind::Children,
                "blockedBy" => Kind::BlockedBy,
                _ => Kind::Blocking,
            };
            if field != "parent" {
                connection::validate(ctx)?;
            }
            let (mut order, mut descending) = (Order::Position, false);
            if let Some(value) = ctx.args.get("orderBy").filter(|value| !value.is_null()) {
                let value = value.object()?;
                order = match value.try_get("field")?.enum_name()? {
                    "CREATED_AT" => Order::CreatedAt,
                    "DEPENDENCY_ADDED_AT" => Order::DependencyAddedAt,
                    _ => return Err(Error::new("Unsupported issue dependency order")),
                };
                descending = value.try_get("direction")?.enum_name()? == "DESC";
            }
            let viewer = Some(request.auth.user.id);
            let (mut total, mut visible) = (0_i64, Vec::new());
            for (repo_id, count) in db::relationships::repository_counts(pool, viewer, id, kind)
                .await
                .map_err(unavailable)?
            {
                if repository::optional_authority(repo_id, "issues", request).await? {
                    total += count;
                    visible.push(repo_id);
                }
            }
            let page = if field == "parent" {
                None
            } else {
                Some(Page::new(ctx, total)?)
            };
            let (offset, limit) = page
                .as_ref()
                .map_or((0, 1), |page| (page.offset, page.limit));
            let links = if limit == 0 || visible.is_empty() {
                Vec::new()
            } else {
                db::relationships::window(
                    pool,
                    &db::relationships::Window {
                        viewer,
                        root: id,
                        kind,
                        repositories: Some(&visible),
                        order,
                        descending,
                        offset,
                        limit,
                    },
                )
                .await
                .map_err(unavailable)?
            };
            let mut nodes = Vec::new();
            for link in links {
                let issue = db::issues::get_issue_by_id(pool, link.id)
                    .await
                    .map_err(unavailable)?
                    .ok_or_else(missing)?;
                repository::by_id(issue.repo_id, request).await?;
                request.authorize(issue.repo_id, "issues", false).await?;
                request.charge(
                    issue
                        .title
                        .len()
                        .saturating_add(issue.body.len())
                        .saturating_mul(4),
                )?;
                nodes.push(object(&issue));
            }
            match page {
                None => Ok(nodes.into_iter().next().unwrap_or(Value::Null)),
                Some(page) => Ok(page.finish(nodes)),
            }
        }
        _ => {
            crate::backend::api::graphql::collaboration::resolve(
                ty, field, ctx, request, parent, repo,
            )
            .await
        }
    }
}

pub(in crate::backend::api::graphql::issue::super) async fn repository(
    ctx: &Arguments,
    request: &RequestContext,
    repo: &codeyard_db_core::models::Repository,
) -> Result<Value> {
    connection::validate(ctx)?;
    let rows = db::issue_attributes::catalog(
        &request.state.pool,
        repo.id,
        Some(request.auth.user.id),
        "type",
    )
    .await
    .map_err(unavailable)?;
    connection::slice(
        ctx,
        rows.iter()
            .filter(|s| s.is_enabled)
            .map(|item| issue_type(item, repo.id))
            .collect(),
    )
}

pub(super) async fn list(
    field: &str,
    args: &Arguments,
    request: &RequestContext,
    repo_id: i64,
) -> Result<Value> {
    let repo = repository::by_id(repo_id, request).await?;
    let mut query = format!(
        "repo:{} is:{}",
        serde_json::to_string(&format!(
            "{}/{}",
            repository::owner(&repo, request).await?,
            repo.name
        ))
        .map_err(unavailable)?,
        if field == "issues" { "issue" } else { "pr" }
    );
    let quote = |value: &str| serde_json::to_string(value).map_err(unavailable);
    let mut add = |key: &str, value: &str| -> Result<()> {
        query.push_str(&format!(" {key}:{}", quote(value)?));
        Ok(())
    };
    for (arg, key) in [("baseRefName", "exact-base"), ("headRefName", "exact-head")] {
        if let Some(value) = args.args.get(arg).filter(|v| !v.is_null()) {
            // `gh pr status` outside a clone asks for the head branch "", which no pull has.
            if value.string()?.is_empty() {
                return connection::slice(args, Vec::new());
            }
            add(key, value.string()?)?;
        }
    }
    if let Some(labels) = args.args.get("labels").filter(|v| !v.is_null()) {
        let labels: Value = labels.deserialize()?;
        for value in crate::backend::api::graphql::metadata_mutations::list(&labels) {
            add("exact-label", value.as_str().ok_or_else(missing)?)?;
        }
    }
    if let Some(filter) = args.args.get("filterBy").filter(|v| !v.is_null()) {
        let filter: Value = filter.deserialize()?;
        for (name, key) in [
            ("assignee", "assignee"),
            ("createdBy", "author"),
            ("mentioned", "mentions"),
            ("milestone", "milestone-number"),
            ("milestoneNumber", "milestone-number"),
        ] {
            if let Some(value) = filter[name].as_str() {
                if value == "*" && matches!(name, "assignee" | "milestone" | "milestoneNumber") {
                    add(
                        "-no",
                        if name == "assignee" {
                            "assignee"
                        } else {
                            "milestone"
                        },
                    )?;
                } else if name == "milestone" {
                    let id = value
                        .parse::<i64>()
                        .map_err(|_| Error::new("milestone must be a database ID or *"))?;
                    let Some(milestone) =
                        db::milestones::get_milestone_by_id(&request.state.pool, repo_id, id)
                            .await
                            .map_err(unavailable)?
                    else {
                        return connection::slice(args, Vec::new());
                    };
                    add(key, &milestone.milestone.number.to_string())?;
                } else {
                    add(key, value)?;
                }
            }
        }
        if let Some(value) = filter["labels"].as_str() {
            add("exact-label", value)?;
        }
        if let Some(values) = filter["labels"].as_array() {
            for value in values {
                add("exact-label", value.as_str().ok_or_else(missing)?)?;
            }
        }
        if !filter["since"].is_null() {
            let since = filter["since"]
                .as_str()
                .ok_or_else(|| Error::new("since must be a date-time"))?;
            add("updated", &format!(">={since}"))?;
        }
        for name in ["type", "issueFieldValues", "viewerSubscribed"] {
            if !filter[name].is_null() {
                return Err(Error::new(format!(
                    "Codeyard has no backing issue {name} filter"
                )));
            }
        }
        if let Some(state) = filter["states"].as_str() {
            add_states(&mut query, std::iter::once(state));
        }
        if let Some(states) = filter["states"].as_array() {
            add_states(&mut query, states.iter().filter_map(Value::as_str));
        }
    }
    if let Some(states) = args.args.get("states").filter(|v| !v.is_null()) {
        let values: Value = states.deserialize()?;
        let values = crate::backend::api::graphql::metadata_mutations::list(&values)
            .iter()
            .map(|value| value.as_str().ok_or_else(missing))
            .collect::<Result<Vec<_>>>()?;
        add_states(&mut query, values.into_iter());
    }
    let (mut sort, mut ascending) = (Sort::Created, false);
    if let Some(order) = args.args.get("orderBy").filter(|v| !v.is_null()) {
        let order = order.object()?;
        sort = match order.try_get("field")?.enum_name()? {
            "CREATED_AT" => Sort::Created,
            "UPDATED_AT" => Sort::Updated,
            "COMMENTS" => Sort::Comments,
            _ => return Err(Error::new("Unsupported discussion order")),
        };
        ascending = order.try_get("direction")?.enum_name()? == "ASC";
    }
    let query = Search::parse(&query).map_err(Error::new)?;
    results(&query, sort, ascending, args, request, None).await
}

fn add_states<'a>(query: &mut String, states: impl Iterator<Item = &'a str>) {
    let states = states
        .map(|s| match s {
            "MERGED" => "is:merged",
            "CLOSED" => "(is:closed -is:merged)",
            _ => "is:open",
        })
        .collect::<Vec<_>>();
    if !states.is_empty() {
        query.push_str(&format!(" ({})", states.join(" OR ")));
    }
}

pub(in crate::backend::api::graphql::issue::super) async fn search(
    args: &Arguments,
    request: &RequestContext,
) -> Result<Value> {
    match args.args.try_get("type")?.enum_name()? {
        "ISSUE_HYBRID" | "ISSUE_SEMANTIC" => {
            return Err(Error::new("Codeyard has no semantic search index"))
        }
        "ISSUE" | "ISSUE_ADVANCED" => {}
        _ => return Err(Error::new("Only issue search is supported")),
    }
    let (query, sort, ascending) =
        Search::parse_ordered(&crate::backend::api::graphql::arg(args, "query")?)
            .map_err(Error::new)?;
    results(&query, sort, ascending, args, request, Some(1000)).await
}

async fn results(
    query: &Search,
    sort: Sort,
    ascending: bool,
    args: &Arguments,
    request: &RequestContext,
    cap: Option<i64>,
) -> Result<Value> {
    connection::validate(args)?;
    let pool = &request.state.pool;
    let viewer = Some(request.auth.user.id);
    let total = db::discussion_search::search(pool, viewer, query, sort, ascending, 0, 0)
        .await
        .map_err(unavailable)?
        .total;
    let page = Page::new(args, cap.map_or(total, |cap| total.min(cap)))?;
    if cap.is_some_and(|cap| page.offset >= cap) {
        return Err(Error::new(
            "Only the first 1000 search results are available",
        ));
    }
    let mut nodes = Vec::new();
    if page.limit != 0 {
        let rows = db::discussion_search::search(
            pool,
            viewer,
            query,
            sort,
            ascending,
            page.offset,
            page.limit,
        )
        .await
        .map_err(unavailable)?;
        for (id, repo, pull, _) in rows.items {
            repository::by_id(repo, request).await?;
            request
                .authorize(repo, if pull { "pull-requests" } else { "issues" }, false)
                .await?;
            let value = if pull {
                let Some(row) = db::pull_requests::records::get_pull_request_by_id(pool, id)
                    .await
                    .map_err(unavailable)?
                else {
                    continue;
                };
                request
                    .authorize(row.repo_id, "pull-requests", false)
                    .await?;
                request.charge(
                    row.title
                        .len()
                        .saturating_add(row.body.len())
                        .saturating_mul(4),
                )?;
                pull_object(&row)
            } else {
                let Some(row) = db::issues::get_issue_by_id(pool, id)
                    .await
                    .map_err(unavailable)?
                else {
                    continue;
                };
                request.authorize(row.repo_id, "issues", false).await?;
                request.charge(
                    row.title
                        .len()
                        .saturating_add(row.body.len())
                        .saturating_mul(4),
                )?;
                object(&row)
            };
            nodes.push(value);
        }
    }
    let mut value = page.finish(nodes);
    value["issueCount"] = json!(total);
    Ok(value)
}
