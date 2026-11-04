//! Safe HTML, authorized page-model and error response construction.
use crate::context::PageContext;
use crate::origin::safe_redirect_path;
use axum::body::Body;
use axum::extract::OriginalUri;
use axum::extract::State;
use axum::http::header;
use axum::http::HeaderMap;
use axum::http::HeaderValue;
use axum::http::Method;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::response::Redirect;
use axum::response::Response;
use codeyard_app::app::AppState;
use codeyard_webui::app::Page;
use codeyard_webui::document;
use codeyard_webui::document::Chrome;
use codeyard_webui::pages::repos::chrome::RepoChrome;
use dioxus::prelude::*;
use std::sync::Arc;

const CSP: &str = "default-src 'none'; script-src 'self' 'wasm-unsafe-eval'; connect-src 'self'; style-src 'self' 'unsafe-inline'; img-src 'self' https: http:; font-src 'self'; form-action 'self'; base-uri 'none'; object-src 'none'; frame-ancestors 'none'";

impl PageContext {
    /// Render the IDENTICAL safe App props for initial SSR and browser hydration,
    /// or return those authorized props for standard hydrated navigation.
    pub fn page_model(&self, title: &str, page: codeyard_webui::app::Page) -> Response {
        let status = match &page {
            codeyard_webui::app::Page::Error { status, .. } => StatusCode::from_u16(*status)
                .ok()
                .filter(|status| status.is_client_error() || status.is_server_error())
                .unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
            _ => StatusCode::OK,
        };
        self.page_model_status(status, title, page)
    }

    /// A page whose form posts here and is then redirected to `target`, such as an OAuth
    /// consent page. Browsers check that redirect against the page's form-action, so the
    /// policy allows the target, and the page always loads as a full document because a
    /// hydrated page keeps the policy of the document it was loaded in.
    pub fn page_model_with_form_target(&self, title: &str, page: Page, target: &str) -> Response {
        if accepts_page_model(&self.headers) {
            let response = Response::builder()
                .header(header::CONTENT_TYPE, "text/html; charset=utf-8")
                .body(Body::empty())
                .expect("static HTML response headers");
            return secure_html_headers(response);
        }
        let mut response = self.page_model(title, page);
        let source = url::Url::parse(target).ok().map(|url| match url.scheme() {
            "http" | "https" => url.origin().ascii_serialization(),
            scheme => format!("{scheme}:"),
        });
        if let Some(policy) = source
            .map(|source| {
                CSP.replace(
                    "form-action 'self'",
                    &format!("form-action 'self' {source}"),
                )
            })
            .and_then(|policy| HeaderValue::from_str(&policy).ok())
        {
            response
                .headers_mut()
                .insert(header::CONTENT_SECURITY_POLICY, policy);
        }
        response
    }

