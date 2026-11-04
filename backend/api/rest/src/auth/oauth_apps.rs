//! GitHub's OAuth device and authorization-code flows, including the public CLI client.
use axum::body::Bytes;
use axum::extract::Form;
use axum::extract::Query;
use axum::extract::State;
use axum::http::header;
use axum::http::HeaderMap;
use axum::http::StatusCode;
use axum::http::Uri;
use axum::response::IntoResponse;
use axum::response::Redirect;
use axum::response::Response;
use axum::routing::get;
use axum::routing::post;
use axum::Json;
use axum::Router;
use codeyard_api_http::context::PageContext;
use codeyard_api_http::shared::user::responses::field;
use codeyard_app::app::AppState;
use codeyard_db_identity::oauth_apps::Exchange;
use codeyard_db_identity::oauth_apps::Grant;
use codeyard_model::http_config::link_origin;
use codeyard_support::token_hash::token_hash;
use codeyard_webui::app::Page;
use codeyard_webui::pages::account;
use serde_json::json;
use serde_json::Value;
use std::collections::HashMap;
use std::sync::Arc;

type Fields = HashMap<String, String>;

pub(super) fn router() -> Router<Arc<AppState>> {
    Router::new()
        .route("/login/device/code", post(device_code))
        .route("/login/oauth/access_token", post(access_token))
        .route("/login/device", get(device_get).post(device_post))
        .route(
            "/login/oauth/authorize",
            get(authorize_get).post(authorize_post),
        )
        .route("/settings/applications", get(applications))
        .route(
            "/settings/applications/new",
            get(register_get).post(register_post),
        )
        .layer(axum::extract::DefaultBodyLimit::max(64 * 1024))
        .layer(axum::middleware::from_fn(
            codeyard_api_http::middleware::private_sensitive_response,
        ))
}

fn payload(headers: &HeaderMap, bytes: &[u8]) -> Option<Fields> {
    if headers
        .get(header::CONTENT_TYPE)
        .is_some_and(|v| v.as_bytes().starts_with(b"application/json"))
    {
        serde_json::from_slice(bytes).ok()
    } else {
        Some(url::form_urlencoded::parse(bytes).into_owned().collect())
    }
}

fn reply(headers: &HeaderMap, value: Value) -> Response {
    let scopes = value
        .get("scope")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_owned();
    let json = headers
        .get(header::ACCEPT)
        .is_some_and(|v| v.as_bytes().windows(4).any(|s| s == b"json"));
    let mut response = if json {
        Json(value).into_response()
    } else {
        let mut body = url::form_urlencoded::Serializer::new(String::new());
        for (key, value) in value.as_object().expect("OAuth response object") {
            body.append_pair(
                key,
                value
                    .as_str()
                    .map(str::to_owned)
                    .unwrap_or_else(|| value.to_string())
                    .as_str(),
            );
        }
        (
            [(header::CONTENT_TYPE, "application/x-www-form-urlencoded")],
            body.finish(),
        )
            .into_response()
    };
    response.headers_mut().insert(
        "x-oauth-scopes",
        scopes
            .parse()
            .unwrap_or_else(|_| header::HeaderValue::from_static("")),
    );
    response
}

fn failure(headers: &HeaderMap, code: &str) -> Response {
    reply(
        headers,
        json!({"error": code, "error_description": code.replace('_', " "), "error_uri": "https://docs.github.com/en/apps/oauth-apps"}),
    )
}

async fn admit(state: &AppState, headers: &HeaderMap, key: &str) -> Result<(), Response> {
    if codeyard_db_cluster::node_config::get_node_configuration(&state.pool)
        .await
        .ok()
        .flatten()
        .is_none_or(|c| !c.setup_completed)
    {
        return Err(StatusCode::SERVICE_UNAVAILABLE.into_response());
    }
    codeyard_db_identity::auth_admission::admit_operation(
        &state.pool,
        codeyard_app::api_shared::http::session::client_address(headers),
        "device",
        &format!(
            "{key}:{}",
            codeyard_app::api_shared::http::session::client_address(headers).unwrap_or("local")
        ),
    )
    .await
    .map(|_| ())
    .map_err(|error| match error {
        codeyard_db_identity::auth_admission::AdmissionError::Limited => {
            StatusCode::TOO_MANY_REQUESTS.into_response()
        }
        codeyard_db_identity::auth_admission::AdmissionError::Unavailable => {
            StatusCode::SERVICE_UNAVAILABLE.into_response()
        }
    })
}

