//! `gh project` resolvers over the shared owner-scoped project domain.
use super::{
    arg, bindings::boxed, connection, db, issue, missing, node_id, repository, unavailable, url,
    user, Arguments, RequestContext, Result, Value,
};
use crate::backend::{db::projects as store, projects as domain};
use async_graphql::{Error, ErrorExtensions};
use chrono::Datelike;
use serde_json::json;

fn operation(error: anyhow::Error) -> Error {
    match error.downcast_ref::<store::Refusal>() {
        Some(store::Refusal::Missing) => missing(),
        Some(refusal) => Error::new(refusal.to_string()).extend_with(|_, ext| {
            ext.set(
                "type",
                if matches!(refusal, store::Refusal::Denied) {
                    "FORBIDDEN"
                } else {
                    "UNPROCESSABLE"
                },
            );
        }),
        None => unavailable(error),
    }
}
fn id(value: &Value, kinds: &[&str]) -> Result<i64> {
    let (kind, id) = value
        .as_str()
        .and_then(node_id::decode)
        .ok_or_else(missing)?;
    if !kinds.contains(&kind) {
        return Err(missing());
    }
    Ok(id)
}
fn owner(value: &Value) -> Result<store::Owner> {
    let id = id(value, &["U", "O"])?;
    Ok(
        if node_id::decode(value.as_str().unwrap()).unwrap().0 == "O" {
            store::Owner::Organization(id)
        } else {
            store::Owner::User(id)
        },
    )
}
fn project_owner(project: &store::Project) -> store::Owner {
    if project.owner_kind == "org" {
        store::Owner::Organization(project.owner_id)
    } else {
        store::Owner::User(project.owner_id)
    }
}
fn project_object(p: &store::Project) -> Value {
    json!({"__typename":"ProjectV2","id":node_id::encode("PVT",p.id),"databaseId":p.id,"number":p.number,"title":p.title,"shortDescription":p.description,"readme":p.readme,"public":p.public,"closed":p.closed_at.is_some(),"closedAt":p.closed_at,"template":p.template,"createdAt":p.created_at,"updatedAt":p.updated_at,"_project":p.id,"_ownerKind":p.owner_kind,"_owner":p.owner_id,"_creator":p.creator_id})
}
fn field_object(f: &store::Field) -> Result<Value> {
    let config = store::field_configuration(f).map_err(operation)?;
    let (ty, prefix) = match f.data_type.as_str() {
        "single_select" => ("ProjectV2SingleSelectField", "PVTSSF"),
        "iteration" => ("ProjectV2IterationField", "PVTIF"),
        _ => ("ProjectV2Field", "PVTF"),
    };
    let mut value = json!({"__typename":ty,"id":node_id::encode(prefix,f.id),"databaseId":f.id,"name":f.name,"dataType":f.data_type.to_uppercase(),"isIssueField":false,"createdAt":f.created_at,"updatedAt":f.updated_at,"_project":f.project_id,"_options":config["options"]});
    if f.data_type == "iteration" {
        let mut active = Vec::new();
        let mut completed = Vec::new();
        for iteration in config["iterations"].as_array().into_iter().flatten() {
            let mut iteration = iteration.clone();
            iteration["startDate"] = iteration["start_date"].clone();
            let end = iteration["start_date"]
                .as_str()
                .and_then(|s| chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d").ok())
                .and_then(|d| {
                    d.checked_add_signed(chrono::Duration::days(
                        iteration["duration"].as_i64().unwrap_or(0),
                    ))
                });
            if end.is_some_and(|end| end <= chrono::Utc::now().date_naive()) {
                completed.push(iteration);
            } else {
                active.push(iteration);
            }
        }
        let day = config["start_date"]
            .as_str()
            .and_then(|s| chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d").ok())
            .map(|d| d.weekday().number_from_monday())
            .unwrap_or(1);
        value["configuration"] = json!({"duration":config["duration"],"startDay":day,"iterations":active,"completedIterations":completed});
    }
    Ok(value)
}
/// GitHub's built-in fields whose values come from an item's issue or pull request: the
/// name, the data type and the content field that holds the value. They follow the stored
/// fields and cannot be edited or deleted.
const BUILT_IN_FIELDS: [(&str, &str, &str); 6] = [
    ("Assignees", "ASSIGNEES", "assignees"),
    ("Labels", "LABELS", "labels"),
    (
        "Linked pull requests",
        "LINKED_PULL_REQUESTS",
        "closedByPullRequestsReferences",
    ),
    ("Milestone", "MILESTONE", "milestone"),
    ("Repository", "REPOSITORY", "repository"),
    ("Reviewers", "REVIEWERS", "reviewRequests"),
];
fn built_in_field(project: &Value, name: &str, data_type: &str) -> Value {
    let id = project["_project"].as_i64().unwrap_or_default();
    json!({"__typename":"ProjectV2Field","id":node_id::git("PVTBF",&format!("{id}/{data_type}")),"databaseId":null,"name":name,"dataType":data_type,"isIssueField":false,
        "createdAt":project["createdAt"],"updatedAt":project["updatedAt"],"_project":id})
}
/// A project's stored fields followed by the built-in ones.
fn project_fields(project: &Value, fields: &[store::Field]) -> Result<Vec<Value>> {
    let mut values: Vec<_> = fields.iter().map(field_object).collect::<Result<_>>()?;
    values.extend(
        BUILT_IN_FIELDS
            .iter()
            .map(|(name, data_type, _)| built_in_field(project, name, data_type)),
    );
    Ok(values)
}
/// The built-in field values of an issue or pull request item. Empty values are left out,
/// as GitHub leaves them out.
async fn built_in_values(item: &store::Item, request: &RequestContext) -> Result<Vec<Value>> {
    let ty = item.content_type();
    if ty == "DraftIssue" {
        return Ok(Vec::new());
    }
    let project = project_object(
        &store::by_id(
            &request.state.pool,
            Some(request.auth.user.id),
            item.project_id,
        )
        .await
        .map_err(operation)?,
    );
    let mut object = item_object(item, request).await?;
    let content = object["content"].take();
    let all = Arguments::new([
        ("first".to_owned(), async_graphql::Value::from(100)),
        (
            "includeClosedPrs".to_owned(),
            async_graphql::Value::from(false),
        ),
        (
            "userLinkedOnly".to_owned(),
            async_graphql::Value::from(false),
        ),
        ("orderByState".to_owned(), async_graphql::Value::from(false)),
    ]);
    let mut values = Vec::new();
    for (name, data_type, source) in BUILT_IN_FIELDS {
        let applies = match data_type {
            "REVIEWERS" => ty == "PullRequest",
            "LINKED_PULL_REQUESTS" => ty == "Issue",
            _ => true,
        };
        if !applies {
            continue;
        }
        let resolved = boxed(|| issue::resolve(ty, source, &all, request, &content)).await?;
        let (suffix, mut entry) = match data_type {
            "MILESTONE" if resolved.is_null() => continue,
            "MILESTONE" => ("Milestone", json!({ "milestone": resolved })),
            "REPOSITORY" => ("Repository", json!({ "repository": resolved })),
            _ => {
                let mut nodes = resolved["nodes"].as_array().cloned().unwrap_or_default();
                if nodes.is_empty() {
                    continue;
                }
                let suffix = match data_type {
                    "ASSIGNEES" => "User",
                    "LABELS" => "Label",
                    "LINKED_PULL_REQUESTS" => "PullRequest",
                    _ => {
                        for node in &mut nodes {
                            *node = node["requestedReviewer"].take();
                        }
                        "Reviewer"
                    }
                };
                (suffix, json!({ "_nodes": nodes }))
            }
        };
        entry["__typename"] = json!(format!("ProjectV2ItemField{suffix}Value"));
        entry["field"] = built_in_field(&project, name, data_type);
        values.push(entry);
    }
    Ok(values)
}
fn draft_object(item: &store::Item) -> Value {
    json!({"__typename":"DraftIssue","id":node_id::encode("DI",item.id),"title":item.draft_title,"body":item.draft_body.as_deref().unwrap_or(""),"createdAt":item.created_at,"updatedAt":item.updated_at,"_creator":item.creator_id,"_project":item.project_id})
}
async fn item_object(item: &store::Item, request: &RequestContext) -> Result<Value> {
    let content = if let Some(id) = item.issue_id {
        let issue = db::issues::get_issue_by_id(&request.state.pool, id)
            .await
            .map_err(unavailable)?
            .ok_or_else(missing)?;
        repository::by_id(issue.repo_id, request).await?;
        issue::object(&issue)
    } else if let Some(id) = item.pull_request_id {
        let pull = db::pull_requests::records::get_pull_request_by_id(&request.state.pool, id)
            .await
            .map_err(unavailable)?
            .ok_or_else(missing)?;
        repository::by_id(pull.repo_id, request).await?;
        issue::pull_object(&pull)
    } else {
        draft_object(item)
    };
    Ok(
        json!({"__typename":"ProjectV2Item","id":node_id::encode("PVTI",item.id),"databaseId":item.id,"isArchived":item.archived,"createdAt":item.created_at,"updatedAt":item.updated_at,"content":content,"_project":item.project_id,"_creator":item.creator_id,"_item":item.id}),
    )
}

/// Projects belong to accounts. Job tokens share one bot identity, so they never reach them.
fn authorize_account(request: &RequestContext) -> Result<()> {
    if request.bounded() {
        return Err(super::token_denied());
    }
    request.require_oauth_scope("repo")
}

pub(super) async fn node(kind: &str, id: i64, request: &RequestContext) -> Result<Value> {
    authorize_account(request)?;
    let pool = &request.state.pool;
    let viewer = Some(request.auth.user.id);
    match kind {
        "PVT" => Ok(project_object(
            &store::by_id(pool, viewer, id).await.map_err(operation)?,
        )),
        "PVTF" | "PVTSSF" | "PVTIF" => {
            let object = field_object(
                &store::field_by_id(pool, viewer, id)
                    .await
                    .map_err(operation)?,
            )?;
            if node_id::decode(object["id"].as_str().unwrap()).unwrap().0 != kind {
                return Err(missing());
            }
            Ok(object)
        }
        "PVTI" | "DI" => {
            let item = store::item_by_id(pool, viewer, id)
                .await
                .map_err(operation)?;
            if kind == "DI" {
                if item.draft_title.is_none() {
                    return Err(missing());
                }
                Ok(draft_object(&item))
            } else {
                item_object(&item, request).await
            }
        }
        _ => Err(missing()),
    }
}

pub(super) async fn content_items(
    ty: &str,
    id: i64,
    ctx: &Arguments,
    request: &RequestContext,
) -> Result<Value> {
    authorize_account(request)?;
    let pool = &request.state.pool;
    let viewer = Some(request.auth.user.id);
    let content = if ty == "Issue" {
        store::Content::Issue(id)
    } else {
        store::Content::PullRequest(id)
    };
    let include_archived = ctx
        .args
        .get("includeArchived")
        .filter(|v| !v.is_null())
        .map(|v| v.deserialize::<bool>())
        .transpose()?
        .unwrap_or(true);
    connection::validate(ctx)?;
    let (count, _) = store::content_items(pool, viewer, &content, include_archived, 0, 0).await.map_err(operation)?;
    let page = connection::Page::new(ctx, count)?;
    let (_, rows) = store::content_items(pool, viewer, &content, include_archived, page.offset, page.limit).await.map_err(operation)?;
    let mut nodes = Vec::new();
    for item in rows { nodes.push(item_object(&item, request).await?); }
    Ok(page.finish(nodes))
}

async fn project_connection(
    ctx: &Arguments,
    request: &RequestContext,
    mut filter: store::ListFilter,
) -> Result<Value> {
    let query = ctx
        .args
        .get("query")
        .filter(|q| !q.is_null())
        .map(|q| q.string())
        .transpose()?
        .unwrap_or("");
    connection::validate(ctx)?;
    let mut terms = Vec::new();
    for term in query.split_whitespace() {
        match term.to_ascii_lowercase().as_str() {
            "is:open" => filter.open = true,
            "is:closed" => filter.closed = true,
            _ => terms.push(term),
        }
    }
    if !terms.is_empty() { filter.titles.push(terms.join(" ")); }
    if let Some(permission) = ctx.args.get("minPermissionLevel").filter(|p| !p.is_null()) {
        filter.writable = permission.enum_name()? != "READ";
    }
    if let Some(order) = ctx.args.get("orderBy").filter(|o| !o.is_null()) {
        let order: Value = order.deserialize()?;
        filter.order = match order["field"].as_str() {
            Some("TITLE") => store::ListOrder::Title,
            Some("UPDATED_AT") => store::ListOrder::Updated,
            Some("CREATED_AT") => store::ListOrder::Created,
            Some("NUMBER") => store::ListOrder::Number,
            _ => return Err(Error::new("Unsupported project order")),
        };
        filter.descending = order["direction"] == "DESC";
    }
    let pool = &request.state.pool;
    let viewer = Some(request.auth.user.id);
    let (count, _) = store::list(pool, viewer, &filter, 0, 0).await.map_err(operation)?;
    let page = connection::Page::new(ctx, count)?;
    let (_, projects) = store::list(pool, viewer, &filter, page.offset, page.limit).await.map_err(operation)?;
    let nodes = projects.iter().map(|project| {
        let mut value = project_object(project);
        value.as_object_mut().unwrap().remove("readme");
        value
    }).collect();
    Ok(page.finish(nodes))
}

pub(super) async fn resource(ctx: &Arguments, request: &RequestContext) -> Result<Value> {
    authorize_account(request)?;
    let raw: String = ctx.args.try_get("url")?.deserialize()?;
    let parsed = reqwest::Url::parse(&raw).map_err(|_| missing())?;
    let origin =
        reqwest::Url::parse(&super::super::http::config::link_origin()).map_err(unavailable)?;
    if parsed.origin() != origin.origin() {
        return Err(missing());
    }
    let parts: Vec<_> = parsed.path().trim_matches('/').split('/').collect();
    if parts.len() != 4 || !matches!(parts[2], "issues" | "pull" | "pulls") {
        return Err(missing());
    }
    let repo = db::repositories::records::get_repository(&request.state.pool, parts[0], parts[1])
        .await
        .map_err(unavailable)?
        .ok_or_else(missing)?;
    repository::by_id(repo.id, request).await?;
    let number = parts[3].parse().map_err(|_| missing())?;
    if parts[2] == "issues" {
        let entry = db::issues::get_issue(&request.state.pool, repo.id, number)
            .await
            .map_err(unavailable)?
            .ok_or_else(missing)?;
        Ok(issue::object(&entry))
    } else {
        let entry =
            db::pull_requests::records::get_pull_request(&request.state.pool, repo.id, number)
                .await
                .map_err(unavailable)?
                .ok_or_else(missing)?;
        Ok(issue::pull_object(&entry))
    }
}

pub(super) async fn resolve(
    ty: &str,
    field: &str,
    ctx: &Arguments,
    request: &RequestContext,
    parent: &Value,
) -> Result<Value> {
    authorize_account(request)?;
    if let Some(value) = parent.get(field) {
        return Ok(value.clone());
    }
    let pool = &request.state.pool;
    let actor = request.auth.user.id;
    let viewer = Some(actor);
    if field == "projects" && matches!(ty, "Repository" | "Organization") {
        if ty == "Repository" {
            repository::by_id(parent["databaseId"].as_i64().ok_or_else(missing)?, request).await?;
        }
        // Classic projects are retired; gh still asks for them before its v2 lookup.
        return connection::slice(ctx, Vec::new());
    }
    if ty == "Repository" && field == "projectsV2" {
        let repo = parent["databaseId"].as_i64().ok_or_else(missing)?;
        repository::by_id(repo, request).await?;
        return project_connection(ctx, request, store::ListFilter { repo: Some(repo), ..Default::default() }).await;
    }
    if matches!(ty, "User" | "Organization") {
        let target = parent["databaseId"].as_i64().ok_or_else(missing)?;
        let owner = if ty == "User" {
            store::Owner::User(target)
        } else {
            store::Owner::Organization(target)
        };
        return match field {
            "team" if ty == "Organization" => {
                let team = db::organizations::teams::get_public_team(
                    pool,
                    actor,
                    target,
                    &arg(ctx, "slug")?,
                )
                .await
                .map_err(unavailable)?
                .ok_or_else(missing)?;
                Ok(
                    json!({"__typename":"Team","id":node_id::encode("T",team.team.id),"slug":team.team.slug}),
                )
            }
            "projectV2" => Ok(project_object(
                &store::get(pool, viewer, owner, ctx.args.try_get("number")?.i64()?)
                    .await
                    .map_err(operation)?,
            )),
            "projectsV2" => {
                project_connection(ctx, request, store::ListFilter { owner: Some(owner), ..Default::default() }).await
            }
            "viewerCanCreateProjects" => Ok(json!(store::can_write(pool, actor, owner)
                .await
                .map_err(operation)?)),
            "organizations" if target == actor => {
                let mut after = 0;
                let mut nodes = Vec::new();
                loop {
                    let page = db::organizations::OrganizationPage::new(after, 100)
                        .map_err(|_| missing())?;
                    let db::organizations::OrganizationOutcome::Applied(orgs) =
                        db::organizations::profiles::list_actor_organizations(pool, actor, page)
                            .await
                            .map_err(unavailable)?
                    else {
                        return Err(missing());
                    };
                    if orgs.is_empty() {
                        break;
                    }
                    for org in &orgs {
                        nodes.push(user::organization_by_id(org.id, request).await?);
                        after = org.id;
                    }
                    if orgs.len() < 100 {
                        break;
                    }
                }
                connection::slice(ctx, nodes)
            }
            _ => Err(Error::new("Unsupported project owner field")),
        };
    }
    match (ty, field) {
        ("ProjectV2", "readme") => {
            let project = store::by_id(pool, viewer, parent["databaseId"].as_i64().ok_or_else(missing)?).await.map_err(operation)?;
            Ok(json!(project.readme))
        }
        (_, "project") => {
            node(
                "PVT",
                parent["_project"].as_i64().ok_or_else(missing)?,
                request,
            )
            .await
        }
        (_, "creator") => {
            user::by_id(parent["_creator"].as_i64().ok_or_else(missing)?, request).await
        }
        ("ProjectV2", "owner") => {
            if parent["_ownerKind"] == "org" {
                user::organization_by_id(parent["_owner"].as_i64().ok_or_else(missing)?, request)
                    .await
            } else {
                user::by_id(parent["_owner"].as_i64().ok_or_else(missing)?, request).await
            }
        }
        ("ProjectV2", "url" | "resourcePath") => {
            let owner = if parent["_ownerKind"] == "org" {
                user::organization_by_id(parent["_owner"].as_i64().ok_or_else(missing)?, request)
                    .await?
            } else {
                user::by_id(parent["_owner"].as_i64().ok_or_else(missing)?, request).await?
            };
            let path = format!(
                "/{}/{}/projects/{}",
                if parent["_ownerKind"] == "org" {
                    "orgs"
                } else {
                    "users"
                },
                owner["login"].as_str().ok_or_else(missing)?,
                parent["number"]
            );
            Ok(json!(if field == "url" { url(&path) } else { path }))
        }
        ("ProjectV2", "viewerCanUpdate" | "viewerCanClose" | "viewerCanReopen") => {
            let project = store::by_id(
                pool,
                viewer,
                parent["_project"].as_i64().ok_or_else(missing)?,
            )
            .await
            .map_err(operation)?;
            Ok(json!(store::can_write(
                pool,
                actor,
                project_owner(&project)
            )
            .await
            .map_err(operation)?))
        }
        ("ProjectV2", "fields" | "field") if parent.get("_deletedFields").is_some() => {
            connection::slice(
                ctx,
                parent["_deletedFields"]
                    .as_array()
                    .cloned()
                    .unwrap_or_default(),
            )
        }
        ("ProjectV2", "items") if parent.get("_deletedItems").is_some() => connection::slice(
            ctx,
            parent["_deletedItems"]
                .as_array()
                .cloned()
                .unwrap_or_default(),
        ),
        ("ProjectV2", "fields" | "field") => {
            let fields = store::fields(
                pool,
                viewer,
                parent["_project"].as_i64().ok_or_else(missing)?,
            )
            .await
            .map_err(operation)?;
            let values = project_fields(parent, &fields)?;
            if field == "field" {
                let name = arg(ctx, "name")?;
                return Ok(values
                    .into_iter()
                    .find(|f| f["name"] == name)
                    .unwrap_or(Value::Null));
            }
            connection::slice(ctx, values)
        }
        ("ProjectV2", "items") => {
            let project = parent["_project"].as_i64().ok_or_else(missing)?;
            let fields = store::fields(pool, viewer, project)
                .await
                .map_err(operation)?;
            let items = store::items(pool, viewer, project)
                .await
                .map_err(operation)?;
            let states: Option<Vec<String>> = ctx
                .args
                .get("archivedStates")
                .filter(|s| !s.is_null())
                .map(|s| s.deserialize())
                .transpose()?;
            let items = items
                .into_iter()
                .filter(|item| {
                    states.as_ref().map_or(!item.archived, |states| {
                        states.iter().any(|s| {
                            s == if item.archived {
                                "ARCHIVED"
                            } else {
                                "NOT_ARCHIVED"
                            }
                        })
                    })
                })
                .collect();
            let query = ctx
                .args
                .get("query")
                .filter(|q| !q.is_null())
                .map(|q| q.string())
                .transpose()?
                .unwrap_or("");
            let mut items = domain::filter_items(items, &fields, query, None).map_err(operation)?;
            // Items are ordered by position only; descending reverses the stored order.
            if let Some(order) = ctx.args.get("orderBy").filter(|v| !v.is_null()) {
                if order.object()?.try_get("direction")?.enum_name()? == "DESC" {
                    items.reverse();
                }
            }
            let page = connection::Page::new(ctx, items.len() as i64)?;
            // Only the requested page is hydrated, so a count-only request hydrates no items.
            let mut nodes = Vec::new();
            for item in items
                .iter()
                .skip(page.offset as usize)
                .take(page.limit as usize)
            {
                nodes.push(item_object(item, request).await?);
            }
            Ok(page.finish(nodes))
        }
        ("ProjectV2Item", "fieldValues" | "fieldValueByName") => {
            let item =
                store::item_by_id(pool, viewer, parent["_item"].as_i64().ok_or_else(missing)?)
                    .await
                    .map_err(operation)?;
            let fields = store::fields(pool, viewer, item.project_id)
                .await
                .map_err(operation)?;
            let mut values = Vec::new();
            for f in fields {
                let value = item.value(&f).map_err(operation)?;
                if value.is_null() {
                    continue;
                }
                let mut entry = json!({"field":field_object(&f)?});
                let suffix = match f.data_type.as_str() {
                    "title" | "text" => {
                        entry["text"] = value;
                        "Text"
                    }
                    "number" => {
                        entry["number"] = value;
                        "Number"
                    }
                    "date" => {
                        entry["date"] = value;
                        "Date"
                    }
                    "single_select" => {
                        entry["name"] =
                            json!(domain::display_value(&f, &value).map_err(operation)?);
                        entry["optionId"] = value;
                        "SingleSelect"
                    }
                    "iteration" => {
                        let config = store::field_configuration(&f).map_err(operation)?;
                        let iteration = config["iterations"]
                            .as_array()
                            .and_then(|xs| xs.iter().find(|i| i["id"] == value))
                            .ok_or_else(missing)?;
                        entry["title"] = iteration["title"].clone();
                        entry["startDate"] = iteration["start_date"].clone();
                        entry["duration"] = iteration["duration"].clone();
                        entry["iterationId"] = value;
                        "Iteration"
                    }
                    _ => continue,
                };
                entry["__typename"] = json!(format!("ProjectV2ItemField{suffix}Value"));
                values.push(entry);
            }
            values.extend(built_in_values(&item, request).await?);
            if field == "fieldValueByName" {
                let name = arg(ctx, "name")?;
                return Ok(values
                    .into_iter()
                    .find(|v| v["field"]["name"] == name)
                    .unwrap_or(Value::Null));
            }
            connection::slice(ctx, values)
        }
        (
            "ProjectV2ItemFieldLabelValue"
            | "ProjectV2ItemFieldPullRequestValue"
            | "ProjectV2ItemFieldReviewerValue"
            | "ProjectV2ItemFieldUserValue",
            "labels" | "pullRequests" | "reviewers" | "users",
        ) => connection::slice(
            ctx,
            parent["_nodes"].as_array().cloned().unwrap_or_default(),
        ),
        ("ProjectV2SingleSelectField", "options") => {
            let names: Option<Vec<String>> = ctx
                .args
                .get("names")
                .filter(|n| !n.is_null())
                .map(|n| n.deserialize())
                .transpose()?;
            Ok(json!(parent["_options"]
                .as_array()
                .into_iter()
                .flatten()
                .filter(|o| names
                    .as_ref()
                    .is_none_or(|names| names.iter().any(|name| o["name"] == *name)))
                .cloned()
                .collect::<Vec<_>>()))
        }
        _ => Err(Error::new(format!(
            "Codeyard does not support {ty}.{field} yet"
        ))),
    }
}

pub(super) async fn mutation(
    field: &str,
    ctx: &Arguments,
    request: &RequestContext,
) -> Result<Value> {
    authorize_account(request)?;
    request
        .auth
        .require_write()
        .map_err(|_| Error::new("Resource not accessible by token"))?;
    let input: Value = ctx.args.try_get("input")?.deserialize()?;
    let pool = &request.state.pool;
    let actor = request.auth.user.id;
    let viewer = Some(actor);
    let mut payload = json!({"clientMutationId":input["clientMutationId"]});
    if field == "createProjectV2" {
        if !input["teamId"].is_null() || !input["repositoryId"].is_null() {
            return Err(Error::new(
                "Create the project first, then link its repository or team",
            ));
        }
        let project = store::create(
            pool,
            actor,
            &request.auth.credential(),
            owner(&input["ownerId"])?,
            input["title"].as_str().ok_or_else(missing)?,
            None,
            false,
        )
        .await
        .map_err(operation)?;
        payload["projectV2"] = project_object(&project);
        return Ok(payload);
    }
    if field == "updateProjectV2DraftIssue" {
        if input["assigneeIds"]
            .as_array()
            .is_some_and(|xs| !xs.is_empty())
        {
            return Err(Error::new("Draft assignees are not supported"));
        }
        let item = store::edit_draft(
            pool,
            actor,
            &request.auth.credential(),
            id(&input["draftIssueId"], &["DI"])?,
            input["title"].as_str(),
            input["body"].as_str(),
        )
        .await
        .map_err(operation)?;
        payload["draftIssue"] = draft_object(&item);
        return Ok(payload);
    }
    if field == "deleteProjectV2Field" {
        let f = store::field_by_id(
            pool,
            viewer,
            id(&input["fieldId"], &["PVTF", "PVTSSF", "PVTIF"])?,
        )
        .await
        .map_err(operation)?;
        let object = field_object(&f)?;
        store::delete_field(pool, actor, &request.auth.credential(), f.project_id, f.id)
            .await
            .map_err(operation)?;
        payload["projectV2Field"] = object;
        return Ok(payload);
    }
    let project = store::by_id(pool, viewer, id(&input["projectId"], &["PVT"])?)
        .await
        .map_err(operation)?;
    match field {
        "updateProjectV2" => {
            if input["readme"].as_str().is_some_and(|s| s.len() > 65536) {
                return Err(Error::new("Readme is too long"));
            }
            let settings = store::Settings {
                title: input["title"].as_str().map(str::to_owned),
                description: input["shortDescription"].as_str().map(str::to_owned),
                public: input["public"].as_bool(),
                closed: input["closed"].as_bool(),
            };
            let mut updated = store::save(pool, actor, &request.auth.credential(), project.id, &settings)
                .await
                .map_err(operation)?;
            if let Some(readme) = input["readme"].as_str() {
                updated = store::metadata(pool, actor, &request.auth.credential(), project.id, Some(readme), None)
                    .await
                    .map_err(operation)?;
            }
            payload["projectV2"] = project_object(&updated);
        }
        "markProjectV2AsTemplate" | "unmarkProjectV2AsTemplate" => {
            payload["projectV2"] = project_object(
                &store::metadata(
                    pool,
                    actor,
                    &request.auth.credential(),
                    project.id,
                    None,
                    Some(field == "markProjectV2AsTemplate"),
                )
                .await
                .map_err(operation)?,
            );
        }
        "deleteProjectV2" => {
            // gh selects the deleted project's counts and owner in the payload.
            let mut object = project_object(&project);
            let fields = store::fields(pool, viewer, project.id)
                .await
                .map_err(operation)?;
            object["_deletedFields"] = json!(project_fields(&object, &fields)?);
            let mut items = Vec::new();
            for item in store::items(pool, viewer, project.id)
                .await
                .map_err(operation)?
            {
                items.push(item_object(&item, request).await?);
            }
            object["_deletedItems"] = json!(items);
            store::delete(pool, actor, &request.auth.credential(), project.id)
                .await
                .map_err(operation)?;
            payload["projectV2"] = object;
        }
        "copyProjectV2" => {
            payload["projectV2"] = project_object(
                &store::copy(
                    pool,
                    actor,
                    &request.auth.credential(),
                    project.id,
                    owner(&input["ownerId"])?,
                    input["title"].as_str().ok_or_else(missing)?,
                    input["includeDraftIssues"].as_bool().unwrap_or(false),
                )
                .await
                .map_err(operation)?,
            );
        }
        "createProjectV2Field" => {
            let options = input["singleSelectOptions"].as_array().map(|xs| {
                xs.iter()
                    .map(|o| store::SelectOption {
                        name: o["name"].as_str().unwrap_or("").into(),
                        color: o["color"].as_str().map(str::to_owned),
                        description: o["description"].as_str().map(str::to_owned),
                    })
                    .collect()
            });
            let config = &input["iterationConfiguration"];
            let iteration = if config.is_null() {
                None
            } else {
                Some(store::IterationConfiguration {
                    start_date: serde_json::from_value(config["startDate"].clone())
                        .map_err(|_| Error::new("Invalid iteration start date"))?,
                    duration: config["duration"]
                        .as_i64()
                        .ok_or_else(missing)?
                        .try_into()
                        .map_err(unavailable)?,
                    iterations: config["iterations"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .map(|item| {
                            Ok(store::Iteration {
                                title: item["title"].as_str().ok_or_else(missing)?.to_owned(),
                                start_date: serde_json::from_value(item["startDate"].clone())
                                    .map_err(|_| Error::new("Invalid iteration start date"))?,
                                duration: item["duration"]
                                    .as_i64()
                                    .ok_or_else(missing)?
                                    .try_into()
                                    .map_err(unavailable)?,
                            })
                        })
                        .collect::<std::result::Result<Vec<_>, Error>>()?,
                })
            };
            let new = store::NewField {
                name: input["name"].as_str().unwrap_or("").into(),
                data_type: input["dataType"].as_str().unwrap_or("").to_lowercase(),
                single_select_options: options,
                iteration_configuration: iteration,
            };
            payload["projectV2Field"] = field_object(
                &store::add_field(pool, actor, &request.auth.credential(), project.id, &new)
                    .await
                    .map_err(operation)?,
            )?;
        }
        "addProjectV2ItemById" | "addProjectV2DraftIssue" => {
            let content = if field == "addProjectV2DraftIssue" {
                if input["assigneeIds"]
                    .as_array()
                    .is_some_and(|xs| !xs.is_empty())
                {
                    return Err(Error::new("Draft assignees are not supported"));
                }
                store::Content::Draft {
                    title: input["title"].as_str().unwrap_or("").into(),
                    body: input["body"].as_str().map(str::to_owned),
                }
            } else {
                let target = id(&input["contentId"], &["I", "PR"])?;
                if node_id::decode(input["contentId"].as_str().unwrap())
                    .unwrap()
                    .0
                    == "I"
                {
                    store::Content::Issue(target)
                } else {
                    store::Content::PullRequest(target)
                }
            };
            let item = store::add_item(pool, actor, &request.auth.credential(), project.id, &content)
                .await
                .map_err(operation)?;
            payload[if field == "addProjectV2DraftIssue" {
                "projectItem"
            } else {
                "item"
            }] = item_object(&item, request).await?;
        }
        "updateProjectV2ItemFieldValue" | "clearProjectV2ItemFieldValue" => {
            let field_id = id(&input["fieldId"], &["PVTF", "PVTSSF", "PVTIF"])?;
            let value = if field == "clearProjectV2ItemFieldValue" {
                Value::Null
            } else {
                let values: Vec<_> = input["value"]
                    .as_object()
                    .into_iter()
                    .flat_map(|o| o.iter())
                    .filter(|(_, v)| !v.is_null())
                    .collect();
                if values.len() != 1 {
                    return Err(Error::new("Exactly one field value must be supplied"));
                }
                if values[0].0 == "date" {
                    let raw = values[0].1.as_str().ok_or_else(missing)?;
                    let date = chrono::NaiveDate::parse_from_str(raw, "%Y-%m-%d")
                        .or_else(|_| {
                            chrono::DateTime::parse_from_rfc3339(raw).map(|date| date.date_naive())
                        })
                        .map_err(|_| Error::new("Invalid date"))?;
                    json!(date)
                } else {
                    values[0].1.clone()
                }
            };
            let item = store::update_item(
                pool,
                actor,
                &request.auth.credential(),
                project.id,
                id(&input["itemId"], &["PVTI"])?,
                &[store::FieldUpdate {
                    id: field_id,
                    value,
                }],
            )
            .await
            .map_err(operation)?;
            payload["projectV2Item"] = item_object(&item, request).await?;
        }
        "archiveProjectV2Item" | "unarchiveProjectV2Item" => {
            let item = store::archive_item(
                pool,
                actor,
                &request.auth.credential(),
                project.id,
                id(&input["itemId"], &["PVTI"])?,
                field == "archiveProjectV2Item",
            )
            .await
            .map_err(operation)?;
            payload["item"] = item_object(&item, request).await?;
        }
        "deleteProjectV2Item" => {
            store::delete_item(pool, actor, &request.auth.credential(), project.id, id(&input["itemId"], &["PVTI"])?)
                .await
                .map_err(operation)?;
            payload["deletedItemId"] = input["itemId"].clone();
        }
        "updateProjectV2ItemPosition" => {
            let after = if input["afterId"].is_null() {
                None
            } else {
                Some(id(&input["afterId"], &["PVTI"])?)
            };
            let item = store::reorder_item(
                pool,
                actor,
                &request.auth.credential(),
                project.id,
                id(&input["itemId"], &["PVTI"])?,
                after,
            )
            .await
            .map_err(operation)?;
            payload["items"] = json!({"nodes":[item_object(&item,request).await?],"edges":[],"totalCount":1,"pageInfo":{"hasNextPage":false,"hasPreviousPage":false,"startCursor":null,"endCursor":null}});
        }
        "linkProjectV2ToTeam" | "unlinkProjectV2FromTeam" => {
            store::link_team(
                pool,
                actor,
                &request.auth.credential(),
                project.id,
                id(&input["teamId"], &["T"])?,
                field == "linkProjectV2ToTeam",
            )
            .await
            .map_err(operation)?;
        }
        "linkProjectV2ToRepository" | "unlinkProjectV2FromRepository" => {
            let repo = repository::by_id(id(&input["repositoryId"], &["R"])?, request).await?;
            store::link_repository(
                pool,
                actor,
                &request.auth.credential(),
                project.id,
                repo.id,
                field == "linkProjectV2ToRepository",
            )
            .await
            .map_err(operation)?;
            payload["repository"] = repository::object(&repo);
        }
        _ => {
            return Err(Error::new(format!(
                "Codeyard does not support Mutation.{field} yet"
            )))
        }
    }
    Ok(payload)
}
