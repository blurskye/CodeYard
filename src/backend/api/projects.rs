//! Project pages and plain forms use the same project operations as REST.
use crate::backend::{
    api::{
        http::context::PageContext,
        user::responses::{field, mutation_guard, Fields},
    },
    app::AppState,
    db::{
        self,
        projects::{self, Content, NewView, Owner},
    },
};
use crate::webui::{app::Page, pages::projects as view};
use axum::{
    extract::{Form, Path, State},
    http::{HeaderMap, StatusCode, Uri},
    response::Response,
    routing::get,
    Router,
};
use serde_json::{json, Value};
use std::{collections::HashMap, sync::Arc};

type Params = HashMap<String, String>;

pub fn router() -> Router<Arc<AppState>> {
    Router::new()
        .route("/users/:username/projects", get(show).post(mutate))
        .route(
            "/users/:username/projects/:project_number",
            get(show).post(mutate),
        )
        .route(
            "/users/:username/projects/:project_number/settings",
            get(show),
        )
        .route(
            "/users/:username/projects/:project_number/views/:view_number",
            get(show),
        )
        .route("/orgs/:org/projects", get(show).post(mutate))
        .route(
            "/orgs/:org/projects/:project_number",
            get(show).post(mutate),
        )
        .route("/orgs/:org/projects/:project_number/settings", get(show))
        .route(
            "/orgs/:org/projects/:project_number/views/:view_number",
            get(show),
        )
}

pub(crate) async fn sidebar(
    ctx: &PageContext,
    repo: i64,
    kind: crate::webui::pages::repos::issues::Kind,
    id: i64,
) -> anyhow::Result<Vec<view::Assignment>> {
    let pool = &ctx.state.pool;
    let viewer = ctx.user.as_ref().map(|u| u.id);
    let content = if kind == crate::webui::pages::repos::issues::Kind::Issue {
        Content::Issue(id)
    } else {
        Content::PullRequest(id)
    };
    let mut item_ids = HashMap::new();
    let mut offset = 0;
    loop {
        let (count, items) =
            projects::content_items(pool, viewer, &content, true, offset, 100, true).await?;
        for item in items {
            item_ids.insert(item.project_id, item.id);
        }
        offset += 100;
        if offset >= count {
            break;
        }
    }
    let mut assigned = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for filter in [
        projects::ListFilter {
            content: Some((matches!(content, Content::Issue(_)), id)),
            ..Default::default()
        },
        projects::ListFilter {
            repo: Some(repo),
            ..Default::default()
        },
    ] {
        let mut offset = 0;
        loop {
            let (count, rows) = projects::list(pool, viewer, &filter, offset, 100).await?;
            for project in rows {
                if seen.insert(project.id) {
                    assigned.push(project);
                }
            }
            offset += 100;
            if offset >= count {
                break;
            }
        }
    }
    let mut result = Vec::new();
    for project in assigned {
        let owner = if project.owner_kind == "org" {
            Owner::Organization(project.owner_id)
        } else {
            Owner::User(project.owner_id)
        };
        let can_edit = match viewer {
            Some(id) => projects::can_write(pool, id, owner).await?,
            None => false,
        };
        let item_id = item_ids.get(&project.id).copied();
        if item_id.is_none() && !can_edit {
            continue;
        }
        let name = match owner {
            Owner::User(id) => {
                db::users::accounts::get_user_by_id(pool, id)
                    .await?
                    .ok_or(projects::Refusal::Missing)?
                    .username
            }
            Owner::Organization(id) => {
                db::organizations::profiles::get_public_organization_by_id(pool, id)
                    .await?
                    .ok_or(projects::Refusal::Missing)?
                    .name
            }
        };
        let path = format!(
            "/{}/{}/projects/{}",
            if project.owner_kind == "org" {
                "orgs"
            } else {
                "users"
            },
            crate::webui::encode_component(&name),
            project.number
        );
        result.push(view::Assignment {
            path,
            title: project.title,
            item_id,
            can_edit,
        });
    }
    Ok(result)
}