async fn device_code(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    bytes: Bytes,
) -> Response {
    if let Err(response) = admit(&state, &headers, "oauth-device-code").await {
        return response;
    }
    let Some(form) = payload(&headers, &bytes) else {
        return failure(&headers, "invalid_request");
    };
    let id = field(&form, "client_id");
    match codeyard_identity::oauth_apps::client(&state.pool, id).await {
        Ok(Some(_)) => (),
        Ok(None) => return failure(&headers, "incorrect_client_credentials"),
        Err(_) => return StatusCode::SERVICE_UNAVAILABLE.into_response(),
    }
    let Some(mut scopes) = codeyard_identity::oauth_apps::scopes(field(&form, "scope")) else {
        return failure(&headers, "invalid_scope");
    };
    match codeyard_db_identity::github_apps::app(&state.pool, id).await {
        Ok(Some(_)) => scopes.clear(),
        Ok(None) => {}
        Err(_) => return StatusCode::SERVICE_UNAVAILABLE.into_response(),
    }
    let value = codeyard_support::crypto::random_hex();
    let user_code = codeyard_identity::oauth_apps::user_code();
    let grant = Grant {
        hash: token_hash(&value),
        client_id: id.to_owned(),
        user_code: user_code.clone(),
        scopes,
        expires_at: chrono::Utc::now().timestamp() + 900,
        interval: 5,
        last_poll: 0,
        user_id: None,
        session_id: None,
        denied: false,
        kind: "device".into(),
        redirect_uri: String::new(),
        state: String::new(),
        challenge: String::new(),
    };
    if codeyard_db_identity::oauth_apps::save_grant(&state.pool, &grant)
        .await
        .is_err()
    {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    }
    reply(
        &headers,
        json!({"device_code": value, "user_code": user_code, "verification_uri": format!("{}/login/device", link_origin()), "expires_in": 900, "interval": 5}),
    )
}

async fn access_token(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    bytes: Bytes,
) -> Response {
    let Some(form) = payload(&headers, &bytes) else {
        return failure(&headers, "invalid_request");
    };
    let id = field(&form, "client_id");
    let app = match codeyard_identity::oauth_apps::client(&state.pool, id).await {
        Ok(Some(app)) => app,
        Ok(None) => return failure(&headers, "incorrect_client_credentials"),
        Err(_) => return StatusCode::SERVICE_UNAVAILABLE.into_response(),
    };
    let device = field(&form, "grant_type") == "urn:ietf:params:oauth:grant-type:device_code";
    if !device && !matches!(field(&form, "grant_type"), "" | "authorization_code") {
        return failure(&headers, "unsupported_grant_type");
    }
    if !device && !codeyard_identity::oauth_apps::valid_secret(&app, field(&form, "client_secret"))
    {
        return failure(&headers, "incorrect_client_credentials");
    }
    let code = field(&form, if device { "device_code" } else { "code" });
    if code.len() != 64 {
        return failure(
            &headers,
            if device {
                "incorrect_device_code"
            } else {
                "bad_verification_code"
            },
        );
    }
    let value = match codeyard_db_identity::github_apps::app(&state.pool, id).await {
        Ok(Some(_)) => format!("ghu_{}", codeyard_support::crypto::random_hex()),
        Ok(None) => codeyard_support::crypto::random_hex(),
        Err(_) => return StatusCode::SERVICE_UNAVAILABLE.into_response(),
    };
    // Device polling has its own durable interval; it must not consume the password-login budget.
    match codeyard_db_identity::oauth_apps::exchange(
        &state.pool,
        &token_hash(code),
        id,
        if device { "device" } else { "web" },
        form.get("redirect_uri").map(String::as_str),
        field(&form, "code_verifier"),
        form.get("state").map(String::as_str),
        &value,
    )
    .await
    {
        Ok(Exchange::Token(scopes)) => reply(
            &headers,
            json!({"access_token": value, "token_type": "bearer", "scope": scopes}),
        ),
        Ok(Exchange::Pending) => failure(&headers, "authorization_pending"),
        Ok(Exchange::SlowDown(interval)) => reply(
            &headers,
            json!({"error": "slow_down", "interval": interval}),
        ),
        Ok(Exchange::Denied) => failure(&headers, "access_denied"),
        Ok(Exchange::Expired) => failure(&headers, "expired_token"),
        Ok(Exchange::Invalid) => failure(
            &headers,
            if device {
                "incorrect_device_code"
            } else {
                "bad_verification_code"
            },
        ),
        Err(_) => StatusCode::SERVICE_UNAVAILABLE.into_response(),
    }
}

