//! Safe HTML, authorized page-model and error response construction.
use super::context::PageContext;
use super::origin::safe_redirect_path;
use crate::backend::app::AppState;
use crate::webui::app::Page;
use crate::webui::document::{self, Chrome};
use crate::webui::pages::repos::chrome::RepoChrome;
use axum::{
    body::Body,
    extract::{OriginalUri, State},
    http::{header, HeaderMap, HeaderValue, Method, StatusCode},
    response::{IntoResponse, Redirect, Response},
};
use dioxus::prelude::*;
use std::sync::Arc;

const CSP: &str = "default-src 'none'; script-src 'self' 'wasm-unsafe-eval'; connect-src 'self'; style-src 'self' 'unsafe-inline'; img-src 'self' https: http:; font-src 'self'; form-action 'self'; base-uri 'none'; object-src 'none'; frame-ancestors 'none'";

impl PageContext {
    /// Render the IDENTICAL safe App props for initial SSR and browser hydration,
    /// or return those authorized props for standard hydrated navigation.
    pub fn page_model(&self, title: &str, page: crate::webui::app::Page) -> Response {
        let status = match &page {
            crate::webui::app::Page::Error { status, .. } => StatusCode::from_u16(*status)
                .ok()
                .filter(|status| status.is_client_error() || status.is_server_error())
                .unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
            _ => StatusCode::OK,
        };
        self.page_model_status(status, title, page)
    }

    /// Submitted non-secret values can be re-rendered with a real validation or
    /// conflict status, without overriding an infrastructure/render failure.
    pub fn page_model_status(
        &self,
        status: StatusCode,
        title: &str,
        page: crate::webui::app::Page,
    ) -> Response {
        let mut envelope = crate::webui::app::PageEnvelope {
            title: title.to_string(),
            location: self.location.clone(),
            chrome: self.chrome(),
            page,
        };
        if crate::backend::api::ci::jobs::collection::bound_log_projection(&mut envelope).is_err() {
            return self.error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "Unable to encode this page.",
            );
        }
        if accepts_page_model(&self.headers) {
            let json = match serde_json::to_vec(&envelope) {
                Ok(json) => json,
                Err(_) => {
                    return self.error(
                        StatusCode::INTERNAL_SERVER_ERROR,
                        "Unable to encode this page.",
                    )
                }
            };
            let response = Response::builder()
                .status(status)
                .header(
                    header::CONTENT_TYPE,
                    crate::webui::bootstrap::MODEL_CONTENT_TYPE,
                )
                .body(Body::from(json))
                .expect("static page model response headers");
            return self.with_cookie(secure_html_headers(response));
        }
        let runtime_module = match super::static_files::runtime_module() {
            Ok(module) => module,
            Err(()) => return self.error(StatusCode::SERVICE_UNAVAILABLE, "The interactive application assets are unavailable. Please try again after the server bundle is repaired."),
        };
        let html = match document::render_model(envelope, runtime_module) {
            Ok(html) => html,
            Err(_) => {
                return self.error(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "Unable to render this page.",
                )
            }
        };
        let mut response = Response::builder()
            .status(status)
            .header(header::CONTENT_TYPE, "text/html; charset=utf-8")
            .body(Body::from(html))
            .expect("static HTML response headers");
        response.headers_mut().insert(
            "x-codeyard-rendering",
            HeaderValue::from_static("ssr-hydrated"),
        );
        self.with_cookie(secure_html_headers(response))
    }

    pub fn error(&self, status: StatusCode, message: &str) -> Response {
        self.error_page(status, message, None)
    }

    /// An error about a repository this viewer can read. The page keeps the repository
    /// header and tabs, so the viewer can get back to it.
    pub fn repo_error(&self, repo: &RepoChrome, status: StatusCode, message: &str) -> Response {
        self.error_page(status, message, Some(repo.clone()))
    }

    fn error_page(&self, status: StatusCode, message: &str, repo: Option<RepoChrome>) -> Response {
        let signed_in = self.user.is_some();
        if accepts_page_model(&self.headers) {
            let page = Page::Error {
                status: status.as_u16(),
                message: message.into(),
                repo: repo.clone().map(Box::new),
                signed_in,
            };
            if let Some(response) =
                error_model_response(status, page, self.location.clone(), self.chrome())
            {
                return self.with_cookie(response);
            }
        }
        let body =
            crate::webui::pages::error::render(status.as_u16(), message, repo.as_ref(), signed_in);
        self.with_cookie(html_response(
            status,
            status.canonical_reason().unwrap_or("Request failed"),
            self.chrome(),
            repo,
            body,
        ))
    }

    pub fn redirect(&self, path: &str) -> Response {
        self.with_cookie(redirect_response(path))
    }
}