fn failure(ctx: &PageContext, error: anyhow::Error) -> Response {
    match error.downcast_ref::<projects::Refusal>() {
        Some(projects::Refusal::Missing) => ctx.error(StatusCode::NOT_FOUND, "Not Found"),
        Some(projects::Refusal::Denied) => {
            ctx.error(StatusCode::FORBIDDEN, "Project write access is required.")
        }
        Some(projects::Refusal::Invalid) => ctx.error(
            StatusCode::UNPROCESSABLE_ENTITY,
            "Check the project fields and try again.",
        ),
        None => ctx.error(
            StatusCode::SERVICE_UNAVAILABLE,
            "Unable to load or save the project.",
        ),
    }
}

async fn owner(ctx: &PageContext, params: &Params) -> anyhow::Result<(Owner, String, String)> {
    if let Some(name) = params.get("org") {
        let org = db::organizations::profiles::get_public_organization(&ctx.state.pool, name)
            .await?
            .ok_or(projects::Refusal::Missing)?;
        let base = format!(
            "/orgs/{}/projects",
            crate::webui::encode_component(&org.name)
        );
        Ok((Owner::Organization(org.id), org.name, base))
    } else {
        let name = params.get("username").ok_or(projects::Refusal::Missing)?;
        let user = db::users::accounts::get_user_by_username(&ctx.state.pool, name)
            .await?
            .ok_or(projects::Refusal::Missing)?;
        let base = format!(
            "/users/{}/projects",
            crate::webui::encode_component(&user.username)
        );
        Ok((Owner::User(user.id), user.username, base))
    }
}

fn summary(project: projects::Project) -> view::Project {
    view::Project {
        number: project.number,
        title: project.title,
        description: project.description.unwrap_or_default(),
        public: project.public,
        closed: project.closed_at.is_some(),
    }
}