fn login(ctx: &PageContext) -> Option<Response> {
    ctx.user.is_none().then(|| {
        ctx.redirect(&format!(
            "/login?redirect={}",
            codeyard_webui::encode_component(&ctx.location)
        ))
    })
}

fn device_page(
    ctx: &PageContext,
    code: &str,
    app: Option<(String, String)>,
    message: Option<&str>,
) -> Response {
    ctx.page_model(
        "Device activation",
        Page::Identity(account::Page::OAuthDevice {
            csrf_token: ctx.csrf_token.clone(),
            code: code.to_owned(),
            application: app,
            message: message.map(str::to_owned),
        }),
    )
}

async fn device_get(State(state): State<Arc<AppState>>, headers: HeaderMap, uri: Uri) -> Response {
    let ctx = match PageContext::load(state, headers, &uri).await {
        Ok(ctx) => ctx,
        Err(response) => return response,
    };
    if let Some(response) = login(&ctx) {
        return response;
    }
    device_page(&ctx, "", None, None)
}

async fn device_post(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    uri: Uri,
    Form(form): Form<Fields>,
) -> Response {
    let ctx = match PageContext::load(state, headers, &uri).await {
        Ok(ctx) => ctx,
        Err(response) => return response,
    };
    if let Some(response) = login(&ctx) {
        return response;
    }
    if let Err(response) = ctx.check_csrf(&form) {
        return response;
    }
    let code = codeyard_identity::oauth_apps::normalize_code(field(&form, "user_code"));
    if let Err(response) = admit(&ctx.state, &ctx.headers, "oauth-user-code").await {
        return response;
    }
    let grant = match codeyard_db_identity::oauth_apps::device_grant(&ctx.state.pool, &code).await {
        Ok(Some(grant)) => grant,
        Ok(None) => {
            return device_page(
                &ctx,
                "",
                None,
                Some("This code is invalid, expired or already used."),
            )
        }
        Err(_) => return ctx.error(StatusCode::SERVICE_UNAVAILABLE, "Authorization unavailable"),
    };
    let app = match codeyard_identity::oauth_apps::client(&ctx.state.pool, &grant.client_id).await {
        Ok(Some(app)) => app,
        _ => return ctx.error(StatusCode::SERVICE_UNAVAILABLE, "Application unavailable"),
    };
    let display_scopes =
        match codeyard_db_identity::github_apps::app(&ctx.state.pool, &grant.client_id).await {
            Ok(Some(app)) => app.permissions,
            Ok(None) => grant.scopes.clone(),
            Err(_) => return ctx.error(StatusCode::SERVICE_UNAVAILABLE, "Application unavailable"),
        };
    if matches!(field(&form, "decision"), "approve" | "deny") {
        let actor = ctx.user.as_ref().unwrap().id;
        let Some(session) = codeyard_api_http::session::extract_session_id(&ctx.headers) else {
            return ctx.error(StatusCode::UNAUTHORIZED, "Sign in again");
        };
        match codeyard_db_identity::oauth_apps::approve(
            &ctx.state.pool,
            &code,
            actor,
            &session,
            field(&form, "decision") == "deny",
        )
        .await
        {
            Ok(true) => {
                return device_page(
                    &ctx,
                    "",
                    None,
                    Some(if field(&form, "decision") == "deny" {
                        "Authorization denied. You may close this page."
                    } else {
                        "Device authorized. You may close this page."
                    }),
                )
            }
            Ok(false) => {
                return ctx.error(
                    StatusCode::BAD_REQUEST,
                    "This authorization is no longer available",
                )
            }
            Err(_) => {
                return ctx.error(StatusCode::SERVICE_UNAVAILABLE, "Authorization unavailable")
            }
        }
    }
    device_page(&ctx, &code, Some((app.name, display_scopes)), None)
}

async fn authorization(
    ctx: &PageContext,
    form: &Fields,
) -> Result<
    (
        codeyard_db_identity::oauth_apps::Application,
        String,
        String,
    ),
    Response,
