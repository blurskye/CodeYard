//! Admin email settings and test-mail forms.
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
use axum::routing::post;
use axum::Router;
use codeyard_api_http::cell_routes::CellRoutes;
use codeyard_api_http::context::PageContext;
use codeyard_api_http::shared::user::responses::field;
use codeyard_api_http::shared::user::responses::Fields;
use codeyard_app::app::AppState;
use codeyard_db_identity::email::NewEmailSettings;
use codeyard_types::mail_config::Security;
use codeyard_types::mail_config::SmtpConfig;
use codeyard_types::mail_config::Source;
use codeyard_webui::app::Page;
use codeyard_webui::pages::admin::email as view;
use std::sync::Arc;

pub fn router() -> Router<Arc<AppState>> {
    Router::new()
        .cell_route("/admin/email", get(show).post(save_form))
        .cell_route("/admin/email/test", post(test_form))
        .layer(DefaultBodyLimit::max(16 * 1024))
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
        port: codeyard_types::mail_config::parse_port(field(fields, "port"), security)?,
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
    let mut password = if server.username.is_empty() {
        String::new()
    } else {
        field(fields, "password").to_owned()
    };
    if password.len() > 255 {
        return Err((
            StatusCode::BAD_REQUEST,
            "The password is too long.".to_owned(),
        ));
    }
    // Inherit credentials only for an authenticated relay and only from valid environment
    // settings. An invalid old environment must not prevent saving its replacement.
    if password.is_empty() && !server.username.is_empty() {
        let saved = codeyard_db_identity::email::get_email_settings(&state.pool)
            .await
            .map_err(|_| {
                (
                    StatusCode::SERVICE_UNAVAILABLE,
                    "Unable to read the current mail settings.".to_owned(),
                )
            })?;
        if saved.is_none() {
            if let Ok(Some(env)) = codeyard_types::mail_config::from_env() {
                password = env.password.unwrap_or_default();
            }
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
    let saved = codeyard_db_identity::email::save_email_settings(
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
async fn current(
    pool: &codeyard_database::DbPool,
    submitted: Option<&Fields>,
) -> Result<view::Settings, String> {
    let mut settings = match codeyard_db_identity::mail_config::describe(pool).await? {
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
        public_url: codeyard_types::http_config::public_origin(),
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
    if let Err(response) = require_account(&ctx, true) {
        return response;
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
    let session = codeyard_api_http::session::extract_session_id(&ctx.headers).unwrap_or_default();
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
    let mut ctx = match PageContext::load(state, headers, &uri).await {
        Ok(ctx) => ctx,
        Err(response) => return response,
    };
    if let Err(response) = mutation_guard(&ctx, &form, true) {
        return response;
    }
    // The result page's own forms, such as the theme, return to the page and not the POST URL.
    ctx.return_to = Some("/admin/email".into());
    let session = codeyard_api_http::session::extract_session_id(&ctx.headers).unwrap_or_default();
    match crate::admin::email_test::send_test(
        &ctx.state.pool,
        ctx.state.secret_encryptor.as_deref(),
        &session,
    )
    .await
    {
        Ok(to) => {
            render(
                &ctx,
                StatusCode::OK,
                Some(format!("Test email sent to {to}.")),
                None,
            )
            .await
        }
        Err(error) => {
            let status = match &error {
                crate::admin::email_test::TestMailError::Authority(
                    codeyard_identity::sessions::AccountAuthorityError::SessionUnavailable,
                ) => StatusCode::SERVICE_UNAVAILABLE,
                crate::admin::email_test::TestMailError::Authority(_) => StatusCode::FORBIDDEN,
                crate::admin::email_test::TestMailError::NoAddress => StatusCode::BAD_REQUEST,
                crate::admin::email_test::TestMailError::Delivery(_) => StatusCode::BAD_GATEWAY,
            };
            render(
                &ctx,
                status,
                Some(format!("The test email failed: {error}")),
                None,
            )
            .await
        }
    }
}
