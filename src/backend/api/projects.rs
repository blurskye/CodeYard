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
    let mut assigned = projects::item_projects(pool, viewer, &content).await?;
    for project in projects::repository_projects(pool, viewer, repo).await? {
        if !assigned.iter().any(|p| p.id == project.id) {
            assigned.push(project);
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
        let item_id = projects::items(pool, viewer, project.id)
            .await?
            .into_iter()
            .find(|i| match content {
                Content::Issue(id) => i.issue_id == Some(id),
                Content::PullRequest(id) => i.pull_request_id == Some(id),
                Content::Draft { .. } => false,
            })
            .map(|i| i.id);
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
        page.projects = projects::list(pool, viewer, owner)
            .await?
            .into_iter()
            .map(summary)
            .collect();
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
    let items = projects::items(pool, viewer, project.id).await?;
    let items =
        crate::backend::projects::filter_items(items, &fields, &page.filter, Some(&config))?;
    // One page of the filtered, sorted items keeps large drafts within the page model.
    const ITEMS_PAGE: usize = 50;
    let page_number = ctx
        .query
        .get("page")
        .and_then(|page| page.parse::<usize>().ok())
        .filter(|page| (1..=10000).contains(page))
        .unwrap_or(1);
    let link = |page_number: usize| {
        let mut query: Vec<(String, String)> = ctx
            .query
            .iter()
            .filter(|(key, _)| key.as_str() != "page")
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect();
        query.push(("page".into(), page_number.to_string()));
        query.sort();
        format!(
            "{}?{}",
            ctx.path,
            url::form_urlencoded::Serializer::new(String::new())
                .extend_pairs(query)
                .finish()
        )
    };
    page.previous = (page_number > 1).then(|| link(page_number - 1));
    page.next = (items.len() > page_number * ITEMS_PAGE).then(|| link(page_number + 1));
    let items: Vec<_> = items
        .into_iter()
        .skip((page_number - 1) * ITEMS_PAGE)
        .take(ITEMS_PAGE)
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
    for item in items {
        let url = if let Some(repo) = item.repo_id {
            let owner = db::repositories::records::repository_owner_name(pool, repo)
                .await?
                .ok_or(projects::Refusal::Missing)?;
            let repo = db::repositories::records::get_repository_by_id(pool, repo)
                .await?
                .ok_or(projects::Refusal::Missing)?;
            // Recheck before projecting content links after the item query's snapshot ends.
            anyhow::ensure!(
                crate::backend::repositories::access::can_read(pool, ctx.user.as_ref(), &repo)
                    .await?,
                projects::Refusal::Missing
            );
            let number = if let Some(id) = item.issue_id {
                db::issues::get_issue_by_id(pool, id)
                    .await?
                    .ok_or(projects::Refusal::Missing)?
                    .number
            } else {
                db::pull_requests::records::get_pull_request_by_id(
                    pool,
                    item.pull_request_id.ok_or(projects::Refusal::Missing)?,
                )
                .await?
                .ok_or(projects::Refusal::Missing)?
                .number
            };
            Some(format!(
                "/{}/{}/{}/{}",
                crate::webui::encode_component(&owner),
                crate::webui::encode_component(&repo.name),
                if item.issue_id.is_some() {
                    "issues"
                } else {
                    "pulls"
                },
                number
            ))
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
    page.project = Some(summary(project));
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
    let (owner, _, base) = owner(ctx, params).await?;
    let Some(number) = params.get("project_number") else {
        let project = projects::create(
            pool,
            actor,
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
            projects::delete(pool, actor, project.id).await?;
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
            projects::delete_field(pool, actor, project.id, integer("field_id")?).await?;
            return Ok(format!("{path}/settings"));
        }
        "draft" => {
            projects::add_item(
                pool,
                actor,
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
            projects::add_item(pool, actor, project.id, &content).await?;
        }
        "update-item" => {
            let fields = projects::fields(pool, Some(actor), project.id).await?;
            let mut updates = Vec::new();
            for f in fields {
                let Some(value) = form.get(&format!("value_{}", f.id)) else {
                    continue;
                };
                let original: String = form.get(&format!("was_{}", f.id))
                    .and_then(|value| serde_json::from_str(value).ok()).ok_or(projects::Refusal::Invalid)?;
                let value = if f.data_type == "text" {
                    crate::backend::repositories::files::native_text(value.clone(), &original)
                } else { value.clone() };
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
            projects::update_item(pool, actor, project.id, integer("item_id")?, &updates).await?;
        }
        "delete-item" => {
            projects::delete_item(pool, actor, project.id, integer("item_id")?).await?
        }
        "view" => {
            let group = field(form, "group");
            let view = projects::create_view(
                pool,
                actor,
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
