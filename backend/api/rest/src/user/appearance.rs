use axum::body::Body;
use axum::extract::Form;
use axum::extract::OriginalUri;
use axum::extract::State;
use axum::http::header;
use axum::http::HeaderMap;
use axum::http::HeaderValue;
use axum::http::StatusCode;
use axum::response::Response;
use codeyard_api_http::context::PageContext;
use codeyard_api_http::origin::safe_redirect_path;
use codeyard_api_http::render::accepts_page_model;
use codeyard_api_http::render::secure_html_headers;
use codeyard_api_http::session::secure_cookie;
use codeyard_app::app::AppState;
use codeyard_webui::document::Theme;
use std::collections::HashMap;
use std::sync::Arc;

pub(crate) async fn set_theme(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    OriginalUri(uri): OriginalUri,
    Form(form): Form<HashMap<String, String>>,
) -> Response {
    let ctx = match PageContext::load(state, headers, &uri).await {
        Ok(ctx) => ctx,
        Err(response) => return response,
    };
    if let Err(response) = ctx.check_theme_form(&form) {
        return response;
    }
    let Some(theme) = form.get("theme").and_then(|v| Theme::parse(v)) else {
        return ctx.error(
            StatusCode::BAD_REQUEST,
            "Choose system, light, or dark appearance.",
        );
    };
    let target = form
        .get("return_to")
        .map(String::as_str)
        .filter(|v| safe_redirect_path(v))
        .unwrap_or("/");
    let mut response = if accepts_page_model(&ctx.headers) {
        let saved = codeyard_webui::components::ThemeSaved { theme };
        let json = match serde_json::to_vec(&saved) {
            Ok(json) => json,
            Err(_) => {
                return ctx.error(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "Unable to confirm the appearance update.",
                )
            }
        };
        ctx.with_cookie(secure_html_headers(
            Response::builder()
                .status(StatusCode::OK)
                .header(
                    header::CONTENT_TYPE,
                    codeyard_webui::bootstrap::THEME_CONTENT_TYPE,
                )
                .body(Body::from(json))
                .expect("static theme response headers"),
        ))
    } else {
        ctx.redirect(target)
    };
    let cookie = format!(
        "theme={}; Path=/; Max-Age=31536000; HttpOnly; SameSite=Lax{}",
        theme.as_str(),
        if secure_cookie() { "; Secure" } else { "" }
    );
    response.headers_mut().append(
        header::SET_COOKIE,
        HeaderValue::from_str(&cookie).expect("validated theme cookie"),
    );
    response
}