async fn model(ctx: &PageContext, params: &Params) -> anyhow::Result<view::Page> {
    let pool = &ctx.state.pool;
    let (owner, name, base) = owner(ctx, params).await?;
    let viewer = ctx.user.as_ref().map(|u| u.id);
    let can_edit = match viewer {
        Some(id) => projects::can_write(pool, id, owner).await?,
        None => false,
    };
    let mut page = view::Page {
        view_state: view::ViewState {
            location: ctx.location.clone(),
            direction: "asc".into(),
            ..Default::default()
        },
        owner: name,
        base,
        csrf_token: ctx.csrf_token.clone(),
        can_edit,
        projects: vec![],
        project: None,
        fields: vec![],
        settings: ctx.path.ends_with("/settings"),
        items: vec![],
        columns: vec![],
        views: vec![],
        view_number: 1,
        filter: ctx.query.get("q").cloned().unwrap_or_default(),
        message: None,
        previous: None,
        next: None,
    };
    let Some(number) = params.get("project_number") else {
        let number = ctx
            .query
            .get("page")
            .map(|value| value.parse::<i64>())
            .transpose()?
            .unwrap_or(1);
        anyhow::ensure!(number > 0, projects::Refusal::Invalid);
        let offset = (number - 1)
            .checked_mul(50)
            .ok_or(projects::Refusal::Invalid)?;
        let mut filter = projects::ListFilter::search(Some(owner), &page.filter);
        filter.order = projects::ListOrder::Number;
        filter.descending = true;
        let (count, rows) = projects::list(pool, viewer, &filter, offset, 50).await?;
        page.projects = rows.into_iter().map(summary).collect();
        let link = |number: i64| {
            let mut query = url::form_urlencoded::Serializer::new(String::new());
            query
                .append_pair("page", &number.to_string())
                .append_pair("q", &page.filter);
            format!("{}?{}", ctx.path, query.finish())
        };
        page.previous = (number > 1).then(|| link(number - 1));
        page.next = (count.saturating_sub(offset) > 50).then(|| link(number + 1));
        return Ok(page);
    };
    let number = number.parse().map_err(|_| projects::Refusal::Missing)?;
    let project = projects::get(pool, viewer, owner, number).await?;
    let fields = projects::fields(pool, viewer, project.id).await?;
    let views = projects::views(pool, viewer, project.id).await?;
    page.view_number = params
        .get("view_number")
        .map(|s| s.parse())
        .transpose()
        .map_err(|_| projects::Refusal::Missing)?
        .unwrap_or(1);
    let view = views
        .iter()
        .find(|v| v.number == page.view_number)
        .ok_or(projects::Refusal::Missing)?;
    let mut config: NewView = serde_json::from_str(&view.configuration)?;
    if let Some(id) = ctx.query.get("sort").filter(|s| !s.is_empty()) {
        let id = id.parse().map_err(|_| projects::Refusal::Invalid)?;
        anyhow::ensure!(
            fields.iter().any(|f| f.id == id),
            projects::Refusal::Invalid
        );
        let direction = ctx
            .query
            .get("direction")
            .map(String::as_str)
            .unwrap_or("asc");
        anyhow::ensure!(
            matches!(direction, "asc" | "desc"),
            projects::Refusal::Invalid
        );
        config.sort_by = vec![(id, direction.into())];
    }
    page.view_state.visible_fields = config.visible_fields.clone();
    if let Some((field, direction)) = config.sort_by.first() {
        page.view_state.sort = field.to_string();
        page.view_state.direction = direction.clone();
    }
    page.views = views
        .iter()
        .map(|v| {
            Ok(view::View {
                number: v.number,
                name: serde_json::from_str::<NewView>(&v.configuration)?.name,
            })
        })
        .collect::<anyhow::Result<_>>()?;
    for field in &fields {
        let config = projects::field_configuration(field)?;
        let options = config[if field.data_type == "iteration" {
            "iterations"
        } else {
            "options"
        }]
        .as_array()
        .map(|options| {
            options
                .iter()
                .map(|o| {
                    (
                        o["id"].as_str().unwrap_or("").to_owned(),
                        o[if field.data_type == "iteration" {
                            "title"
                        } else {
                            "name"
                        }]
                        .as_str()
                        .unwrap_or("")
                        .to_owned(),
                    )
                })
                .collect()
        })
        .unwrap_or_default();
        page.fields.push(view::Field {
            id: field.id,
            name: field.name.clone(),
            data_type: field.data_type.clone(),
            options,
        });
    }
    let items = projects::items(pool, viewer, project.id)
        .await?
        .into_iter()
        .filter(|item| !item.archived)
        .collect();
    let items =
        crate::backend::projects::filter_items(items, &fields, &page.filter, Some(&config))?;
    const ITEMS_PAGE: usize = 50;
    // Item identities, rather than fixed row offsets, delimit byte-sized pages in
    // the current filtered/sorted order. Backward pages are budgeted in reverse.
    let backwards = ctx.query.contains_key("before");
    let cursor = ctx.query.get(if backwards { "before" } else { "after" });
    let position = cursor
        .map(|cursor| -> anyhow::Result<usize> {
            let id: i64 = cursor.parse().map_err(|_| projects::Refusal::Invalid)?;
            items
                .iter()
                .position(|item| item.id == id)
                .ok_or_else(|| projects::Refusal::Missing.into())
        })
        .transpose()?;
    let total = items.len();
    let start = if backwards {
        0
    } else {
        position.map_or(0, |index| index + 1)
    };
    let end = if backwards {
        position.unwrap_or(total)
    } else {
        total
    };
    let link = |direction: &str, id: i64| {
        let mut query: Vec<(String, String)> = ctx
            .query
            .iter()
            .filter(|(key, _)| !matches!(key.as_str(), "page" | "after" | "before"))
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect();
        query.push((direction.into(), id.to_string()));
        query.sort();
        format!(
            "{}?{}",
            ctx.path,
            url::form_urlencoded::Serializer::new(String::new())
                .extend_pairs(query)
                .finish()
        )
    };
    let mut items: Vec<_> = items
        .into_iter()
        .enumerate()
        .skip(start)
        .take(end.saturating_sub(start))
        .collect();
    if backwards {
        items.reverse();
    }
    items.truncate(ITEMS_PAGE);
    let item_ids: Vec<_> = items.iter().map(|(_, item)| item.id).collect();
    let links: HashMap<_, _> = projects::item_links(pool, viewer, project.id, &item_ids)
        .await?
        .into_iter()
        .map(|(id, owner, name, kind, number)| {
            (
                id,
                format!(
                    "/{}/{}/{kind}/{number}",
                    crate::webui::encode_component(&owner),
                    crate::webui::encode_component(&name)
                ),
            )
        })
        .collect();
    let group = if config.layout == "board" {
        config
            .vertical_group_by
            .first()
            .and_then(|id| fields.iter().find(|f| f.id == *id))
    } else {
        None
    };
    if let Some(field) = group {
        let options = projects::field_configuration(field)?;
        page.columns.push(view::Column {
            name: "No value".into(),
            items: vec![],
        });
        if let Some(options) = options["options"].as_array() {
            for option in options {
                page.columns.push(view::Column {
                    name: option["name"].as_str().unwrap_or("").into(),
                    items: vec![],
                });
            }
        }
    }
    page.project = Some(summary(project));
    // Reserve envelope/pager space and count JSON escaping and board duplication.
    let limit = crate::webui::bootstrap::MAX_PAGE_MODEL_BYTES - 256 * 1024;
    let mut bytes = serde_json::to_vec(&page)?.len();
    anyhow::ensure!(bytes < limit, projects::Refusal::Invalid);
    let mut first = None;
    let mut last = None;
    for (index, item) in items {
        // The batched projection rechecks repository access after loading and sorting items.
        let url = if item.repo_id.is_some() {
            Some(
                links
                    .get(&item.id)
                    .cloned()
                    .ok_or(projects::Refusal::Missing)?,
            )
        } else {
            None
        };
        let mut values = Vec::new();
        for field in fields.iter().filter(|f| f.data_type != "title") {
            let value = item.value(field)?;
            values.push((
                field.id,
                if value.is_null() {
                    String::new()
                } else {
                    value
                        .as_str()
                        .map(str::to_owned)
                        .unwrap_or_else(|| value.to_string())
                },
                crate::backend::projects::display_value(field, &value)?,
            ));
        }
        let output = view::Item {
            id: item.id,
            title: item.title.clone(),
            url,
            body: item.draft_body.clone().unwrap_or_default(),
            values,
        };
        let cost = serde_json::to_vec(&output)?
            .len()
            .saturating_add(16)
            .saturating_mul(if group.is_some() { 2 } else { 1 });
        if bytes.saturating_add(cost) > limit {
            anyhow::ensure!(!page.items.is_empty(), projects::Refusal::Invalid);
            break;
        }
        bytes += cost;
        first = Some(first.map_or(index, |first: usize| first.min(index)));
        last = Some(last.map_or(index, |last: usize| last.max(index)));
        if let Some(field) = group {
            let value = item.value(field)?;
            let options = &page
                .fields
                .iter()
                .find(|f| f.id == field.id)
                .ok_or(projects::Refusal::Missing)?
                .options;
            let index = options
                .iter()
                .position(|(id, _)| value.as_str() == Some(id))
                .map_or(0, |i| i + 1);
            page.columns[index].items.push(output.clone());
        }
        page.items.push(output);
    }
    if backwards {
        page.items.reverse();
        for column in &mut page.columns {
            column.items.reverse();
        }
    }
    page.previous = first
        .filter(|first| *first > 0)
        .and_then(|_| page.items.first())
        .map(|item| link("before", item.id));
    page.next = last
        .filter(|last| *last + 1 < total)
        .and_then(|_| page.items.last())
        .map(|item| link("after", item.id));
    Ok(page)
}

