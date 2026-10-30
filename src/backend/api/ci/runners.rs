//! Native runner settings and credential redemption; the REST adapter shares the same DAOs.
use crate::backend::{
    api::http::context::PageContext,
    app::AppState,
    ci::runner_credentials as credentials,
    db::{
        self,
        runners::{self as native, GroupInput, Labels, Refusal, Registration, Scope, Selection},
    },
};
use crate::webui::{app::Page, pages::runners as view};
use axum::{
    body::Body,
    extract::{DefaultBodyLimit, Path, State},
    http::{HeaderMap, StatusCode, Uri},
    response::{IntoResponse, Response},
    routing::{get, post},
    Form, Json, Router,
};
use base64::{engine::general_purpose::STANDARD, Engine};
use serde::Deserialize;
use serde_json::{json, Value};
use std::{collections::HashMap, sync::Arc};

type Fields = HashMap<String, String>;
pub fn router() -> Router<Arc<AppState>> {
    Router::new()
        .route("/settings/actions/runners", get(show).post(save))
        .route("/api/actions/runners/:id/remove", post(remove))
        .route("/api/actions/runners/download", get(download))
        .layer(DefaultBodyLimit::max(512 * 1024))
}
async fn scope(ctx: &PageContext) -> Result<Scope, Response> {
    if ctx.query.contains_key("org") && ctx.query.contains_key("repo") {
        return Err(ctx.error(
            StatusCode::BAD_REQUEST,
            "Choose one organization or repository.",
        ));
    }
    let org = ctx
        .query
        .get("org")
        .and_then(|v| v.parse::<i64>().ok())
        .filter(|id| *id > 0);
    let repo = ctx
        .query
        .get("repo")
        .and_then(|v| v.parse::<i64>().ok())
        .filter(|id| *id > 0);
    match (org, repo) {
        (Some(id), None) => Ok(Scope::Organization(id)),
        (None, Some(id)) => {
            let repository =
                match db::repositories::records::get_repository_by_id(&ctx.state.pool, id).await {
                    Ok(Some(repo)) => repo,
                    Ok(None) => {
                        return Err(ctx.error(StatusCode::NOT_FOUND, "Repository not found."))
                    }
                    Err(_) => {
                        return Err(ctx.error(
                            StatusCode::SERVICE_UNAVAILABLE,
                            "Repository settings unavailable.",
                        ))
                    }
                };
            match crate::backend::repositories::access::can_read(
                &ctx.state.pool,
                ctx.user.as_ref(),
                &repository,
            )
            .await
            {
                Ok(true) => Ok(Scope::Repository(id)),
                Ok(false) => Err(ctx.error(StatusCode::NOT_FOUND, "Repository not found.")),
                Err(_) => Err(ctx.error(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "Repository settings unavailable.",
                )),
            }
        }
        _ => Err(ctx.error(
            StatusCode::BAD_REQUEST,
            "Choose one organization or repository.",
        )),
    }
}
fn refusal_details(refusal: &Refusal) -> (StatusCode, String) {
    match refusal {
        Refusal::Denied => (StatusCode::FORBIDDEN, "Runner administration permission required".to_owned()),
        Refusal::Missing => (StatusCode::NOT_FOUND, "Runner resource not found".to_owned()),
        Refusal::Invalid(field) => (StatusCode::UNPROCESSABLE_ENTITY, format!("Check {field}.")),
        Refusal::Conflict(message) => (StatusCode::CONFLICT, (*message).to_owned()),
    }
}
fn failure(ctx: &PageContext, error: anyhow::Error) -> Response {
    let (status, message) = error.downcast_ref::<Refusal>().map(refusal_details).unwrap_or_else(|| (
        StatusCode::SERVICE_UNAVAILABLE, "Runner settings are unavailable.".to_owned(),
    ));
    ctx.error(status, &message)
}
async fn show(State(s): State<Arc<AppState>>, headers: HeaderMap, uri: Uri) -> Response {
    let ctx = match PageContext::load(s, headers, &uri).await {
        Ok(ctx) => ctx,
        Err(response) => return response,
    };
    render(&ctx, StatusCode::OK, None, Fields::new()).await
}
async fn render(
    ctx: &PageContext,
    status: StatusCode,
    message: Option<String>,
    draft: Fields,
) -> Response {
    let actor = match ctx.require_user() {
        Ok(actor) => actor.id,
        Err(response) => return response,
    };
    let scope = match scope(ctx).await {
        Ok(scope) => scope,
        Err(response) => return response,
    };
    let group_window = match page_window(ctx, "group_page", 10) { Ok(page) => page, Err(response) => return response };
    let runner_window = match page_window(ctx, "runner_page", 25) { Ok(page) => page, Err(response) => return response };
    let rows = match native::list_runners(&ctx.state.pool, actor, scope, &native::RunnerQuery { page: runner_window, ..Default::default() }).await {
        Ok(rows) => rows,
        Err(error) => return failure(ctx, error),
    };
    let (title, back, groups) = match scope {
        Scope::Organization(org) => {
            let org = match db::organizations::get_public_organization_by_id(&ctx.state.pool, org)
                .await
            {
                Ok(Some(org)) => org,
                _ => return ctx.error(StatusCode::NOT_FOUND, "Organization not found."),
            };
            let groups = match native::list_groups(&ctx.state.pool, actor, org.id, &native::GroupQuery { page: group_window, ..Default::default() }).await {
                Ok(rows) => rows,
                Err(error) => return failure(ctx, error),
            };
            (
                format!("{} runners", org.name),
                format!("/settings/organizations/{}", org.id),
                groups,
            )
        }
        Scope::Repository(repo) => {
            let repo = match db::repositories::records::get_repository_by_id(&ctx.state.pool, repo)
                .await
            {
                Ok(Some(repo)) => repo,
                _ => return ctx.error(StatusCode::NOT_FOUND, "Repository not found."),
            };
            let owner = match db::ci::ci_repository_owner(&ctx.state.pool, repo.id).await {
                Ok(Some(owner)) => owner,
                _ => {
                    return ctx.error(
                        StatusCode::SERVICE_UNAVAILABLE,
                        "Repository settings unavailable.",
                    )
                }
            };
            (
                format!("{owner}/{} runners", repo.name),
                format!(
                    "/{}/{}/settings",
                    crate::webui::encode_component(&owner),
                    crate::webui::encode_component(&repo.name)
                ),
                native::Listing { total: 0, rows: Vec::new() },
            )
        }
    };
    let group_navigation = navigation(scope, group_window, runner_window, groups.total, true);
    let runner_navigation = navigation(scope, group_window, runner_window, rows.total, false);
    let groups = groups.rows
        .into_iter()
        .map(|g| -> anyhow::Result<_> {
            Ok(view::Group {
                id: g.id,
                name: g.name,
                visibility: g.visibility,
                default: g.is_default,
                public: g.allows_public_repositories,
                restricted: g.restricted_to_workflows,
                workflows: serde_json::from_str::<Vec<String>>(&g.selected_workflows)?.join("\n"),
                repositories: g
                    .repositories
                    .iter()
                    .map(i64::to_string)
                    .collect::<Vec<_>>()
                    .join(", "),
            })
        })
        .collect::<anyhow::Result<Vec<_>>>();
    let rows = rows.rows
        .into_iter()
        .map(|r| -> anyhow::Result<_> {
            Ok(view::Runner {
                id: r.id,
                name: r.name,
                online: r.online,
                busy: r.busy,
                group: r.group_id,
                ephemeral: r.ephemeral,
                labels: serde_json::from_str::<Vec<String>>(&r.labels)?.join(", "),
            })
        })
        .collect::<anyhow::Result<Vec<_>>>();
    let (groups, runners) = match (groups, rows) {
        (Ok(groups), Ok(rows)) => (groups, rows),
        _ => {
            return ctx.error(
                StatusCode::SERVICE_UNAVAILABLE,
                "Runner settings are unavailable.",
            )
        }
    };
    ctx.page_model_status(
        status,
        &title,
        Page::RunnerSettings(view::Page {
            csrf_token: ctx.csrf_token.clone(),
            title: title.clone(),
            back,
            action: page_destination(scope, group_window, runner_window),
            organization: matches!(scope, Scope::Organization(_)),
            groups,
            runners,
            group_navigation,
            runner_navigation,
            message,
            draft,
        }),
    )
}
fn page_window(ctx: &PageContext, key: &str, limit: i64) -> Result<native::Window, Response> {
    let page = ctx.query.get(key).map_or(Some(1), |value| value.parse::<i64>().ok());
    let offset = page.filter(|page| *page > 0).and_then(|page| (page - 1).checked_mul(limit)).filter(|offset| *offset <= 1_000_000)
        .ok_or_else(|| ctx.error(StatusCode::BAD_REQUEST, "Invalid runner settings page."))?;
    Ok(native::Window { offset, limit })
}
fn page_destination(scope: Scope, groups: native::Window, runners: native::Window) -> String {
    format!("{}&group_page={}&runner_page={}", destination(scope), groups.offset / groups.limit + 1, runners.offset / runners.limit + 1)
}
fn navigation(scope: Scope, groups: native::Window, runners: native::Window, total: i64, group: bool) -> Vec<(String, String)> {
    let current = if group { groups } else { runners };
    let mut links = Vec::new();
    for (label, offset) in [("Previous", (current.offset > 0).then_some(current.offset - current.limit)),
        ("Next", (current.offset + current.limit < total).then_some(current.offset + current.limit))] {
        if let Some(offset) = offset {
            let page = native::Window { offset, ..current };
            links.push((label.into(), if group { page_destination(scope, page, runners) } else { page_destination(scope, groups, page) }));
        }
    }
    links
}
fn destination(scope: Scope) -> String {
    match scope {
        Scope::Organization(id) => format!("/settings/actions/runners?org={id}"),
        Scope::Repository(id) => format!("/settings/actions/runners?repo={id}"),
    }
}
fn field<'a>(form: &'a Fields, key: &str) -> &'a str {
    form.get(key).map(String::as_str).unwrap_or("")
}
fn id(form: &Fields, key: &str) -> anyhow::Result<i64> {
    field(form, key)
        .parse::<i64>()
        .ok()
        .filter(|id| *id > 0)
        .ok_or_else(|| Refusal::Invalid("id").into())
}
fn ids(value: &str) -> anyhow::Result<Vec<i64>> {
    if value.trim().is_empty() {
        return Ok(Vec::new());
    }
    value
        .split(',')
        .map(|s| {
            s.trim()
                .parse::<i64>()
                .map_err(|_| Refusal::Invalid("repositories").into())
        })
        .collect()
}
fn labels(value: &str) -> Vec<String> {
    value
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .collect()
}
async fn save(
    State(s): State<Arc<AppState>>,
    headers: HeaderMap,
    uri: Uri,
    Form(form): Form<Fields>,
) -> Response {
    let ctx = match PageContext::load(s, headers, &uri).await {
        Ok(ctx) => ctx,
        Err(response) => return response,
    };
    if let Err(response) = ctx.check_csrf(&form) {
        return response;
    }
    let actor = match ctx.require_user() {
        Ok(actor) => actor.id,
        Err(response) => return response,
    };
    let scope = match scope(&ctx).await {
        Ok(scope) => scope,
        Err(response) => return response,
    };
    if form.len() > 12 || form.iter().any(|(key, value)| value.len() > if key == "workflows" { 110_000 } else { 49152 }) {
        return ctx.error(StatusCode::BAD_REQUEST, "Runner form is too large.");
    }
    let result = mutate(&ctx, actor, scope, &form).await;
    match result {
        Ok(Some(credential)) => (
            [
                ("content-type", "application/json"),
                ("cache-control", "no-store"),
                (
                    "content-disposition",
                    "attachment; filename=codeyard-runner-credential.json",
                ),
            ],
            Json(credential),
        )
            .into_response(),
        Ok(None) => ctx.redirect(&destination(scope)),
        Err(error) => {
            let (status, message) = match error.downcast_ref::<Refusal>() {
                Some(refusal @ (Refusal::Invalid(_) | Refusal::Conflict(_))) => refusal_details(refusal),
                _ => return failure(&ctx, error),
            };
            let mut draft = form;
            draft.remove("csrf_token");
            render(&ctx, status, Some(message), draft).await
        }
    }
}
async fn mutate(
    ctx: &PageContext,
    actor: i64,
    scope: Scope,
    form: &Fields,
) -> anyhow::Result<Option<Value>> {
    let pool = &ctx.state.pool;
    match field(form, "action") {
        "group-create" | "group-save" => {
            let Scope::Organization(org) = scope else {
                return Err(Refusal::Denied.into());
            };
            let visibility = field(form, "visibility").to_owned();
            // Repository IDs only apply to selected access. Other choices ignore the field, and the store clears the selection.
            let repositories = if visibility == "selected" {
                ids(field(form, "repositories"))?
            } else {
                Vec::new()
            };
            let input = GroupInput {
                name: Some(field(form, "name").to_owned()),
                visibility: Some(visibility.clone()),
                allows_public_repositories: Some(field(form, "public") == "yes"),
                restricted_to_workflows: Some(field(form, "restricted") == "yes"),
                selected_workflows: Some(
                    field(form, "workflows")
                        .lines()
                        .map(str::trim)
                        .filter(|s| !s.is_empty())
                        .map(str::to_owned)
                        .collect(),
                ),
                selected_repository_ids: (visibility == "selected").then_some(repositories),
                ..Default::default()
            };
            native::save_group(
                pool,
                actor,
                org,
                if field(form, "action") == "group-save" {
                    Some(id(form, "id")?)
                } else {
                    None
                },
                &input,
            )
            .await?;
        }
        "group-delete" => {
            let Scope::Organization(org) = scope else {
                return Err(Refusal::Denied.into());
            };
            anyhow::ensure!(
                field(form, "confirm") == "yes",
                Refusal::Invalid("confirmation")
            );
            native::delete_group(pool, actor, org, id(form, "id")?).await?;
        }
        "runner-delete" => {
            anyhow::ensure!(
                field(form, "confirm") == "yes",
                Refusal::Invalid("confirmation")
            );
            native::delete_runner(pool, actor, scope, id(form, "id")?).await?;
        }
        "labels" => {
            native::labels(
                pool,
                actor,
                scope,
                id(form, "id")?,
                &Labels::Replace(labels(field(form, "labels"))),
            )
            .await?;
        }
        "move" => {
            let Scope::Organization(org) = scope else {
                return Err(Refusal::Denied.into());
            };
            native::group_runners(
                pool,
                actor,
                org,
                id(form, "group")?,
                &Selection::Add(id(form, "id")?),
            )
            .await?;
        }
        "registration-token" | "jit" => {
            native::authorize_scope(pool, actor, scope).await?;
            let ticket = credentials::ticket(pool, ctx.state.cluster.as_ref()).await?;
            let expires = chrono::Utc::now() + chrono::Duration::hours(1);
            let jit = field(form, "action") == "jit";
            let work_folder = field(form, "work_folder");
            if jit {
                anyhow::ensure!(
                    credentials::valid_work_folder(work_folder),
                    Refusal::Invalid("work_folder")
                );
            }
            let labels = labels(field(form, "labels"));
            anyhow::ensure!(!jit || !labels.is_empty(), Refusal::Invalid("labels"));
            let group = if jit && matches!(scope, Scope::Organization(_)) {
                Some(id(form, "group")?)
            } else {
                None
            };
            let session = crate::backend::api::http::session::extract_session_id(&ctx.headers)
                .ok_or(Refusal::Denied)?;
            let row = native::registration(
                pool,
                actor,
                scope,
                Registration {
                    session: Some(&session),
                    hash: &ticket.secret_hash(),
                    expires,
                    name: jit.then_some(field(form, "name")),
                    group,
                    labels: &labels,
                },
            )
            .await?;
            if let Some(row) = row {
                let config = credentials::JitConfig {
                    ticket: ticket.encode(),
                    node_name: native::jit_node_name(row.id, &ticket.secret_hash()),
                    work_folder: work_folder.to_owned(),
                };
                return Ok(Some(
                    json!({"runner_id":row.id,"encoded_jit_config":STANDARD.encode(serde_json::to_vec(&config)?)}),
                ));
            }
            return Ok(Some(json!({"token":ticket.encode(),"expires_at":expires})));
        }
        "remove-token" => {
            let token = credentials::new_remove_token();
            let expires = chrono::Utc::now() + chrono::Duration::hours(1);
            native::remove_token(
                pool,
                actor,
                scope,
                &credentials::remove_hash(&token),
                expires,
            )
            .await?;
            return Ok(Some(json!({"token":token,"expires_at":expires})));
        }
        _ => return Err(Refusal::Invalid("action").into()),
    };
    Ok(None)
}
#[derive(Deserialize)]
struct Remove {
    token: String,
}
async fn remove(
    State(s): State<Arc<AppState>>,
    Path(id): Path<i64>,
    Json(input): Json<Remove>,
) -> Response {
    match native::remove_with_token(&s.pool, id, &credentials::remove_hash(&input.token)).await {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(error) => {
            let status = match error.downcast_ref::<Refusal>() {
                Some(Refusal::Denied) => StatusCode::FORBIDDEN,
                Some(Refusal::Missing) => StatusCode::NOT_FOUND,
                _ => StatusCode::SERVICE_UNAVAILABLE,
            };
            (
                status,
                Json(json!({"error":"runner_removal_refused","message":"Runner removal refused"})),
            )
                .into_response()
        }
    }
}
async fn download() -> Response {
    let file = match std::env::current_exe() {
        Ok(path) => tokio::fs::File::open(path).await,
        Err(error) => Err(error),
    };
    match file {
        Ok(file) => {
            use futures_util::StreamExt;
            let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(120);
            let stream = futures_util::stream::unfold(Some(tokio_util::io::ReaderStream::new(file)), move |reader| async move {
                let mut reader = reader?;
                match tokio::time::timeout_at(deadline, reader.next()).await {
                    Ok(Some(bytes)) => Some((bytes, Some(reader))),
                    Ok(None) => None,
                    Err(_) => Some((Err(std::io::ErrorKind::TimedOut.into()), None)),
                }
            });
            ([
                ("content-type", "application/octet-stream"),
                ("content-disposition", "attachment; filename=codeyard-runner"),
            ], Body::from_stream(stream)).into_response()
        }
        Err(_) => StatusCode::SERVICE_UNAVAILABLE.into_response(),
    }
}
