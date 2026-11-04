use crate::backend::api::http::context::PageContext;
use crate::backend::api::http::origin::safe_redirect_path;
use crate::backend::api::http::render::accepts_page_model;
use crate::backend::api::http::render::secure_html_headers;
use crate::backend::api::http::session::secure_cookie;
use crate::backend::app::AppState;
use axum::{
    body::Body,
    extract::{Form, OriginalUri, State},
    http::{header, HeaderMap, HeaderValue, StatusCode},
    response::Response,
};
use codeyard_webui::document::Theme;
use std::{collections::HashMap, sync::Arc};

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
