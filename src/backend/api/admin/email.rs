//! Admin email settings: the page and its forms, plus the same operations as JSON.
use super::accounts::require_admin_user;
use crate::backend::api::http::context::PageContext;
use crate::backend::api::http::error::{ApiError, ApiFailure};
use crate::backend::api::http::{csrf, middleware, session};
use crate::backend::api::user::responses::{field, mutation_guard, Fields};
use crate::backend::app::AppState;
use crate::backend::db::{self, email::NewEmailSettings};
use crate::backend::email::{
    self,
    config::{self, Security, SmtpConfig, Source},
};
use crate::webui::{app::Page, pages::admin::email as view};
use axum::{
    extract::{DefaultBodyLimit, Form, State},
    http::{HeaderMap, StatusCode, Uri},
    middleware::from_fn,
    response::Response,
    routing::{get, post},
    Json, Router,
};
use std::sync::Arc;

pub fn router() -> Router<Arc<AppState>> {
    let api = Router::new()
        .route("/api/auth/admin/email", get(api_get).put(api_put))
        .route("/api/auth/admin/email/test", post(api_test))
        .route_layer(from_fn(csrf::auth_submission))
        .layer(from_fn(middleware::private_api_response));
    let pages = Router::new()
        .route("/admin/email", get(show).post(save_form))
        .route("/admin/email/test", post(test_form))
        .layer(DefaultBodyLimit::max(16 * 1024));
    api.merge(pages)
}

/// Reads and checks the form fields. The password is handled apart from the rest.
fn parse_fields(fields: &Fields) -> Result<SmtpConfig, String> {
    let text = |name: &str| field(fields, name).trim().to_owned();
    let security = match text("security").as_str() {
        "" => Security::Starttls,
        value => Security::parse(value).ok_or("Choose STARTTLS, TLS or no encryption.")?,
    };
    let config = SmtpConfig {
        host: text("host"),
        port: config::parse_port(field(fields, "port"), security)?,
        security,
        username: text("username"),
        password: None,
        from_name: text("from_name"),
        from_address: text("from_address"),
        reply_to: text("reply_to"),
    };
    config.validate()?;
    Ok(config)
}

async fn save(
    state: &AppState,
    actor: i64,
    session: &str,
    fields: &Fields,
) -> Result<(), (StatusCode, String)> {
    let server = parse_fields(fields).map_err(|message| (StatusCode::BAD_REQUEST, message))?;
    let mut password = field(fields, "password").to_owned();
    if password.len() > 255 {
        return Err((
            StatusCode::BAD_REQUEST,
            "The password is too long.".to_owned(),
        ));
    }
    // The first save from the environment's settings keeps the environment's password.
    if password.is_empty() {
        if let Ok(Some((env, Source::Environment, _))) = config::describe(&state.pool).await {
            password = env.password.unwrap_or_default();
        }
    }
    let sealed = if password.is_empty() {
        None
    } else {
        let secrets = state.secret_encryptor.as_deref().ok_or((
            StatusCode::CONFLICT,
            "Set SECRETS_MASTER_KEY on the server to save a mail password.".to_owned(),
        ))?;
        let sealed = secrets.encrypt(password.as_bytes()).map_err(|_| {
            (
                StatusCode::SERVICE_UNAVAILABLE,
                "Unable to protect the mail password.".to_owned(),
            )
        })?;
        Some(sealed)
    };
    let saved = db::email::save_email_settings(
        &state.pool,
        actor,
        session,
        &NewEmailSettings {
            host: &server.host,
            port: i32::from(server.port),
            security: server.security.as_str(),
            username: &server.username,
            password: sealed
                .as_ref()
                .map(|(ciphertext, nonce)| (ciphertext.as_slice(), nonce.as_slice())),
            from_name: &server.from_name,
            from_address: &server.from_address,
            reply_to: &server.reply_to,
        },
    )
    .await
    .map_err(|_| {
        (
            StatusCode::SERVICE_UNAVAILABLE,
            "Unable to save the email settings.".to_owned(),
        )
    })?;
    if saved {
        Ok(())
    } else {
        Err((
            StatusCode::FORBIDDEN,
            "Administrator access is required.".to_owned(),
        ))
    }
}

