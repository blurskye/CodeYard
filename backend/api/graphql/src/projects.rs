//! `gh project` resolvers over the shared owner-scoped project domain.
use crate::arg;
use crate::arguments::Arguments;
use crate::bindings::boxed;
use crate::missing;
use crate::unavailable;
use crate::url;
use crate::RequestContext;
use crate::Result;
use async_graphql::Error;
use async_graphql::ErrorExtensions;
use chrono::Datelike;
use codeyard_types::node_id;
use serde_json::json;
use serde_json::Value;

fn operation(error: anyhow::Error) -> Error {
    match error.downcast_ref::<codeyard_types::project_query::Refusal>() {
        Some(codeyard_types::project_query::Refusal::Missing) => missing(),
        Some(refusal) => Error::new(refusal.to_string()).extend_with(|_, ext| {
            ext.set(
                "type",
                if matches!(refusal, codeyard_types::project_query::Refusal::Denied) {
                    "FORBIDDEN"
                } else {
                    "UNPROCESSABLE"
                },
            );
        }),
        None => unavailable(error),
    }
}
fn date(value: &Value) -> Result<chrono::NaiveDate> {
    let raw = value.as_str().ok_or_else(missing)?;
    chrono::NaiveDate::parse_from_str(raw, "%Y-%m-%d")
        .or_else(|_| chrono::DateTime::parse_from_rfc3339(raw).map(|date| date.date_naive()))
        .map_err(|_| Error::new("Invalid date"))
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
fn owner(value: &Value) -> Result<codeyard_db_repos::projects::Owner> {
    let id = id(value, &["U", "O"])?;
    Ok(
        if node_id::decode(value.as_str().unwrap()).unwrap().0 == "O" {
            codeyard_db_repos::projects::Owner::Organization(id)
        } else {
            codeyard_db_repos::projects::Owner::User(id)
        },
    )
}
fn project_owner(
    project: &codeyard_db_repos::projects::Project,
) -> codeyard_db_repos::projects::Owner {
    if project.owner_kind == "org" {
        codeyard_db_repos::projects::Owner::Organization(project.owner_id)
    } else {
        codeyard_db_repos::projects::Owner::User(project.owner_id)
    }
}
fn project_object(p: &codeyard_db_repos::projects::Project) -> Value {
    json!({"__typename":"ProjectV2","id":node_id::scoped("PVT",p.owner_id,p.id),"databaseId":p.id,"number":p.number,"title":p.title,"shortDescription":p.description,"readme":p.readme,"public":p.public,"closed":p.closed_at.is_some(),"closedAt":p.closed_at,"template":p.template,"createdAt":p.created_at,"updatedAt":p.updated_at,"_project":p.id,"_ownerKind":p.owner_kind,"_owner":p.owner_id,"_creator":p.creator_id})
}
fn field_object(f: &codeyard_db_repos::projects::Field) -> Result<Value> {
    let config = codeyard_db_repos::projects::field_configuration(f).map_err(operation)?;
    let (ty, prefix) = match f.data_type.as_str() {
        "single_select" => ("ProjectV2SingleSelectField", "PVTSSF"),
        "iteration" => ("ProjectV2IterationField", "PVTIF"),
        _ => ("ProjectV2Field", "PVTF"),
    };
    let mut value = json!({"__typename":ty,"id":node_id::scoped(prefix,f.owner_id,f.id),"databaseId":f.id,"name":f.name,"dataType":f.data_type.to_uppercase(),"isIssueField":false,"createdAt":f.created_at,"updatedAt":f.updated_at,"_project":f.project_id,"_options":config["options"]});
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
const BUILT_IN_FIELDS: [(codeyard_db_repos::projects::BuiltInField, &str, &str); 6] = [
    (
        codeyard_db_repos::projects::BuiltInField::Assignees,
        "ASSIGNEES",
        "assignees",
    ),
    (
        codeyard_db_repos::projects::BuiltInField::Labels,
        "LABELS",
        "labels",
    ),
    (
        codeyard_db_repos::projects::BuiltInField::LinkedPullRequests,
        "LINKED_PULL_REQUESTS",
        "closedByPullRequestsReferences",
    ),
    (
        codeyard_db_repos::projects::BuiltInField::Milestone,
        "MILESTONE",
        "milestone",
    ),
    (
        codeyard_db_repos::projects::BuiltInField::Repository,
        "REPOSITORY",
        "repository",
    ),
    (
        codeyard_db_repos::projects::BuiltInField::Reviewers,
        "REVIEWERS",
        "reviewRequests",
    ),
];
fn built_in_field(project: &Value, name: &str, data_type: &str) -> Value {
    let id = project["_project"].as_i64().unwrap_or_default();
    json!({"__typename":"ProjectV2Field","id":node_id::project_field(project["_owner"].as_i64().unwrap_or_default(),id,data_type),"databaseId":null,"name":name,"dataType":data_type,"isIssueField":false,
        "createdAt":project["createdAt"],"updatedAt":project["updatedAt"],"_project":id})
}
fn order_fields(ctx: &Arguments, values: &mut [Value]) -> Result<()> {
    if let Some(order) = ctx.args.get("orderBy").filter(|value| !value.is_null()) {
        let order = order.object()?;
        let field = order.try_get("field")?.enum_name()?;
        if field != "POSITION" {
            let key = if field == "NAME" { "name" } else { "createdAt" };
            values.sort_by(|a, b| {
                a[key]
                    .as_str()
                    .cmp(&b[key].as_str())
                    .then(a["id"].as_str().cmp(&b["id"].as_str()))
            });
        }
        if order.try_get("direction")?.enum_name()? == "DESC" {
            values.reverse();
        }
    }
    Ok(())
}

/// A project's stored fields followed by the built-in ones.
fn project_fields(
    project: &Value,
    fields: &[codeyard_db_repos::projects::Field],
) -> Result<Vec<Value>> {
    let mut values: Vec<_> = fields.iter().map(field_object).collect::<Result<_>>()?;
    values.extend(
        BUILT_IN_FIELDS
            .iter()
            .map(|(field, data_type, _)| built_in_field(project, field.name(), data_type)),
    );
    Ok(values)
}
/// The built-in field values of an issue or pull request item. Empty values are left out,
/// as GitHub leaves them out.
async fn built_in_values(
    item: &codeyard_db_repos::projects::Item,
    request: &RequestContext,
    selected: Option<&str>,
    include_deleted: bool,
) -> Result<Vec<Value>> {
    if selected.is_some_and(|name| {
        !BUILT_IN_FIELDS
            .iter()
            .any(|(field, _, _)| field.name() == name)
    }) {
        return Ok(Vec::new());
    }
    let ty = item.content_type();
    if ty == "DraftIssue" {
        return Ok(Vec::new());
    }
    let project = project_object(
        &codeyard_db_repos::projects::by_id_at(
            &request.state.pool,
            Some(request.auth.user.id),
            item.project_id,
            include_deleted,
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
    for (field, data_type, source) in BUILT_IN_FIELDS {
        let name = field.name();
        if selected.is_some_and(|selected| selected != name) {
            continue;
        }
        let applies = match data_type {
            "REVIEWERS" => ty == "PullRequest",
            "LINKED_PULL_REQUESTS" => ty == "Issue",
            _ => true,
        };
        if !applies {
            continue;
        }
        if data_type == "REVIEWERS" {
            let pull = content["databaseId"].as_i64().ok_or_else(missing)?;
            if crate::review_requests::count(pull, request).await? > 0 {
                values.push(json!({"__typename":"ProjectV2ItemFieldReviewerValue","_project":item.project_id,"_reviewPull":pull,"field":built_in_field(&project, name, data_type)}));
            }
            continue;
        }
        let resolved = boxed(|| crate::issue::resolve(ty, source, &all, request, &content)).await?;
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
fn draft_object(item: &codeyard_db_repos::projects::Item) -> Value {
    json!({"__typename":"DraftIssue","id":node_id::scoped("DI",item.owner_id,item.id),"title":item.draft_title,"body":item.draft_body.as_deref().unwrap_or(""),"createdAt":item.created_at,"updatedAt":item.updated_at,"_creator":item.creator_id,"_project":item.project_id})
}
fn charge_item(item: &codeyard_db_repos::projects::Item, request: &RequestContext) -> Result<()> {
    request.charge(
        item.field_values.len()
            + item.title.len()
            + item.draft_title.as_ref().map_or(0, |s| s.len())
            + item.draft_body.as_ref().map_or(0, |s| s.len())
            + 256,
    )
}
fn charge_fields(
    fields: &[codeyard_db_repos::projects::Field],
    request: &RequestContext,
) -> Result<()> {
    request.charge(
        fields
            .iter()
            .map(|field| field.configuration.len() + field.name.len() + 128)
            .sum(),
    )
}

async fn item_object(
    item: &codeyard_db_repos::projects::Item,
    request: &RequestContext,
) -> Result<Value> {
    charge_item(item, request)?;
    let content = if let Some(id) = item.issue_id {
        let issue = codeyard_db_repos::issues::get_issue_by_id(&request.state.pool, id)
            .await
            .map_err(unavailable)?
            .ok_or_else(missing)?;
        crate::repository::by_id(issue.repo_id, request).await?;
        request.authorize(issue.repo_id, "issues", false).await?;
        request.charge(issue.body.len() + issue.title.len())?;
        crate::issue::object(&issue)
    } else if let Some(id) = item.pull_request_id {
        let pull = codeyard_db_repos::pull_requests::records::get_pull_request_by_id(
            &request.state.pool,
            id,
        )
        .await
        .map_err(unavailable)?
        .ok_or_else(missing)?;
        crate::repository::by_id(pull.repo_id, request).await?;
        request
            .authorize(pull.repo_id, "pull-requests", false)
            .await?;
        request.charge(pull.body.len() + pull.title.len())?;
        crate::issue::pull_object(&pull)
    } else {
        draft_object(item)
    };
    Ok(
        json!({"__typename":"ProjectV2Item","id":node_id::scoped("PVTI",item.owner_id,item.id),"databaseId":item.id,"isArchived":item.archived,"createdAt":item.created_at,"updatedAt":item.updated_at,"content":content,"_project":item.project_id,"_creator":item.creator_id,"_item":item.id}),
    )
}

/// Projects belong to accounts. Job tokens share one bot identity, so they never reach them.
fn authorize_account(request: &RequestContext) -> Result<()> {
    if request.bounded() {
        return Err(crate::token_denied());
    }
    request.require_oauth_scope("repo")
}

pub(super) async fn computed_field(id: &str, request: &RequestContext) -> Result<Value> {
    authorize_account(request)?;
    let (owner, project, field) = node_id::decode_project_field(id).ok_or_else(missing)?;
    let project = codeyard_db_repos::projects::by_id(
        &request.state.pool,
        Some(request.auth.user.id),
        project,
    )
    .await
    .map_err(operation)?;
    if owner.is_some_and(|owner| owner != project.owner_id) {
        return Err(missing());
    }
    let (name, data_type, _) = BUILT_IN_FIELDS
        .iter()
        .find(|(_, data_type, _)| *data_type == field)
        .ok_or_else(missing)?;
    Ok(built_in_field(
        &project_object(&project),
        name.name(),
        data_type,
    ))
}

pub(super) async fn node(kind: &str, id: i64, request: &RequestContext) -> Result<Value> {
    authorize_account(request)?;
    let pool = &request.state.pool;
    let viewer = Some(request.auth.user.id);
    match kind {
        "PVT" => Ok(project_object(
            &codeyard_db_repos::projects::by_id(pool, viewer, id)
                .await
                .map_err(operation)?,
        )),
        "PVTF" | "PVTSSF" | "PVTIF" => {
            let object = field_object(
                &codeyard_db_repos::projects::field_by_id(pool, viewer, id)
                    .await
                    .map_err(operation)?,
            )?;
            if node_id::decode(object["id"].as_str().unwrap()).unwrap().0 != kind {
                return Err(missing());
            }
            Ok(object)
        }
        "PVTI" | "DI" => {
            let item = codeyard_db_repos::projects::item_by_id(pool, viewer, id)
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
        codeyard_db_repos::projects::Content::Issue(id)
    } else {
        codeyard_db_repos::projects::Content::PullRequest(id)
    };
    let include_archived = ctx
        .args
        .get("includeArchived")
        .filter(|v| !v.is_null())
        .map(|v| v.deserialize::<bool>())
        .transpose()?
        .unwrap_or(true);
    crate::connection::validate(ctx)?;
    let total = codeyard_db_repos::projects::content_items(
        pool,
        viewer,
        &content,
        include_archived,
        0,
        0,
        true,
    )
    .await
    .map_err(operation)?
    .0;
    let page = crate::connection::Page::new(ctx, total)?;
    let rows = if page.limit == 0 {
        Vec::new()
    } else {
        codeyard_db_repos::projects::content_items(
            pool,
            viewer,
            &content,
            include_archived,
            page.offset,
            page.limit,
            false,
        )
        .await
        .map_err(operation)?
        .1
    };
    let mut nodes = Vec::new();
    for item in rows {
        nodes.push(item_object(&item, request).await?);
    }
    Ok(page.finish(nodes))
}

/// What a project deleted in this request held, kept in its answer since deletion purges it.
fn removed_contents(
    parent: &Value,
) -> Result<Option<codeyard_db_repos::projects::ProjectContents>> {
    parent
        .get("_removed")
        .map(|removed| serde_json::from_value(removed.clone()).map_err(unavailable))
        .transpose()
}

async fn project_items(ctx: &Arguments, request: &RequestContext, parent: &Value) -> Result<Value> {
    crate::connection::validate(ctx)?;
    let pool = &request.state.pool;
    let viewer = Some(request.auth.user.id);
    let project = parent["_project"].as_i64().ok_or_else(missing)?;
    let include_deleted = parent["_deleted"] == true;
    let states: Option<Vec<String>> = ctx
        .args
        .get("archivedStates")
        .filter(|value| !value.is_null())
        .map(|value| value.deserialize())
        .transpose()?;
    let archived = match states {
        None => Some(false),
        Some(states) => match (
            states.iter().any(|value| value == "ARCHIVED"),
            states.iter().any(|value| value == "NOT_ARCHIVED"),
        ) {
            (true, true) => None,
            (true, false) => Some(true),
            (false, true) => Some(false),
            (false, false) => return Ok(crate::connection::Page::new(ctx, 0)?.finish(Vec::new())),
        },
    };
    let query = ctx
        .args
        .get("query")
        .filter(|value| !value.is_null())
        .map(|value| value.string())
        .transpose()?
        .unwrap_or("");
    let descending = if let Some(order) = ctx.args.get("orderBy").filter(|value| !value.is_null()) {
        order.object()?.try_get("direction")?.enum_name()? == "DESC"
    } else {
        false
    };
    let removed = removed_contents(parent)?;
    let (page, rows) = if query.trim().is_empty() && removed.is_none() {
        let mut window = codeyard_db_repos::projects::ItemWindow {
            project,
            include_deleted,
            archived,
            descending,
            offset: 0,
            limit: 0,
            count: true,
        };
        let total = codeyard_db_repos::projects::items_window(pool, viewer, &window)
            .await
            .map_err(operation)?
            .0;
        let page = crate::connection::Page::new(ctx, total)?;
        window.offset = page.offset;
        window.limit = page.limit;
        window.count = false;
        let rows = if page.limit == 0 {
            Vec::new()
        } else {
            codeyard_db_repos::projects::items_window(pool, viewer, &window)
                .await
                .map_err(operation)?
                .1
        };
        (page, rows)
    } else {
        let (fields, rows) = match removed {
            Some(removed) => (removed.fields, removed.items),
            None => (
                codeyard_db_repos::projects::fields_at(pool, viewer, project, include_deleted)
                    .await
                    .map_err(operation)?,
                codeyard_db_repos::projects::items_at(pool, viewer, project, include_deleted)
                    .await
                    .map_err(operation)?,
            ),
        };
        charge_fields(&fields, request)?;
        for item in &rows {
            charge_item(item, request)?;
        }
        let rows = rows
            .into_iter()
            .filter(|item| archived.is_none_or(|value| value == item.archived))
            .collect();
        let mut rows =
            codeyard_app::projects::filter_items(rows, &fields, query, None).map_err(operation)?;
        if descending {
            rows.reverse();
        }
        let page = crate::connection::Page::new(ctx, rows.len() as i64)?;
        let rows = rows
            .into_iter()
            .skip(page.offset as usize)
            .take(page.limit as usize)
            .collect();
        (page, rows)
    };
    let mut nodes = Vec::new();
    for item in rows {
        nodes.push(item_object(&item, request).await?);
    }
    Ok(page.finish(nodes))
}

async fn project_connection(
    ctx: &Arguments,
    request: &RequestContext,
    mut filter: codeyard_db_repos::projects::ListFilter,
) -> Result<Value> {
    let query = ctx
        .args
        .get("query")
        .filter(|q| !q.is_null())
        .map(|q| q.string())
        .transpose()?
        .unwrap_or("");
    crate::connection::validate(ctx)?;
    let mut terms = Vec::new();
    for term in query.split_whitespace() {
        match term.to_ascii_lowercase().as_str() {
            "is:open" => filter.open = true,
            "is:closed" => filter.closed = true,
            _ => terms.push(term),
        }
    }
    if !terms.is_empty() {
        filter.titles.push(terms.join(" "));
    }
    if let Some(permission) = ctx.args.get("minPermissionLevel").filter(|p| !p.is_null()) {
        filter.writable = permission.enum_name()? != "READ";
    }
    if let Some(order) = ctx.args.get("orderBy").filter(|o| !o.is_null()) {
        let order: Value = order.deserialize()?;
        filter.order = match order["field"].as_str() {
            Some("TITLE") => codeyard_db_repos::projects::ListOrder::Title,
            Some("UPDATED_AT") => codeyard_db_repos::projects::ListOrder::Updated,
            Some("CREATED_AT") => codeyard_db_repos::projects::ListOrder::Created,
            Some("NUMBER") => codeyard_db_repos::projects::ListOrder::Number,
            _ => return Err(Error::new("Unsupported project order")),
        };
        filter.descending = order["direction"] == "DESC";
    }
    let pool = &request.state.pool;
    let viewer = Some(request.auth.user.id);
    let (count, _) = codeyard_db_repos::projects::list(pool, viewer, &filter, 0, 0)
        .await
        .map_err(operation)?;
    let page = crate::connection::Page::new(ctx, count)?;
    let (_, projects) =
        codeyard_db_repos::projects::list(pool, viewer, &filter, page.offset, page.limit)
            .await
            .map_err(operation)?;
    let nodes = projects
        .iter()
        .map(|project| {
            let mut value = project_object(project);
            value.as_object_mut().unwrap().remove("readme");
            value
        })
        .collect();
    Ok(page.finish(nodes))
}

pub(super) async fn resource(ctx: &Arguments, request: &RequestContext) -> Result<Value> {
    authorize_account(request)?;
    let raw: String = ctx.args.try_get("url")?.deserialize()?;
    let parsed = reqwest::Url::parse(&raw).map_err(|_| missing())?;
    let origin =
        reqwest::Url::parse(&codeyard_types::http_config::link_origin()).map_err(unavailable)?;
    if parsed.origin() != origin.origin() {
        return Err(missing());
    }
    let parts: Vec<_> = parsed.path().trim_matches('/').split('/').collect();
    if parts.len() != 4 || !matches!(parts[2], "issues" | "pull" | "pulls") {
        return Err(missing());
    }
    let repo = codeyard_db_repos::repositories::records::get_repository(
        &request.state.pool,
        parts[0],
        parts[1],
    )
    .await
    .map_err(unavailable)?
    .ok_or_else(missing)?;
    crate::repository::by_id(repo.id, request).await?;
    let number = parts[3].parse().map_err(|_| missing())?;
    if parts[2] == "issues" {
        let entry = codeyard_db_repos::issues::get_issue(&request.state.pool, repo.id, number)
            .await
            .map_err(unavailable)?
            .ok_or_else(missing)?;
        Ok(crate::issue::object(&entry))
    } else {
        let entry = codeyard_db_repos::pull_requests::records::get_pull_request(
            &request.state.pool,
            repo.id,
            number,
        )
        .await
        .map_err(unavailable)?
        .ok_or_else(missing)?;
        Ok(crate::issue::pull_object(&entry))
    }
}

fn mark_deleted(value: &mut Value, project: i64) {
    match value {
        Value::Array(values) => {
            for value in values {
                mark_deleted(value, project);
            }
        }
        Value::Object(values) => {
            if values.get("_project").and_then(Value::as_i64) == Some(project) {
                values.insert("_deleted".into(), json!(true));
            }
            for value in values.values_mut() {
                mark_deleted(value, project);
            }
        }
        _ => {}
    }
}

pub(super) async fn resolve(
    ty: &str,
    field: &str,
    ctx: &Arguments,
    request: &RequestContext,
    parent: &Value,
) -> Result<Value> {
    let mut value = resolve_at(ty, field, ctx, request, parent).await?;
    if parent["_deleted"] == true {
        mark_deleted(&mut value, parent["_project"].as_i64().ok_or_else(missing)?);
    }
    Ok(value)
}

async fn resolve_at(
    ty: &str,
    field: &str,
    ctx: &Arguments,
    request: &RequestContext,
    parent: &Value,
) -> Result<Value> {
    authorize_account(request)?;
    if let Some(id) = parent["_project"].as_i64() {
        let project = codeyard_db_repos::projects::by_id_at(
            &request.state.pool,
            Some(request.auth.user.id),
            id,
            parent["_deleted"] == true,
        )
        .await
        .map_err(operation)?;
        request.charge(
            project.readme.len()
                + project.title.len()
                + project.description.as_ref().map_or(0, |text| text.len())
                + 128,
        )?;
        if project.owner_kind == "org" {
            request.authorize_organization(project.owner_id).await?;
        }
    }
    if let Some(value) = parent.get(field) {
        return Ok(value.clone());
    }
    let pool = &request.state.pool;
    let actor = request.auth.user.id;
    let viewer = Some(actor);
    if field == "projects" && matches!(ty, "Repository" | "Organization") {
        if ty == "Repository" {
            crate::repository::by_id(parent["databaseId"].as_i64().ok_or_else(missing)?, request)
                .await?;
        }
        // Classic projects are retired; gh still asks for them before its v2 lookup.
        return crate::connection::slice(ctx, Vec::new());
    }
    if ty == "Repository" && field == "projectsV2" {
        let repo = parent["databaseId"].as_i64().ok_or_else(missing)?;
        crate::repository::by_id(repo, request).await?;
        return project_connection(
            ctx,
            request,
            codeyard_db_repos::projects::ListFilter {
                repo: Some(repo),
                ..Default::default()
            },
        )
        .await;
    }
    if matches!(ty, "User" | "Organization") {
        let target = parent["databaseId"].as_i64().ok_or_else(missing)?;
        let owner = if ty == "User" {
            codeyard_db_repos::projects::Owner::User(target)
        } else {
            codeyard_db_repos::projects::Owner::Organization(target)
        };
        return match field {
            "team" if ty == "Organization" => {
                crate::team::by_slug(target, &arg(ctx, "slug")?, request).await
            }
            "projectV2" => Ok(project_object(
                &codeyard_db_repos::projects::get(
                    pool,
                    viewer,
                    owner,
                    ctx.args.try_get("number")?.i64()?,
                )
                .await
                .map_err(operation)?,
            )),
            "projectsV2" => {
                project_connection(
                    ctx,
                    request,
                    codeyard_db_repos::projects::ListFilter {
                        owner: Some(owner),
                        ..Default::default()
                    },
                )
                .await
            }
            "viewerCanCreateProjects" => Ok(json!(codeyard_db_repos::projects::can_write(
                pool, actor, owner
            )
            .await
            .map_err(operation)?)),
            _ => Err(Error::new("Unsupported project owner field")),
        };
    }
    match (ty, field) {
        ("ProjectV2", "readme") => {
            let project = codeyard_db_repos::projects::by_id_at(
                pool,
                viewer,
                parent["databaseId"].as_i64().ok_or_else(missing)?,
                parent["_deleted"] == true,
            )
            .await
            .map_err(operation)?;
            Ok(json!(project.readme))
        }
        (_, "project") if parent["_deleted"] == true => Ok(project_object(
            &codeyard_db_repos::projects::by_id_at(
                pool,
                viewer,
                parent["_project"].as_i64().ok_or_else(missing)?,
                true,
            )
            .await
            .map_err(operation)?,
        )),
        (_, "project") => {
            node(
                "PVT",
                parent["_project"].as_i64().ok_or_else(missing)?,
                request,
            )
            .await
        }
        (_, "creator") => {
            crate::user::by_id(parent["_creator"].as_i64().ok_or_else(missing)?, request).await
        }
        ("ProjectV2", "owner") => {
            if parent["_ownerKind"] == "org" {
                crate::user::organization_by_id(
                    parent["_owner"].as_i64().ok_or_else(missing)?,
                    request,
                )
                .await
            } else {
                crate::user::by_id(parent["_owner"].as_i64().ok_or_else(missing)?, request).await
            }
        }
        ("ProjectV2", "url" | "resourcePath") => {
            let owner = if parent["_ownerKind"] == "org" {
                crate::user::organization_by_id(
                    parent["_owner"].as_i64().ok_or_else(missing)?,
                    request,
                )
                .await?
            } else {
                crate::user::by_id(parent["_owner"].as_i64().ok_or_else(missing)?, request).await?
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
        ("ProjectV2", "viewerCanUpdate" | "viewerCanClose" | "viewerCanReopen")
            if parent["_deleted"] == true =>
        {
            Ok(json!(false))
        }
        ("ProjectV2", "viewerCanUpdate" | "viewerCanClose" | "viewerCanReopen") => {
            let project = codeyard_db_repos::projects::by_id(
                pool,
                viewer,
                parent["_project"].as_i64().ok_or_else(missing)?,
            )
            .await
            .map_err(operation)?;
            Ok(json!(codeyard_db_repos::projects::can_write(
                pool,
                actor,
                project_owner(&project)
            )
            .await
            .map_err(operation)?))
        }
        ("ProjectV2", "fields" | "field") => {
            let fields = match removed_contents(parent)? {
                Some(removed) => removed.fields,
                None => codeyard_db_repos::projects::fields_at(
                    pool,
                    viewer,
                    parent["_project"].as_i64().ok_or_else(missing)?,
                    parent["_deleted"] == true,
                )
                .await
                .map_err(operation)?,
            };
            charge_fields(&fields, request)?;
            let mut values = project_fields(parent, &fields)?;
            if field == "field" {
                let name = arg(ctx, "name")?;
                return Ok(values
                    .into_iter()
                    .find(|f| f["name"] == name)
                    .unwrap_or(Value::Null));
            }
            order_fields(ctx, &mut values)?;
            crate::connection::slice(ctx, values)
        }
        ("ProjectV2", "items") => project_items(ctx, request, parent).await,
        ("ProjectV2Item", "fieldValues" | "fieldValueByName") => {
            let item = codeyard_db_repos::projects::item_by_id_at(
                pool,
                viewer,
                parent["_item"].as_i64().ok_or_else(missing)?,
                parent["_deleted"] == true,
            )
            .await
            .map_err(operation)?;
            charge_item(&item, request)?;
            let fields = codeyard_db_repos::projects::fields_at(
                pool,
                viewer,
                item.project_id,
                parent["_deleted"] == true,
            )
            .await
            .map_err(operation)?;
            charge_fields(&fields, request)?;
            let selected = if field == "fieldValueByName" {
                Some(arg(ctx, "name")?)
            } else {
                None
            };
            let mut values = Vec::new();
            for f in fields {
                if selected.as_ref().is_some_and(|name| name != &f.name) {
                    continue;
                }
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
                            json!(codeyard_app::projects::display_value(&f, &value)
                                .map_err(operation)?);
                        entry["optionId"] = value;
                        "SingleSelect"
                    }
                    "iteration" => {
                        let config = codeyard_db_repos::projects::field_configuration(&f)
                            .map_err(operation)?;
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
            if selected.is_some() && !values.is_empty() {
                return Ok(values.remove(0));
            }
            values.extend(
                built_in_values(
                    &item,
                    request,
                    selected.as_deref(),
                    parent["_deleted"] == true,
                )
                .await?,
            );
            if let Some(name) = selected {
                return Ok(values
                    .into_iter()
                    .find(|value| value["field"]["name"] == name)
                    .unwrap_or(Value::Null));
            }
            if let Some(order) = ctx.args.get("orderBy").filter(|value| !value.is_null()) {
                if order.object()?.try_get("direction")?.enum_name()? == "DESC" {
                    values.reverse();
                }
            }
            crate::connection::slice(ctx, values)
        }
        ("ProjectV2ItemFieldReviewerValue", "reviewers") => {
            crate::review_requests::reviewers(
                ctx,
                request,
                parent["_reviewPull"].as_i64().ok_or_else(missing)?,
            )
            .await
        }
        (
            "ProjectV2ItemFieldLabelValue"
            | "ProjectV2ItemFieldPullRequestValue"
            | "ProjectV2ItemFieldUserValue",
            "labels" | "pullRequests" | "users",
        ) => crate::connection::slice(
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
        let project = codeyard_db_repos::projects::create(
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
        let item = codeyard_db_repos::projects::edit_draft(
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
        let f = codeyard_db_repos::projects::field_by_id(
            pool,
            viewer,
            id(&input["fieldId"], &["PVTF", "PVTSSF", "PVTIF"])?,
        )
        .await
        .map_err(operation)?;
        let object = field_object(&f)?;
        codeyard_db_repos::projects::delete_field(
            pool,
            actor,
            &request.auth.credential(),
            f.project_id,
            f.id,
        )
        .await
        .map_err(operation)?;
        payload["projectV2Field"] = object;
        return Ok(payload);
    }
    let project =
        codeyard_db_repos::projects::by_id(pool, viewer, id(&input["projectId"], &["PVT"])?)
            .await
            .map_err(operation)?;
    match field {
        "updateProjectV2" => {
            if let Some(readme) = input["readme"].as_str() {
                codeyard_types::issue_input::validate_body(readme, false).map_err(Error::new)?;
            }
            let settings = codeyard_db_repos::projects::Settings {
                title: input["title"].as_str().map(str::to_owned),
                description: input["shortDescription"].as_str().map(str::to_owned),
                public: input["public"].as_bool(),
                closed: input["closed"].as_bool(),
            };
            let mut updated = codeyard_db_repos::projects::save(
                pool,
                actor,
                &request.auth.credential(),
                project.id,
                &settings,
            )
            .await
            .map_err(operation)?;
            if let Some(readme) = input["readme"].as_str() {
                updated = codeyard_db_repos::projects::metadata(
                    pool,
                    actor,
                    &request.auth.credential(),
                    project.id,
                    Some(readme),
                    None,
                )
                .await
                .map_err(operation)?;
            }
            payload["projectV2"] = project_object(&updated);
        }
        "markProjectV2AsTemplate" | "unmarkProjectV2AsTemplate" => {
            payload["projectV2"] = project_object(
                &codeyard_db_repos::projects::metadata(
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
            let removed = codeyard_db_repos::projects::delete(
                pool,
                actor,
                &request.auth.credential(),
                project.id,
            )
            .await
            .map_err(operation)?;
            let mut object = project_object(&project);
            // Deletion purges the fields and items, so the answer keeps what it removed.
            object["_removed"] = serde_json::to_value(&removed).map_err(unavailable)?;
            object["_deleted"] = json!(true);
            payload["projectV2"] = object;
        }
        "copyProjectV2" => {
            payload["projectV2"] = project_object(
                &codeyard_db_repos::projects::copy(
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
            let options = input
                .get("singleSelectOptions")
                .filter(|value| !value.is_null())
                .map(|value| {
                    crate::metadata_mutations::list(value)
                        .iter()
                        .map(|o| codeyard_db_repos::projects::SelectOption {
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
                Some(codeyard_db_repos::projects::IterationConfiguration {
                    start_date: date(&config["startDate"])?,
                    duration: config["duration"]
                        .as_i64()
                        .ok_or_else(missing)?
                        .try_into()
                        .map_err(unavailable)?,
                    iterations: crate::metadata_mutations::list(&config["iterations"])
                        .iter()
                        .map(|item| {
                            Ok(codeyard_db_repos::projects::Iteration {
                                title: item["title"].as_str().ok_or_else(missing)?.to_owned(),
                                start_date: date(&item["startDate"])?,
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
            let new = codeyard_db_repos::projects::NewField {
                name: input["name"].as_str().unwrap_or("").into(),
                data_type: input["dataType"].as_str().unwrap_or("").to_lowercase(),
                single_select_options: options,
                iteration_configuration: iteration,
            };
            payload["projectV2Field"] = field_object(
                &codeyard_db_repos::projects::add_field(
                    pool,
                    actor,
                    &request.auth.credential(),
                    project.id,
                    &new,
                )
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
                codeyard_db_repos::projects::Content::Draft {
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
                    codeyard_db_repos::projects::Content::Issue(target)
                } else {
                    codeyard_db_repos::projects::Content::PullRequest(target)
                }
            };
            let item = codeyard_db_repos::projects::add_item(
                pool,
                actor,
                &request.auth.credential(),
                project.id,
                &content,
            )
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
                    json!(date(values[0].1)?)
                } else {
                    values[0].1.clone()
                }
            };
            let item = codeyard_db_repos::projects::update_item(
                pool,
                actor,
                &request.auth.credential(),
                project.id,
                id(&input["itemId"], &["PVTI"])?,
                &[codeyard_db_repos::projects::FieldUpdate {
                    id: field_id,
                    value,
                }],
            )
            .await
            .map_err(operation)?;
            payload["projectV2Item"] = item_object(&item, request).await?;
        }
        "archiveProjectV2Item" | "unarchiveProjectV2Item" => {
            let item = codeyard_db_repos::projects::archive_item(
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
            codeyard_db_repos::projects::delete_item(
                pool,
                actor,
                &request.auth.credential(),
                project.id,
                id(&input["itemId"], &["PVTI"])?,
            )
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
            codeyard_db_repos::projects::reorder_item(
                pool,
                actor,
                &request.auth.credential(),
                project.id,
                id(&input["itemId"], &["PVTI"])?,
                after,
            )
            .await
            .map_err(operation)?;
            payload["_project"] = json!(project.id);
        }
        "linkProjectV2ToTeam" | "unlinkProjectV2FromTeam" => {
            codeyard_db_repos::projects::link_team(
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
            let repo =
                crate::repository::by_id(id(&input["repositoryId"], &["R"])?, request).await?;
            codeyard_db_repos::projects::link_repository(
                pool,
                actor,
                &request.auth.credential(),
                project.id,
                repo.id,
                field == "linkProjectV2ToRepository",
            )
            .await
            .map_err(operation)?;
            payload["repository"] = crate::repository::object(&repo);
        }
        _ => {
            return Err(Error::new(format!(
                "Codeyard does not support Mutation.{field} yet"
            )))
        }
    }
    Ok(payload)
}
