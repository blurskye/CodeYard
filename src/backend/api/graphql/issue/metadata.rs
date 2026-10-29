use super::*;
use crate::backend::db::{
    discussion_search::{Search, Sort},
    relationships::Kind,
};

pub(in super::super) fn issue_type(item: &db::issue_attributes::Attribute) -> Value {
    json!({"id":node_id::encode("IT",item.id),"name":item.name,"description":item.description,"color":item.color.as_deref().unwrap_or("gray").to_uppercase()})
}

pub(super) async fn resolve(
    ty: &str,
    field: &str,
    ctx: &Arguments,
    request: &RequestContext,
    parent: &Value,
    repo: &db::models::Repository,
    id: i64,
) -> Result<Value> {
    let pool = &request.state.pool;
    match field {
        "issueType" => Ok(db::issue_attributes::assigned_type(pool, repo.id, id)
            .await
            .map_err(unavailable)?
            .as_ref()
            .map(issue_type)
            .unwrap_or(Value::Null)),
        "subIssuesSummary" => {
            let (mut total, mut completed) = (0_i64, 0_i64);
            for counts in db::relationships::summaries(pool, &[id]).await.map_err(unavailable)? {
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
            let mut links = Vec::new();
            loop {
                let rows = db::relationships::list(
                    pool,
                    Some(request.auth.user.id),
                    id,
                    kind,
                    links.len() as i64,
                    100,
                )
                .await
                .map_err(unavailable)?;
                let done = rows.len() < 100;
                links.extend(rows);
                if done {
                    break;
                }
            }
            let mut visible = std::collections::HashMap::new();
            for link in &links {
                request.charge(link.title.len())?;
                if !visible.contains_key(&link.repo_id) {
                    visible.insert(link.repo_id, repository::optional_authority(link.repo_id, "issues", request).await?);
                }
            }
            links.retain(|link| visible[&link.repo_id]);
            if let Some(order) = ctx.args.get("orderBy").filter(|value| !value.is_null()) {
                let order = order.object()?;
                if order.try_get("field")?.enum_name()? == "CREATED_AT" {
                    links.sort_by(|a, b| a.created_at.cmp(&b.created_at).then(a.id.cmp(&b.id)));
                }
                if order.try_get("direction")?.enum_name()? == "DESC" { links.reverse(); }
            }
            let page = if field == "parent" { None } else { Some(Page::new(ctx, links.len() as i64)?) };
            let (offset, limit) = page.as_ref().map_or((0, 1), |page| (page.offset as usize, page.limit as usize));
            let mut nodes = Vec::new();
            for link in links.into_iter().skip(offset).take(limit) {
                let issue = db::issues::get_issue_by_id(pool, link.id)
                    .await
                    .map_err(unavailable)?
                    .ok_or_else(missing)?;
                repository::by_id(issue.repo_id, request).await?;
                request.authorize(issue.repo_id, "issues", false).await?;
                request.charge(issue.title.len().saturating_add(issue.body.len()).saturating_mul(4))?;
                nodes.push(object(&issue));
            }
            match page {
                None => Ok(nodes.into_iter().next().unwrap_or(Value::Null)),
                Some(page) => Ok(page.finish(nodes)),
            }
        }
        _ => super::super::collaboration::resolve(ty, field, ctx, request, parent, repo).await,
    }
}

pub(in super::super) async fn repository(
    ctx: &Arguments,
    request: &RequestContext,
    repo: &db::models::Repository,
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
            .map(issue_type)
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
        for value in labels.deserialize::<Vec<String>>()? {
            add("exact-label", &value)?;
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
                    add("-no", if name == "assignee" { "assignee" } else { "milestone" })?;
                } else if name == "milestone" {
                    let id = value.parse::<i64>().map_err(|_| Error::new("milestone must be a database ID or *"))?;
                    let Some(milestone) = db::milestones::get_milestone_by_id(&request.state.pool, repo_id, id).await.map_err(unavailable)? else {
                        return connection::slice(args, Vec::new());
                    };
                    add(key, &milestone.milestone.number.to_string())?;
                } else { add(key, value)?; }
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
        let values = states.deserialize::<Vec<String>>()?;
        add_states(&mut query, values.iter().map(String::as_str));
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

pub(in super::super) async fn search(args: &Arguments, request: &RequestContext) -> Result<Value> {
    match args.args.try_get("type")?.enum_name()? {
        "ISSUE_HYBRID" | "ISSUE_SEMANTIC" => {
            return Err(Error::new("Codeyard has no semantic search index"))
        }
        "ISSUE" | "ISSUE_ADVANCED" => {}
        _ => return Err(Error::new("Only issue search is supported")),
    }
    let (query, sort, ascending) =
        Search::parse_ordered(&super::super::arg(args, "query")?).map_err(Error::new)?;
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
                    .await.map_err(unavailable)? else { continue };
                request.authorize(row.repo_id, "pull-requests", false).await?;
                request.charge(row.title.len().saturating_add(row.body.len()).saturating_mul(4))?;
                pull_object(&row)
            } else {
                let Some(row) = db::issues::get_issue_by_id(pool, id)
                    .await.map_err(unavailable)? else { continue };
                request.authorize(row.repo_id, "issues", false).await?;
                request.charge(row.title.len().saturating_add(row.body.len()).saturating_mul(4))?;
                object(&row)
            };
            nodes.push(value);
        }
    }
    let mut value = page.finish(nodes);
    value["issueCount"] = json!(total);
    Ok(value)
}