pub(crate) fn accepts_page_model(headers: &HeaderMap) -> bool {
    headers
        .get(header::ACCEPT)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|accept| {
            accept.split(',').any(|part| {
                let mut fields = part.trim().split(';');
                fields.next() == Some(crate::webui::bootstrap::MODEL_CONTENT_TYPE)
                    && !fields.any(|field| field.trim() == "q=0")
            })
        })
}

pub(crate) fn redirect_response(path: &str) -> Response {
    let path = if safe_redirect_path(path) { path } else { "/" };
    let mut response = StatusCode::SEE_OTHER.into_response();
    response.headers_mut().insert(
        header::LOCATION,
        HeaderValue::from_str(path).unwrap_or_else(|_| HeaderValue::from_static("/")),
    );
    secure_html_headers(response)
}
pub(crate) fn secure_html_headers(mut response: Response) -> Response {
    let h = response.headers_mut();
    h.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("no-store, private"),
    );
    h.append(header::VARY, HeaderValue::from_static("Accept, Cookie"));
    h.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(CSP),
    );
    h.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    h.insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("same-origin"),
    );
    h.insert(header::X_FRAME_OPTIONS, HeaderValue::from_static("DENY"));
    response
}
fn html_response(
    status: StatusCode,
    title: &str,
    chrome: Chrome,
    repo: Option<RepoChrome>,
    body: Element,
) -> Response {
    let html = document::render(title, chrome, repo, body);
    let response = Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, "text/html; charset=utf-8")
        .body(Body::from(html))
        .expect("static HTML response headers");
    secure_html_headers(response)
}
// Do not recurse through page_model_status: this also serves serialization and
// asset failures from that function. Native requests retain the safe HTML path.
pub(crate) fn error_model_response(
    status: StatusCode,
    page: Page,
    location: String,
    chrome: Chrome,
) -> Option<Response> {
    let envelope = crate::webui::app::PageEnvelope {
        title: status.canonical_reason().unwrap_or("Request failed").into(),
        location,
        chrome,
        page,
    };
    let json = serde_json::to_vec(&envelope).ok()?;
    let response = Response::builder()
        .status(status)
        .header(
            header::CONTENT_TYPE,
            crate::webui::bootstrap::MODEL_CONTENT_TYPE,
        )
        .body(Body::from(json))
        .ok()?;
    Some(secure_html_headers(response))
}

pub(crate) fn public_error(status: StatusCode, message: &str) -> Response {
    html_response(
        status,
        status.canonical_reason().unwrap_or("Request failed"),
        Chrome::default(),
        None,
        crate::webui::pages::error::render(status.as_u16(), message, None, false),
    )
}

/// Unmatched addresses get the 404 page. A page address typed with a trailing slash, such as
/// `/alice/demo/`, is sent to the same address without it, as GitHub does.
pub(crate) async fn not_found(
    State(state): State<Arc<AppState>>,
    method: Method,
    headers: HeaderMap,
    OriginalUri(uri): OriginalUri,
) -> Response {
    let path = uri.path();
    if matches!(method, Method::GET | Method::HEAD) && path.len() > 1 && path.ends_with('/') {
        let trimmed = path.trim_end_matches('/');
        let target = match uri.query() {
            Some(query) => format!("{trimmed}?{query}"),
            None => trimmed.to_owned(),
        };
        // Doubled slashes would make a protocol-relative address.
        if !target.is_empty() && !target.starts_with("//") {
            return Redirect::permanent(&target).into_response();
        }
    }
    match PageContext::load(state, headers, &uri).await {
        Ok(ctx) => ctx.error(StatusCode::NOT_FOUND, "Check the address and try again."),
        Err(response) => response,
    }
}