    /// Submitted non-secret values can be re-rendered with a real validation or
    /// conflict status, without overriding an infrastructure/render failure.
    pub fn page_model_status(
        &self,
        status: StatusCode,
        title: &str,
        page: codeyard_webui::app::Page,
    ) -> Response {
        let observation = crate::traffic::page(self, title, &page);
        let mut envelope = codeyard_webui::app::PageEnvelope {
            title: title.to_string(),
            location: self.location.clone(),
            chrome: self.chrome(),
            page,
        };
        if crate::shared::ci::responses::bound_log_projection(&mut envelope).is_err()
            || crate::shared::repos::pulls::diff::bound_thread_projection(&mut envelope).is_err()
        {
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
                    codeyard_webui::bootstrap::MODEL_CONTENT_TYPE,
                )
                .body(Body::from(json))
                .expect("static page model response headers");
            let mut response = self.with_cookie(secure_html_headers(response));
            if let Some(observation) = observation {
                response.extensions_mut().insert(observation);
            }
            return response;
        }
        let runtime_module = match crate::static_files::runtime_module() {
            Ok(module) => module,
            Err(()) => return self.error(StatusCode::SERVICE_UNAVAILABLE, "The interactive application assets are unavailable. Please try again after the server bundle is repaired."),
        };
        let mut response = Response::builder()
            .status(status)
            .header(header::CONTENT_TYPE, "text/html; charset=utf-8")
            .body(Body::empty())
            .expect("static HTML response headers");
        response.extensions_mut().insert(ModelRender {
            envelope,
            runtime_module,
        });
        response.headers_mut().insert(
            "x-codeyard-rendering",
            HeaderValue::from_static("ssr-hydrated"),
        );
        let mut response = self.with_cookie(secure_html_headers(response));
        if let Some(observation) = observation {
            response.extensions_mut().insert(observation);
        }
        response
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
        let body = codeyard_webui::pages::error::render(
            status.as_u16(),
            message,
            repo.as_ref(),
            signed_in,
        );
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

#[derive(Clone)]
struct ModelRender {
    envelope: codeyard_webui::app::PageEnvelope,
    runtime_module: &'static str,
}

static MODEL_RENDERS: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(4);
/// How long a page waits for a render slot before the server answers that it is busy.
const RENDER_WAIT: std::time::Duration = std::time::Duration::from_secs(10);
static RENDER_WAITERS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// A page waiting for a render slot. Each holds its whole page model, so their number is capped.
struct RenderWaiter;

impl RenderWaiter {
    fn join(limit: usize) -> Option<Self> {
        let earlier = RENDER_WAITERS.fetch_add(1, std::sync::atomic::Ordering::AcqRel);
        let waiter = Self;
        (earlier < limit).then_some(waiter)
    }
}

impl Drop for RenderWaiter {
    fn drop(&mut self) {
        RENDER_WAITERS.fetch_sub(1, std::sync::atomic::Ordering::AcqRel);
    }
}

async fn render_slot(waiters: usize) -> Option<tokio::sync::SemaphorePermit<'static>> {
    if let Ok(permit) = MODEL_RENDERS.try_acquire() {
        return Some(permit);
    }
    let _waiter = RenderWaiter::join(waiters)?;
    tokio::time::timeout(RENDER_WAIT, MODEL_RENDERS.acquire())
        .await
        .ok()?
        .ok()
}

pub async fn render_page(
    axum::Extension(config): axum::Extension<Arc<codeyard_model::http_config::WebConfig>>,
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    let head = request.method() == Method::HEAD;
    let mut response = next.run(request).await;
    if let Some(render) = response.extensions_mut().remove::<ModelRender>() {
        // A burst of page loads waits its turn; a slot that stays taken, or a full queue,
        // answers busy.
        let Some(permit) = render_slot(config.page_render_waiters).await else {
            return public_error(
                StatusCode::SERVICE_UNAVAILABLE,
                "Page rendering busy. Please try again.",
            );
        };
        let html = tokio::task::spawn_blocking(move || {
            let _permit = permit;
            document::render_model(render.envelope, render.runtime_module)
        })
        .await;
        match html {
            Ok(Ok(html)) => {
                // Axum sized the deferred empty body before this outer middleware ran.
                response
                    .headers_mut()
                    .insert(header::CONTENT_LENGTH, html.len().into());
                if !head {
                    *response.body_mut() = Body::from(html);
                }
            }
            _ => {
                return public_error(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "Unable to render this page.",
                )
            }
        }
    }
    response
}

pub fn accepts_page_model(headers: &HeaderMap) -> bool {
    headers
        .get(header::ACCEPT)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|accept| {
            accept.split(',').any(|part| {
                let mut fields = part.trim().split(';');
                fields.next() == Some(codeyard_webui::bootstrap::MODEL_CONTENT_TYPE)
                    && !fields.any(|field| {
                        field
                            .trim()
                            .strip_prefix("q=")
                            .and_then(|quality| quality.trim().parse::<f32>().ok())
                            .is_some_and(|quality| quality <= 0.0)
                    })
            })
        })
}

pub fn redirect_response(path: &str) -> Response {
    let path = if safe_redirect_path(path) { path } else { "/" };
    let mut response = StatusCode::SEE_OTHER.into_response();
    response.headers_mut().insert(
        header::LOCATION,
        HeaderValue::from_str(path).unwrap_or_else(|_| HeaderValue::from_static("/")),
    );
    secure_html_headers(response)
}
pub fn secure_html_headers(mut response: Response) -> Response {
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
    let envelope = codeyard_webui::app::PageEnvelope {
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
            codeyard_webui::bootstrap::MODEL_CONTENT_TYPE,
        )
        .body(Body::from(json))
        .ok()?;
    Some(secure_html_headers(response))
}

