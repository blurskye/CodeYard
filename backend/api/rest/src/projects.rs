//! Project pages and plain forms use the same project operations as REST.
use crate::user::responses::mutation_guard;
use axum::extract::Form;
use axum::extract::Path;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::http::StatusCode;
use axum::http::Uri;
use axum::response::Response;
use axum::routing::get;
use axum::Router;
use codeyard_api_http::context::PageContext;
use codeyard_api_http::shared::user::responses::field;
use codeyard_api_http::shared::user::responses::Fields;
use codeyard_app::app::AppState;
use codeyard_db_repos::projects::Content;
use codeyard_db_repos::projects::NewView;
use codeyard_webui::app::Page;
use codeyard_webui::pages::projects as view;
use serde_json::json;
use serde_json::Value;
use std::collections::HashMap;
use std::sync::Arc;

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

fn failure(ctx: &PageContext, error: anyhow::Error) -> Response {
    match error.downcast_ref::<codeyard_model::project_query::Refusal>() {
        Some(codeyard_model::project_query::Refusal::Missing) => {
            ctx.error(StatusCode::NOT_FOUND, "Not Found")
        }
        Some(codeyard_model::project_query::Refusal::Denied) => {
            ctx.error(StatusCode::FORBIDDEN, "Project write access is required.")
        }
        Some(codeyard_model::project_query::Refusal::Invalid) => ctx.error(
            StatusCode::UNPROCESSABLE_ENTITY,
            "Check the project fields and try again.",
        ),
        None => ctx.error(
            StatusCode::SERVICE_UNAVAILABLE,
            "Unable to load or save the project.",
        ),
    }
}