> {
    let mut app = codeyard_identity::oauth_apps::client(&ctx.state.pool, field(form, "client_id"))
        .await
        .map_err(|_| ctx.error(StatusCode::SERVICE_UNAVAILABLE, "Application unavailable"))?
        .ok_or_else(|| ctx.error(StatusCode::NOT_FOUND, "Application not found"))?;
    if let Some(uri) = form.get("redirect_uri") {
        if codeyard_db_identity::github_app_manifests::callback(
            &ctx.state.pool,
            &app.client_id,
            uri,
        )
        .await
        .map_err(|_| ctx.error(StatusCode::SERVICE_UNAVAILABLE, "Application unavailable"))?
        {
            app.redirect_uri = uri.clone();
        }
    }
    let redirect =
        codeyard_identity::oauth_apps::redirect(&app, form.get("redirect_uri").map(String::as_str))
            .ok_or_else(|| ctx.error(StatusCode::BAD_REQUEST, "Invalid redirect URI"))?;
    let scopes = codeyard_identity::oauth_apps::scopes(field(form, "scope"))
        .ok_or_else(|| ctx.error(StatusCode::BAD_REQUEST, "Unsupported OAuth scope"))?;
    if field(form, "state").len() > 1024
        || (!field(form, "code_challenge").is_empty()
            && (field(form, "code_challenge_method") != "S256"
                || field(form, "code_challenge").len() != 43))
    {
        return Err(ctx.error(StatusCode::BAD_REQUEST, "Invalid authorization request"));
    }
    let mut scopes = scopes;
    if let Some(github_app) =
        codeyard_db_identity::github_apps::app(&ctx.state.pool, &app.client_id)
            .await
            .map_err(|_| ctx.error(StatusCode::SERVICE_UNAVAILABLE, "Application unavailable"))?
    {
        app.name = format!(
            "{} (repository permissions: {})",
            app.name, github_app.permissions
        );
        scopes.clear();
    }
    Ok((app, redirect, scopes))
}

async fn authorize_get(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    uri: Uri,
    Query(form): Query<Fields>,
) -> Response {
    let ctx = match PageContext::load(state, headers, &uri).await {
        Ok(ctx) => ctx,
        Err(response) => return response,
    };
    if let Some(response) = login(&ctx) {
        return response;
    }
    let (app, redirect, scopes) = match authorization(&ctx, &form).await {
        Ok(values) => values,
        Err(response) => return response,
    };
    ctx.page_model_with_form_target(
        "Authorize application",
        Page::Identity(account::Page::OAuthAuthorize {
            csrf_token: ctx.csrf_token.clone(),
            name: app.name,
            client_id: app.client_id,
            redirect_uri: redirect.clone(),
            scopes,
            state: field(&form, "state").to_owned(),
            challenge: field(&form, "code_challenge").to_owned(),
        }),
        &redirect,
    )
}

async fn authorize_post(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    uri: Uri,
    Form(form): Form<Fields>,
) -> Response {
    let ctx = match PageContext::load(state, headers, &uri).await {
        Ok(ctx) => ctx,
        Err(response) => return response,
    };
    if let Some(response) = login(&ctx) {
        return response;
    }
    if let Err(response) = ctx.check_csrf(&form) {
        return response;
    }
    let (app, redirect, scopes) = match authorization(&ctx, &form).await {
        Ok(values) => values,
        Err(response) => return response,
    };
    // The grant keeps the callback as given: the code exchange compares it byte for byte.
    let mut target = url::Url::parse(&redirect).expect("validated redirect");
    if field(&form, "decision") != "approve" {
        target
            .query_pairs_mut()
            .append_pair("error", "access_denied")
            .append_pair("state", field(&form, "state"));
        return Redirect::to(target.as_str()).into_response();
    }
    let Some(session) = codeyard_api_http::session::extract_session_id(&ctx.headers) else {
        return ctx.error(StatusCode::UNAUTHORIZED, "Sign in again");
    };
    let value = codeyard_support::crypto::random_hex();
    let grant = Grant {
        hash: token_hash(&value),
        client_id: app.client_id,
        user_code: codeyard_support::crypto::random_hex(),
        scopes,
        expires_at: chrono::Utc::now().timestamp() + 600,
        interval: 5,
        last_poll: 0,
        user_id: ctx.user.as_ref().map(|u| u.id),
        session_id: Some(session),
        denied: false,
        kind: "web".into(),
        redirect_uri: redirect,
        state: field(&form, "state").to_owned(),
        challenge: field(&form, "code_challenge").to_owned(),
    };
    if codeyard_db_identity::oauth_apps::save_grant(&ctx.state.pool, &grant)
        .await
        .is_err()
    {
        return ctx.error(StatusCode::SERVICE_UNAVAILABLE, "Authorization unavailable");
    }
    target
        .query_pairs_mut()
        .append_pair("code", &value)
        .append_pair("state", field(&form, "state"));
    Redirect::to(target.as_str()).into_response()
}