pub fn public_error(status: StatusCode, message: &str) -> Response {
    public_error_returning_to(status, message, String::new())
}

/// A public error page whose appearance switch returns to `return_to`, a page the handler
/// chose because the failing address itself may accept only POST.
pub(crate) fn public_error_returning_to(
    status: StatusCode,
    message: &str,
    return_to: String,
) -> Response {
    html_response(
        status,
        status.canonical_reason().unwrap_or("Request failed"),
        Chrome {
            return_to,
            ..Chrome::default()
        },
        None,
        codeyard_webui::pages::error::render(status.as_u16(), message, None, false),
    )
}

/// Unmatched addresses get the 404 page. A page address typed with a trailing slash, such as
/// `/alice/demo/`, is sent to the same address without it, as GitHub does.
pub async fn not_found(
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
        if codeyard_webui::navigation::safe_page_target(&target) {
            return Redirect::permanent(&target).into_response();
        }
    }
    match PageContext::load(state, headers, &uri).await {
        Ok(ctx) => ctx.error(StatusCode::NOT_FOUND, "Check the address and try again."),
        Err(response) => response,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tower::ServiceExt;

    fn config(
        page_render_waiters: usize,
    ) -> axum::Extension<Arc<codeyard_model::http_config::WebConfig>> {
        axum::Extension(Arc::new(codeyard_model::http_config::WebConfig {
            cors_origins: Vec::new(),
            trusted_proxies: Default::default(),
            page_render_waiters,
        }))
    }

    #[tokio::test]
    async fn model_rendering_waits_for_a_bounded_slot_without_blocking_http() {
        let permits = MODEL_RENDERS.acquire_many(4).await.unwrap();
        let page = axum::Router::new()
            .route(
                "/",
                axum::routing::get(|| async {
                    let mut response = Response::builder()
                        .status(StatusCode::CREATED)
                        .body(Body::empty())
                        .unwrap();
                    response.extensions_mut().insert(ModelRender {
                        envelope: codeyard_webui::app::PageEnvelope {
                            title: "Help".into(),
                            location: "/help".into(),
                            chrome: Chrome::default(),
                            page: Page::Help,
                        },
                        runtime_module: "/assets/app.js",
                    });
                    response
                }),
            )
            .layer(axum::middleware::from_fn(render_page));
        // With no room to wait, a page is answered busy at once instead of queueing.
        let full = page
            .clone()
            .layer(config(0))
            .oneshot(
                axum::http::Request::builder()
                    .uri("/")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(full.status(), StatusCode::SERVICE_UNAVAILABLE);
        let router = page.layer(config(64));
        let mut request = tokio::spawn(
            router.clone().oneshot(
                axum::http::Request::builder()
                    .uri("/")
                    .body(Body::empty())
                    .unwrap(),
            ),
        );
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(10), &mut request)
                .await
                .is_err()
        );
        drop(permits);
        let response = request.await.unwrap().unwrap();
        assert_eq!(response.status(), StatusCode::CREATED);
        let length = response.headers()[header::CONTENT_LENGTH]
            .to_str()
            .unwrap()
            .parse::<usize>()
            .unwrap();
        let body = axum::body::to_bytes(response.into_body(), 1024 * 1024)
            .await
            .unwrap();
        assert_eq!(length, body.len());
        assert!(String::from_utf8_lossy(&body).contains("Keyboard shortcuts"));
        let response = router
            .oneshot(
                axum::http::Request::builder()
                    .method(Method::HEAD)
                    .uri("/")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::CREATED);
        assert_eq!(
            response.headers()[header::CONTENT_LENGTH]
                .to_str()
                .unwrap()
                .parse::<usize>()
                .unwrap(),
            length
        );
        assert!(axum::body::to_bytes(response.into_body(), 1024 * 1024)
            .await
            .unwrap()
            .is_empty());
    }
}