fn summary(project: codeyard_db_repos::projects::Project) -> view::Project {
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
        Some(id) => codeyard_db_repos::projects::can_write(pool, id, owner).await?,
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
        anyhow::ensure!(number > 0, codeyard_model::project_query::Refusal::Invalid);
        let offset = (number - 1)
            .checked_mul(50)
            .ok_or(codeyard_model::project_query::Refusal::Invalid)?;
        let mut filter = codeyard_db_repos::projects::ListFilter::search(Some(owner), &page.filter);
        filter.order = codeyard_db_repos::projects::ListOrder::Number;
        filter.descending = true;
        let (count, rows) =
            codeyard_db_repos::projects::list(pool, viewer, &filter, offset, 50).await?;
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
    let number = number
        .parse()
        .map_err(|_| codeyard_model::project_query::Refusal::Missing)?;
    let project = codeyard_db_repos::projects::get(pool, viewer, owner, number).await?;
    let fields = codeyard_db_repos::projects::fields(pool, viewer, project.id).await?;
    let views = codeyard_db_repos::projects::views(pool, viewer, project.id).await?;
    page.view_number = params
        .get("view_number")
        .map(|s| s.parse())
        .transpose()
        .map_err(|_| codeyard_model::project_query::Refusal::Missing)?
        .unwrap_or(1);
    let view = views
        .iter()
        .find(|v| v.number == page.view_number)
        .ok_or(codeyard_model::project_query::Refusal::Missing)?;
    let mut config: NewView = serde_json::from_str(&view.configuration)?;
    if let Some(id) = ctx.query.get("sort").filter(|s| !s.is_empty()) {
        let id = id
            .parse()
            .map_err(|_| codeyard_model::project_query::Refusal::Invalid)?;
        anyhow::ensure!(
            fields.iter().any(|f| f.id == id),
            codeyard_model::project_query::Refusal::Invalid
        );
        let direction = ctx
            .query
            .get("direction")
            .map(String::as_str)
            .unwrap_or("asc");
        anyhow::ensure!(
            matches!(direction, "asc" | "desc"),
            codeyard_model::project_query::Refusal::Invalid
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
        let config = codeyard_db_repos::projects::field_configuration(field)?;
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
    let items = codeyard_db_repos::projects::items(pool, viewer, project.id)
        .await?
        .into_iter()
        .filter(|item| !item.archived)
        .collect();
    let items = codeyard_app::projects::filter_items(items, &fields, &page.filter, Some(&config))?;
    const ITEMS_PAGE: usize = 50;
    // Item identities, rather than fixed row offsets, delimit byte-sized pages in
    // the current filtered/sorted order. Backward pages are budgeted in reverse.
    let backwards = ctx.query.contains_key("before");
    let cursor = ctx.query.get(if backwards { "before" } else { "after" });
    let position = cursor
        .map(|cursor| -> anyhow::Result<usize> {
            let id: i64 = cursor
                .parse()
                .map_err(|_| codeyard_model::project_query::Refusal::Invalid)?;
            items
                .iter()
                .position(|item| item.id == id)
                .ok_or_else(|| codeyard_model::project_query::Refusal::Missing.into())
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
    let links: HashMap<_, _> =
        codeyard_db_repos::projects::item_links(pool, viewer, project.id, &item_ids)
            .await?
            .into_iter()
            .map(|(id, owner, name, kind, number)| {
                (
                    id,
                    format!(
                        "/{}/{}/{kind}/{number}",
                        codeyard_webui::encode_component(&owner),
                        codeyard_webui::encode_component(&name)
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
        let options = codeyard_db_repos::projects::field_configuration(field)?;
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
    let limit = codeyard_webui::bootstrap::MAX_PAGE_MODEL_BYTES - 256 * 1024;
    let mut bytes = serde_json::to_vec(&page)?.len();
    anyhow::ensure!(
        bytes < limit,
        codeyard_model::project_query::Refusal::Invalid
    );
    let mut first = None;
    let mut last = None;
    for (index, item) in items {
        // The batched projection rechecks repository access after loading and sorting items.
        let url = if item.repo_id.is_some() {
            Some(
                links
                    .get(&item.id)
                    .cloned()
                    .ok_or(codeyard_model::project_query::Refusal::Missing)?,
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
                codeyard_app::projects::display_value(field, &value)?,
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
            anyhow::ensure!(
                !page.items.is_empty(),
                codeyard_model::project_query::Refusal::Invalid
            );
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
                .ok_or(codeyard_model::project_query::Refusal::Missing)?
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
    let actor = ctx
        .user
        .as_ref()
        .ok_or(codeyard_model::project_query::Refusal::Denied)?
        .id;
    let session = codeyard_api_http::session::extract_session_id(&ctx.headers)
        .ok_or(codeyard_model::project_query::Refusal::Denied)?;
    let credential =
        codeyard_db_core::queries::users::accounts::AccountCredential::Session(&session);
    let (owner, _, base) = owner(ctx, params).await?;
    let Some(number) = params.get("project_number") else {
        let project = codeyard_db_repos::projects::create(
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
    let project = codeyard_db_repos::projects::get(
        pool,
        Some(actor),
        owner,
        number
            .parse()
            .map_err(|_| codeyard_model::project_query::Refusal::Missing)?,
    )
    .await?;
    let path = format!("{base}/{}", project.number);
    let integer = |name: &str| {
        field(form, name)
            .parse::<i64>()
            .map_err(|_| codeyard_model::project_query::Refusal::Invalid)
    };
    match field(form, "action") {
        "save" => {
            codeyard_db_repos::projects::save(
                pool,
                actor,
                &credential,
                project.id,
                &codeyard_db_repos::projects::Settings {
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
            codeyard_db_repos::projects::delete(pool, actor, &credential, project.id).await?;
            return Ok(base);
        }
        "add-field" => {
            let kind = field(form, "data_type");
            let single_select_options = (kind == "single_select").then(|| {
                field(form, "options")
                    .lines()
                    .filter(|s| !s.trim().is_empty())
                    .map(|s| codeyard_db_repos::projects::SelectOption {
                        name: s.trim().into(),
                        color: None,
                        description: None,
                    })
                    .collect()
            });
            let iteration_configuration = if kind == "iteration" {
                Some(codeyard_db_repos::projects::IterationConfiguration {
                    start_date: field(form, "start_date")
                        .parse()
                        .map_err(|_| codeyard_model::project_query::Refusal::Invalid)?,
                    duration: field(form, "duration")
                        .parse()
                        .map_err(|_| codeyard_model::project_query::Refusal::Invalid)?,
                    iterations: vec![codeyard_db_repos::projects::Iteration {
                        title: "Iteration 1".into(),
                        start_date: field(form, "start_date")
                            .parse()
                            .map_err(|_| codeyard_model::project_query::Refusal::Invalid)?,
                        duration: field(form, "duration")
                            .parse()
                            .map_err(|_| codeyard_model::project_query::Refusal::Invalid)?,
                    }],
                })
            } else {
                None
            };
            codeyard_db_repos::projects::add_field(
                pool,
                actor,
                &credential,
                project.id,
                &codeyard_db_repos::projects::NewField {
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
            codeyard_db_repos::projects::delete_field(
                pool,
                actor,
                &credential,
                project.id,
                integer("field_id")?,
            )
            .await?;
            return Ok(format!("{path}/settings"));
        }
        "draft" => {
            codeyard_db_repos::projects::add_item(
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
            let repo = codeyard_db_repos::repositories::records::get_repository(
                pool,
                field(form, "owner"),
                field(form, "repo"),
            )
            .await?
            .ok_or(codeyard_model::project_query::Refusal::Missing)?;
            if field(form, "action") == "link-repository" {
                codeyard_db_repos::projects::link_repository(
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
                codeyard_identity::repository_access::can_read(pool, ctx.user.as_ref(), &repo)
                    .await?,
                codeyard_model::project_query::Refusal::Missing
            );
            let number = field(form, "number")
                .parse()
                .map_err(|_| codeyard_model::project_query::Refusal::Invalid)?;
            let content = match field(form, "kind") {
                "Issue" => Content::Issue(
                    codeyard_db_repos::issues::get_issue(pool, repo.id, number)
                        .await?
                        .ok_or(codeyard_model::project_query::Refusal::Missing)?
                        .id,
                ),
                "PullRequest" => Content::PullRequest(
                    codeyard_db_repos::pull_requests::records::get_pull_request(
                        pool, repo.id, number,
                    )
                    .await?
                    .ok_or(codeyard_model::project_query::Refusal::Missing)?
                    .id,
                ),
                _ => return Err(codeyard_model::project_query::Refusal::Invalid.into()),
            };
            codeyard_db_repos::projects::add_item(pool, actor, &credential, project.id, &content)
                .await?;
        }
        "update-item" => {
            let fields = codeyard_db_repos::projects::fields(pool, Some(actor), project.id).await?;
            let mut updates = Vec::new();
            for f in fields {
                let Some(value) = form.get(&format!("value_{}", f.id)) else {
                    continue;
                };
                let original: String = form
                    .get(&format!("was_{}", f.id))
                    .and_then(|value| serde_json::from_str(value).ok())
                    .ok_or(codeyard_model::project_query::Refusal::Invalid)?;
                let value = if f.data_type == "text" {
                    codeyard_app::repositories::files::native_text(value.clone(), &original)
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
                    serde_json::from_str(&value)
                        .map_err(|_| codeyard_model::project_query::Refusal::Invalid)?
                } else {
                    json!(value)
                };
                updates.push(codeyard_db_repos::projects::FieldUpdate { id: f.id, value });
            }
            codeyard_db_repos::projects::update_item(
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
            codeyard_db_repos::projects::delete_item(
                pool,
                actor,
                &credential,
                project.id,
                integer("item_id")?,
            )
            .await?
        }
        "view" => {
            let group = field(form, "group");
            let view = codeyard_db_repos::projects::create_view(
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
        _ => return Err(codeyard_model::project_query::Refusal::Invalid.into()),
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
                .filter(|path| codeyard_api_http::origin::safe_redirect_path(path))
                .map(String::as_str)
                .unwrap_or(&path),
        ),
        Err(error) => failure(&ctx, error),
    }
}

use codeyard_api_http::shared::projects::Params;

use codeyard_api_http::shared::projects::owner;