async fn applications(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    uri: Uri,
) -> Response {
    let ctx = match PageContext::load(state, headers, &uri).await {
        Ok(ctx) => ctx,
        Err(response) => return response,
    };
    if let Some(response) = login(&ctx) {
        return response;
    }
    let page = ctx
        .query
        .get("page")
        .and_then(|value| value.parse::<i64>().ok())
        .unwrap_or(1)
        .max(1);
    let size = codeyard_db_identity::oauth_apps::APPLICATION_PAGE_SIZE;
    let actor = ctx.database_actor(ctx.user.as_ref().unwrap().id);
    match codeyard_db_identity::oauth_apps::applications(
        &ctx.state.pool,
        actor,
        page.saturating_sub(1).saturating_mul(size),
        size + 1,
    )
    .await
    {
        Ok(Some(mut apps)) => {
            let more = apps.len() > size as usize;
            apps.truncate(size as usize);
            ctx.page_model(
                "OAuth applications",
                Page::Identity(account::Page::OAuthApplications {
                    applications: apps
                        .into_iter()
                        .map(|app| (app.name, app.client_id, app.redirect_uri))
                        .collect(),
                    previous: (page > 1)
                        .then(|| format!("/settings/applications?page={}", page - 1)),
                    next: more
                        .then(|| format!("/settings/applications?page={}", page.saturating_add(1))),
                }),
            )
        }
        Ok(None) => ctx.error(StatusCode::UNAUTHORIZED, "Sign in again"),
        Err(_) => ctx.error(StatusCode::SERVICE_UNAVAILABLE, "Applications unavailable"),
    }
}

async fn register_get(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    uri: Uri,
) -> Response {
    let ctx = match PageContext::load(state, headers, &uri).await {
        Ok(ctx) => ctx,
        Err(response) => return response,
    };
    if let Some(response) = login(&ctx) {
        return response;
    }
    ctx.page_model(
        "Register OAuth application",
        Page::Identity(account::Page::OAuthRegister {
            csrf_token: ctx.csrf_token.clone(),
            credentials: None,
        }),
    )
}

async fn register_post(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    uri: Uri,
    Form(form): Form<Fields>,
) -> Response {
    let ctx = match PageContext::load(state, headers, &uri).await {
        Ok(ctx) => ctx,
        Err(response) => return response,
    };
    if let Some(response) = login(&ctx) {
        return response;
    }
    if let Err(response) = ctx.check_csrf(&form) {
        return response;
    }
    let name = field(&form, "name").trim();
    if name.is_empty()
        || name.len() > 100
        || name.contains('\0')
        || !codeyard_identity::oauth_apps::valid_url(field(&form, "homepage"))
        || !codeyard_identity::oauth_apps::valid_url(field(&form, "redirect_uri"))
    {
        return ctx.error(
            StatusCode::UNPROCESSABLE_ENTITY,
            "Supply a name, homepage URL and callback URL",
        );
    }
    let Some(session) = codeyard_api_http::session::extract_session_id(&ctx.headers) else {
        return ctx.error(StatusCode::UNAUTHORIZED, "Sign in again");
    };
    let id = codeyard_support::crypto::random_hex();
    let secret = codeyard_support::crypto::random_hex();
    match codeyard_db_identity::oauth_apps::register(
        &ctx.state.pool,
        ctx.user.as_ref().unwrap().id,
        &session,
        &id,
        &token_hash(&secret),
        name,
        field(&form, "homepage"),
        field(&form, "redirect_uri"),
    )
    .await
    {
        Ok(Some(_)) => ctx.page_model(
            "OAuth application registered",
            Page::Identity(account::Page::OAuthRegister {
                csrf_token: ctx.csrf_token.clone(),
                credentials: Some((id, secret)),
            }),
        ),
        Ok(None) => ctx.error(StatusCode::FORBIDDEN, "Account unavailable"),
        Err(_) => ctx.error(StatusCode::SERVICE_UNAVAILABLE, "Registration unavailable"),
    }
}