/// The settings to show. Fields the admin just submitted replace the stored ones.
async fn current(pool: &db::DbPool, submitted: Option<&Fields>) -> Result<view::Settings, String> {
    let mut settings = match config::describe(pool).await? {
        Some((config, source, password_set)) => view::Settings {
            source: match source {
                Source::Database => "database",
                Source::Environment => "environment",
            }
            .to_owned(),
            host: config.host,
            port: config.port.to_string(),
            security: config.security.as_str().to_owned(),
            username: config.username,
            password_set,
            from_name: config.from_name,
            from_address: config.from_address,
            reply_to: config.reply_to,
        },
        None => empty_settings(),
    };
    if let Some(fields) = submitted {
        for (target, name) in [
            (&mut settings.host, "host"),
            (&mut settings.port, "port"),
            (&mut settings.security, "security"),
            (&mut settings.username, "username"),
            (&mut settings.from_name, "from_name"),
            (&mut settings.from_address, "from_address"),
            (&mut settings.reply_to, "reply_to"),
        ] {
            *target = field(fields, name).trim().to_owned();
        }
    }
    Ok(settings)
}

fn empty_settings() -> view::Settings {
    view::Settings {
        source: "none".to_owned(),
        host: String::new(),
        port: String::new(),
        security: "starttls".to_owned(),
        username: String::new(),
        password_set: false,
        from_name: String::new(),
        from_address: String::new(),
        reply_to: String::new(),
    }
}

/// Sends the test message to the administrator's own address and returns that address.
async fn send_test_mail(state: &AppState, admin: &db::models::User) -> Result<String, String> {
    let to = admin
        .email
        .as_deref()
        .map(str::trim)
        .filter(|email| !email.is_empty())
        .ok_or("Your account has no email address. Add one in Settings under Email address.")?;
    email::send_test(&state.pool, state.secret_encryptor.as_deref(), to).await?;
    Ok(to.to_owned())
}

async fn render(
    ctx: &PageContext,
    status: StatusCode,
    message: Option<String>,
    submitted: Option<&Fields>,
) -> Response {
    let (settings, problem) = match current(&ctx.state.pool, submitted).await {
        Ok(settings) => (settings, None),
        Err(problem) => (empty_settings(), Some(problem)),
    };
    let is_error = problem.is_some() || !status.is_success();
    let model = view::Page {
        csrf_token: ctx.csrf_token.clone(),
        settings,
        can_store_password: ctx.state.secret_encryptor.is_some(),
        public_url: crate::backend::api::http::config::public_origin(),
        message: problem.or(message),
        is_error,
    };
    ctx.page_model_status(status, "Email", Page::Email(model))
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
        .then(|| "Email settings saved.".to_owned());
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
    let actor = ctx.user.as_ref().expect("admin guard").id;
    let session = session::extract_session_id(&ctx.headers).unwrap_or_default();
    match save(&ctx.state, actor, &session, &form).await {
        Ok(()) => ctx.redirect("/admin/email?saved=1"),
        Err((status, message)) => render(&ctx, status, Some(message), Some(&form)).await,
    }
}

async fn test_form(
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
    let admin = ctx.user.as_ref().expect("admin guard");
    match send_test_mail(&ctx.state, admin).await {
        Ok(to) => {
            render(
                &ctx,
                StatusCode::OK,
                Some(format!("Test email sent to {to}.")),
                None,
            )
            .await
        }
        Err(message) => {
            let message = format!("The test email failed: {message}");
            render(&ctx, StatusCode::BAD_GATEWAY, Some(message), None).await
        }
    }
}

fn failure((status, message): (StatusCode, String)) -> ApiFailure {
    let code = match status {
        StatusCode::BAD_REQUEST => "validation_error",
        StatusCode::FORBIDDEN => "forbidden",
        StatusCode::CONFLICT => "conflict",
        _ => "unavailable",
    };
    ApiError::failure(status, code, &message)
}

async fn api_get(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Result<Json<view::Settings>, ApiFailure> {
    require_admin_user(&state, &headers).await?;
    let settings = current(&state.pool, None)
        .await
        .map_err(|message| failure((StatusCode::SERVICE_UNAVAILABLE, message)))?;
    Ok(Json(settings))
}

async fn api_put(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(fields): Json<Fields>,
) -> Result<Json<view::Settings>, ApiFailure> {
    let admin = require_admin_user(&state, &headers).await?;
    let session = session::extract_session_id(&headers).unwrap_or_default();
    save(&state, admin.id, &session, &fields)
        .await
        .map_err(failure)?;
    api_get(State(state), headers).await
}

async fn api_test(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Result<Json<serde_json::Value>, ApiFailure> {
    let admin = require_admin_user(&state, &headers).await?;
    match send_test_mail(&state, &admin).await {
        Ok(to) => Ok(Json(serde_json::json!({ "sent_to": to }))),
        Err(message) => Err(ApiError::failure(
            StatusCode::BAD_GATEWAY,
            "email_failed",
            &message,
        )),
    }
}