async fn show(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    uri: Uri,
    Path(params): Path<Params>,
) -> Response {
    let ctx = match PageContext::load(state, headers, &uri).await {
        Ok(ctx) => ctx,
        Err(response) => return response,
    };
    match model(&ctx, &params).await {
        Ok(model) => ctx.page_model("Projects", Page::Projects(model)),
        Err(error) => failure(&ctx, error),
    }
}

async fn apply(ctx: &PageContext, params: &Params, form: &Fields) -> anyhow::Result<String> {
    let pool = &ctx.state.pool;
    let actor = ctx.user.as_ref().ok_or(projects::Refusal::Denied)?.id;
    let session = crate::backend::api::http::session::extract_session_id(&ctx.headers)
        .ok_or(projects::Refusal::Denied)?;
    let credential = db::users::accounts::AccountCredential::Session(&session);
    let (owner, _, base) = owner(ctx, params).await?;
    let Some(number) = params.get("project_number") else {
        let project = projects::create(
            pool,
            actor,
            &credential,
            owner,
            field(form, "title"),
            None,
            form.contains_key("public"),
        )
        .await?;
        return Ok(format!("{base}/{}", project.number));
    };
    let project = projects::get(
        pool,
        Some(actor),
        owner,
        number.parse().map_err(|_| projects::Refusal::Missing)?,
    )
    .await?;
    let path = format!("{base}/{}", project.number);
    let integer = |name: &str| {
        field(form, name)
            .parse::<i64>()
            .map_err(|_| projects::Refusal::Invalid)
    };
    match field(form, "action") {
        "save" => {
            projects::save(
                pool,
                actor,
                &credential,
                project.id,
                &projects::Settings {
                    title: Some(field(form, "title").into()),
                    description: Some(field(form, "description").into()),
                    public: Some(form.contains_key("public")),
                    closed: Some(form.contains_key("closed")),
                },
            )
            .await?;
            return Ok(format!("{path}/settings"));
        }
        "delete" => {
            projects::delete(pool, actor, &credential, project.id).await?;
            return Ok(base);
        }
        "add-field" => {
            let kind = field(form, "data_type");
            let single_select_options = (kind == "single_select").then(|| {
                field(form, "options")
                    .lines()
                    .filter(|s| !s.trim().is_empty())
                    .map(|s| projects::SelectOption {
                        name: s.trim().into(),
                        color: None,
                        description: None,
                    })
                    .collect()
            });
            let iteration_configuration = if kind == "iteration" {
                Some(projects::IterationConfiguration {
                    start_date: field(form, "start_date")
                        .parse()
                        .map_err(|_| projects::Refusal::Invalid)?,
                    duration: field(form, "duration")
                        .parse()
                        .map_err(|_| projects::Refusal::Invalid)?,
                    iterations: vec![projects::Iteration {
                        title: "Iteration 1".into(),
                        start_date: field(form, "start_date")
                            .parse()
                            .map_err(|_| projects::Refusal::Invalid)?,
                        duration: field(form, "duration")
                            .parse()
                            .map_err(|_| projects::Refusal::Invalid)?,
                    }],
                })
            } else {
                None
            };
            projects::add_field(
                pool,
                actor,
                &credential,
                project.id,
                &projects::NewField {
                    name: field(form, "name").into(),
                    data_type: kind.into(),
                    single_select_options,
                    iteration_configuration,
                },
            )
            .await?;
            return Ok(format!("{path}/settings"));
        }
        "delete-field" => {
            projects::delete_field(pool, actor, &credential, project.id, integer("field_id")?)
                .await?;
            return Ok(format!("{path}/settings"));
        }
        "draft" => {
            projects::add_item(
                pool,
                actor,
                &credential,
                project.id,
                &Content::Draft {
                    title: field(form, "title").into(),
                    body: Some(field(form, "body").into()),
                },
            )
            .await?;
        }
        "add-item" | "link-repository" => {
            let repo = db::repositories::records::get_repository(
                pool,
                field(form, "owner"),
                field(form, "repo"),
            )
            .await?
            .ok_or(projects::Refusal::Missing)?;
            if field(form, "action") == "link-repository" {
                projects::link_repository(
                    pool,
                    actor,
                    &credential,
                    project.id,
                    repo.id,
                    !form.contains_key("unlink"),
                )
                .await?;
                return Ok(format!("{path}/settings"));
            }
            anyhow::ensure!(
                crate::backend::repositories::access::can_read(pool, ctx.user.as_ref(), &repo)
                    .await?,
                projects::Refusal::Missing
            );
            let number = field(form, "number")
                .parse()
                .map_err(|_| projects::Refusal::Invalid)?;
            let content = match field(form, "kind") {
                "Issue" => Content::Issue(
                    db::issues::get_issue(pool, repo.id, number)
                        .await?
                        .ok_or(projects::Refusal::Missing)?
                        .id,
                ),
                "PullRequest" => Content::PullRequest(
                    db::pull_requests::records::get_pull_request(pool, repo.id, number)
                        .await?
                        .ok_or(projects::Refusal::Missing)?
                        .id,
                ),
                _ => return Err(projects::Refusal::Invalid.into()),
            };
            projects::add_item(pool, actor, &credential, project.id, &content).await?;
        }
        "update-item" => {
            let fields = projects::fields(pool, Some(actor), project.id).await?;
            let mut updates = Vec::new();
            for f in fields {
                let Some(value) = form.get(&format!("value_{}", f.id)) else {
                    continue;
                };
                let original: String = form
                    .get(&format!("was_{}", f.id))
                    .and_then(|value| serde_json::from_str(value).ok())
                    .ok_or(projects::Refusal::Invalid)?;
                let value = if f.data_type == "text" {
                    crate::backend::repositories::files::native_text(value.clone(), &original)
                } else {
                    value.clone()
                };
                // An unchanged field keeps whatever another editor saved meanwhile.
                if value == original {
                    continue;
                }
                let value = if value.is_empty() {
                    Value::Null
                } else if f.data_type == "number" {
                    serde_json::from_str(&value).map_err(|_| projects::Refusal::Invalid)?
                } else {
                    json!(value)
                };
                updates.push(projects::FieldUpdate { id: f.id, value });
            }
            projects::update_item(
                pool,
                actor,
                &credential,
                project.id,
                integer("item_id")?,
                &updates,
            )
            .await?;
        }
        "delete-item" => {
            projects::delete_item(pool, actor, &credential, project.id, integer("item_id")?).await?
        }
        "view" => {
            let group = field(form, "group");
            let view = projects::create_view(
                pool,
                actor,
                &credential,
                project.id,
                &NewView {
                    name: field(form, "name").into(),
                    layout: field(form, "layout").into(),
                    filter: Some(field(form, "filter").into()),
                    visible_fields: vec![],
                    sort_by: vec![],
                    group_by: vec![],
                    vertical_group_by: if group.is_empty() {
                        vec![]
                    } else {
                        vec![integer("group")?]
                    },
                },
            )
            .await?;
            return Ok(format!("{path}/views/{}", view.number));
        }
        _ => return Err(projects::Refusal::Invalid.into()),
    }
    Ok(path)
}

async fn mutate(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    uri: Uri,
    Path(params): Path<Params>,
    Form(form): Form<Fields>,
) -> Response {
    let ctx = match PageContext::load(state, headers, &uri).await {
        Ok(ctx) => ctx,
        Err(response) => return response,
    };
    if let Err(response) = mutation_guard(&ctx, &form, false) {
        return response;
    }
    match apply(&ctx, &params, &form).await {
        Ok(path) => ctx.redirect(
            form.get("return_to")
                .filter(|path| crate::backend::api::http::origin::safe_redirect_path(path))
                .map(String::as_str)
                .unwrap_or(&path),
        ),
        Err(error) => failure(&ctx, error),
    }
}
