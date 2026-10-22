//! The admin Actions page: whether workflows run on this site, the limits on each run and the
//! runner image.
use crate::backend::api::http::context::PageContext;
use crate::backend::api::http::session;
use crate::backend::api::user::responses::{field, mutation_guard, Fields};
use crate::backend::app::AppState;
use crate::backend::db::{self, actions::ActionsSettings};
use crate::webui::{app::Page, pages::admin::actions as view};
use axum::{
    extract::{DefaultBodyLimit, Form, State},
    http::{HeaderMap, StatusCode, Uri},
    response::Response,
    routing::get,
    Router,
};
use std::sync::Arc;

pub fn router() -> Router<Arc<AppState>> {
    Router::new()
        .route("/admin/actions", get(show).post(save_form))
        .layer(DefaultBodyLimit::max(16 * 1024))
}

/// An optional whole number from a settings form: empty is `None`, anything else must lie in
/// `1..=max`. Repository settings read their limits with it too.
pub(crate) fn optional_number(
    fields: &Fields,
    name: &str,
    label: &str,
    max: i32,
) -> Result<Option<i32>, String> {
    let text = field(fields, name).trim();
    if text.is_empty() {
        return Ok(None);
    }
    match text.parse::<i32>() {
        Ok(value) if (1..=max).contains(&value) => Ok(Some(value)),
        _ => Err(format!("{label} must be a whole number from 1 to {max}.")),
    }
}

/// The limits' upper bounds, as the schema checks them.
pub(crate) const MAX_TIMEOUT_MINUTES: i32 = 10080;
pub(crate) const MAX_CPUS: i32 = 1024;
pub(crate) const MAX_MEMORY_GIB: i32 = 4096;

fn parse_fields(fields: &Fields) -> Result<ActionsSettings, String> {
    let image = field(fields, "runner_image").trim();
    if image.len() > 255 || image.chars().any(char::is_whitespace) {
        return Err("The runner image must be one image name without spaces.".to_owned());
    }
    Ok(ActionsSettings {
        enabled: field(fields, "enabled") == "on",
        run_timeout_minutes: optional_number(
            fields,
            "run_timeout_minutes",
            "The timeout",
            MAX_TIMEOUT_MINUTES,
        )?,
        job_cpus: optional_number(fields, "job_cpus", "CPUs per job", MAX_CPUS)?,
        job_memory_gib: optional_number(
            fields,
            "job_memory_gib",
            "Memory per job",
            MAX_MEMORY_GIB,
        )?,
        runner_image: (!image.is_empty()).then(|| image.to_owned()),
    })
}

fn text(value: Option<i32>) -> String {
    value.map(|value| value.to_string()).unwrap_or_default()
}

async fn render(
    ctx: &PageContext,
    status: StatusCode,
    message: Option<String>,
    submitted: Option<&Fields>,
) -> Response {
    let settings = match submitted {
        Some(fields) => view::Settings {
            enabled: field(fields, "enabled") == "on",
            run_timeout_minutes: field(fields, "run_timeout_minutes").trim().to_owned(),
            job_cpus: field(fields, "job_cpus").trim().to_owned(),
            job_memory_gib: field(fields, "job_memory_gib").trim().to_owned(),
            runner_image: field(fields, "runner_image").trim().to_owned(),
        },
        None => match db::actions::get_actions_settings(&ctx.state.pool).await {
            Ok(saved) => view::Settings {
                enabled: saved.enabled,
                run_timeout_minutes: text(saved.run_timeout_minutes),
                job_cpus: text(saved.job_cpus),
                job_memory_gib: text(saved.job_memory_gib),
                runner_image: saved.runner_image.unwrap_or_default(),
            },
            Err(_) => {
                return ctx.error(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "Unable to load the Actions settings.",
                )
            }
        },
    };
    let config = &ctx.state.ci_runs.config;
    let model = view::Page {
        csrf_token: ctx.csrf_token.clone(),
        settings,
        default_timeout_minutes: config.run_timeout_seconds.div_ceil(60),
        default_image: config
            .act_platform_map
            .get("ubuntu-latest")
            .cloned()
            .unwrap_or_default(),
        runs_per_node: config.max_active_runs,
        is_error: !status.is_success(),
        message,
    };
    ctx.page_model_status(status, "Actions", Page::AdminActions(model))
}

async fn show(State(state): State<Arc<AppState>>, headers: HeaderMap, uri: Uri) -> Response {
    let ctx = match PageContext::load(state, headers, &uri).await {
        Ok(ctx) => ctx,
        Err(response) => return response,
    };
    if ctx.administrator().is_none() {
        return ctx.error(StatusCode::FORBIDDEN, "Administrator access is required.");
    }
    let saved = ctx
        .query
        .contains_key("saved")
        .then(|| "Actions settings saved.".to_owned());
    render(&ctx, StatusCode::OK, saved, None).await
}

async fn save_form(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    uri: Uri,
    Form(form): Form<Fields>,
) -> Response {
    let ctx = match PageContext::load(state, headers, &uri).await {
        Ok(ctx) => ctx,
        Err(response) => return response,
    };
    if let Err(response) = mutation_guard(&ctx, &form, true) {
        return response;
    }
    let settings = match parse_fields(&form) {
        Ok(settings) => settings,
        Err(message) => {
            return render(&ctx, StatusCode::BAD_REQUEST, Some(message), Some(&form)).await
        }
    };
    let actor = ctx.user.as_ref().expect("admin guard").id;
    let session = session::extract_session_id(&ctx.headers).unwrap_or_default();
    match db::actions::save_actions_settings(&ctx.state.pool, actor, &session, &settings).await {
        Ok(true) => ctx.redirect("/admin/actions?saved=1"),
        Ok(false) => ctx.error(StatusCode::FORBIDDEN, "Administrator access is required."),
        Err(_) => {
            let message = "Unable to save the Actions settings.".to_owned();
            render(
                &ctx,
                StatusCode::SERVICE_UNAVAILABLE,
                Some(message),
                Some(&form),
            )
            .await
        }
    }
}
