//! The admin Actions page: whether workflows run on this site, the limits on each run and the
//! runner image.
use crate::user::responses::mutation_guard;
use crate::user::responses::require_account;
use axum::extract::DefaultBodyLimit;
use axum::extract::Form;
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
use codeyard_db_actions::actions::ActionsSettings;
use codeyard_webui::app::Page;
use codeyard_webui::pages::admin::actions as view;
use std::sync::Arc;

pub fn router() -> Router<Arc<AppState>> {
    Router::new()
        .route("/admin/actions", get(show).post(save_form))
        .layer(DefaultBodyLimit::max(16 * 1024))
}

fn parse_fields(fields: &Fields) -> Result<ActionsSettings, String> {
    let image = field(fields, "runner_image").trim();
    if image.len() > 255 || image.chars().any(char::is_whitespace) {
        return Err("The runner image must be one image name without spaces.".to_owned());
    }
    let results_url = field(fields, "results_url").trim();
    if !results_url.is_empty() {
        let url = url::Url::parse(results_url)
            .map_err(|_| "The Results URL must be an HTTP or HTTPS origin.".to_owned())?;
        if !matches!(url.scheme(), "http" | "https")
            || url.host_str().is_none()
            || !url.username().is_empty()
            || url.password().is_some()
            || url.query().is_some()
            || url.fragment().is_some()
            || url.path() != "/"
        {
            return Err(
                "The Results URL must be an HTTP or HTTPS origin without credentials or a path."
                    .to_owned(),
            );
        }
    }
    let permissions = match field(fields, "default_workflow_permissions").trim() {
        "" | "restricted" => "restricted",
        "permissive" => "permissive",
        _ => return Err("Workflow token permissions must be restricted or permissive.".into()),
    };
    let legacy = !fields.contains_key("default_engine");
    let settings = ActionsSettings {
        default_engine: match field(fields, "default_engine").trim() {
            "" => "codeyard-actions",
            value => value,
        }
        .into(),
        codeyard_actions_enabled: legacy || field(fields, "codeyard_actions_enabled") == "on",
        act_enabled: legacy || field(fields, "act_enabled") == "on",
        default_workflow_permissions: permissions.into(),
        results_url: (!results_url.is_empty())
            .then(|| results_url.trim_end_matches('/').to_owned()),
        cache_size_gib: optional_number(fields, "cache_size_gib", "Cache size in GiB", 10240)?,
        cache_retention_days: optional_number(
            fields,
            "cache_retention_days",
            "Cache expiry in days",
            365,
        )?,
        artifact_retention_days: optional_number(
            fields,
            "artifact_retention_days",
            "Artifact retention in days",
            365,
        )?,
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
    };
    settings
        .validate_engines()
        .map_err(|error| error.to_string())?;
    Ok(settings)
}

async fn render(
    ctx: &PageContext,
    status: StatusCode,
    message: Option<String>,
    submitted: Option<&Fields>,
) -> Response {
    let settings = match submitted {
        Some(fields) => view::Settings {
            default_engine: field(fields, "default_engine").to_owned(),
            codeyard_actions_enabled: field(fields, "codeyard_actions_enabled") == "on",
            act_enabled: field(fields, "act_enabled") == "on",
            default_workflow_permissions: field(fields, "default_workflow_permissions").to_owned(),
            enabled: field(fields, "enabled") == "on",
            run_timeout_minutes: field(fields, "run_timeout_minutes").trim().to_owned(),
            job_cpus: field(fields, "job_cpus").trim().to_owned(),
            job_memory_gib: field(fields, "job_memory_gib").trim().to_owned(),
            runner_image: field(fields, "runner_image").trim().to_owned(),
            results_url: field(fields, "results_url").trim().to_owned(),
            cache_size_gib: field(fields, "cache_size_gib").trim().to_owned(),
            cache_retention_days: field(fields, "cache_retention_days").trim().to_owned(),
            artifact_retention_days: field(fields, "artifact_retention_days").trim().to_owned(),
        },
        None => match codeyard_db_actions::actions::get_actions_settings(&ctx.state.pool).await {
            Ok(saved) => view::Settings {
                default_engine: saved.default_engine,
                codeyard_actions_enabled: saved.codeyard_actions_enabled,
                act_enabled: saved.act_enabled,
                default_workflow_permissions: saved.default_workflow_permissions,
                enabled: saved.enabled,
                run_timeout_minutes: text(saved.run_timeout_minutes),
                job_cpus: text(saved.job_cpus),
                job_memory_gib: text(saved.job_memory_gib),
                runner_image: saved.runner_image.unwrap_or_default(),
                results_url: saved.results_url.unwrap_or_default(),
                cache_size_gib: text(saved.cache_size_gib),
                cache_retention_days: text(saved.cache_retention_days),
                artifact_retention_days: text(saved.artifact_retention_days),
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
    if let Err(response) = require_account(&ctx, true) {
        return response;
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
    let session = codeyard_api_http::session::extract_session_id(&ctx.headers).unwrap_or_default();
    match codeyard_db_actions::actions::save_actions_settings(
        &ctx.state.pool,
        actor,
        &session,
        &settings,
    )
    .await
    {
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

use codeyard_api_http::shared::admin::actions::MAX_CPUS;

use codeyard_api_http::shared::admin::actions::MAX_MEMORY_GIB;

use codeyard_api_http::shared::admin::actions::MAX_TIMEOUT_MINUTES;

use codeyard_api_http::shared::admin::actions::optional_number;

use codeyard_api_http::shared::admin::actions::text;
