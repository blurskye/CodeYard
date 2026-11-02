//! Primary REST limits use persistent counters, including anonymous requests.
use super::{auth, Error, Result};
use crate::backend::{
    api::http::session,
    app::AppState,
    db::api_limits::{self, Budget},
};
use axum::{
    extract::Request,
    http::{HeaderMap, StatusCode},
    middleware::Next,
    response::{IntoResponse, Response},
    routing::{get, MethodRouter},
    Extension, Json,
};
use serde_json::{json, Value};
use std::sync::Arc;

#[derive(Clone)]
struct Requester {
    key: String,
    authenticated: bool,
}

impl Requester {
    fn limit(&self, resource: &str) -> (i64, i64) {
        match (resource, self.authenticated) {
            ("search", true) => (30, 60),
            ("search", false) => (10, 60),
            (_, true) => (5000, 3600),
            (_, false) => (60, 3600),
        }
    }
}

async fn budget(
    state: &AppState,
    requester: &Requester,
    resource: &str,
    consume: bool,
) -> Result<(Budget, bool)> {
    let (limit, seconds) = requester.limit(resource);
    api_limits::budget(
        &state.pool,
        api_limits::Request {
            key: &requester.key,
            resource,
            now: chrono::Utc::now().timestamp(),
            limit,
            seconds,
            consume,
        },
    )
    .await
    .map_err(Error::unavailable)
}

fn headers(budget: &Budget, limit: i64, resource: &str) -> HeaderMap {
    let mut headers = HeaderMap::new();
    for (name, value) in [
        ("x-ratelimit-limit", limit),
        ("x-ratelimit-remaining", (limit - budget.used).max(0)),
        ("x-ratelimit-used", budget.used),
        ("x-ratelimit-reset", budget.reset_at),
    ] {
        headers.insert(name, value.to_string().parse().expect("numeric rate limit"));
    }
    headers.insert(
        "x-ratelimit-resource",
        resource.parse().expect("REST resource"),
    );
    headers
}

pub(super) async fn enforce(
    Extension(state): Extension<Arc<AppState>>,
    mut request: Request,
    next: Next,
) -> Response {
    let checked = async {
        let path = request
            .uri()
            .path()
            .strip_prefix("/api/v3")
            .unwrap_or(request.uri().path());
        // JWT and Basic credentials are validated by the application endpoint, never as PATs.
        // A valid app JWT has its app's own budget. Anything else there consumes the anonymous
        // IP budget; no credential bypasses limits.
        let (auth, app) = if auth::application_credentials(path) {
            let app = match auth::credential(request.headers()) {
                Some(value) => {
                    crate::backend::identity::github_apps::authenticate(&state.pool, value)
                        .await
                        .ok()
                        .flatten()
                }
                None => None,
            };
            (None, app)
        } else {
            (
                auth::authenticate(&state, request.headers(), false).await?,
                None,
            )
        };
        let requester = Requester {
            authenticated: auth.is_some() || app.is_some(),
            key: match (auth, app) {
                (Some(auth), _) => format!("user:{}", auth.user.id),
                (None, Some(app)) => format!("app:{}", app.id),
                (None, None) => format!(
                    "ip:{}",
                    session::client_address(request.headers()).unwrap_or("unknown")
                ),
            },
        };
        let path = request
            .uri()
            .path()
            .strip_prefix("/api/v3")
            .unwrap_or(request.uri().path());
        let resource = if path.starts_with("/search/") {
            "search"
        } else {
            "core"
        };
        let consume = !(path == "/rate_limit" && auth::read_method(request.method()));
        let (budget, allowed) = budget(&state, &requester, resource, consume).await?;
        let headers = headers(&budget, requester.limit(resource).0, resource);
        if !allowed {
            let mut response = (
                headers,
                Error::new(StatusCode::FORBIDDEN, "API rate limit exceeded"),
            )
                .into_response();
            let wait = (budget.reset_at - chrono::Utc::now().timestamp()).max(1);
            response.headers_mut().insert(
                "retry-after",
                wait.to_string().parse().expect("numeric wait"),
            );
            return Ok::<_, Error>((None, Some(response)));
        }
        request.extensions_mut().insert(requester);
        Ok((Some(headers), None))
    }
    .await;
    match checked {
        Err(error) => error.into_response(),
        Ok((_, Some(response))) => response,
        Ok((Some(headers), None)) => {
            let mut response = next.run(request).await;
            response.headers_mut().extend(headers);
            response
        }
        Ok((None, None)) => Error::unavailable(()).into_response(),
    }
}

fn document(value: &Budget, limit: i64) -> Value {
    json!({"limit":limit,"remaining":(limit-value.used).max(0),"used":value.used,"reset":value.reset_at})
}

async fn get_limits(
    Extension(state): Extension<Arc<AppState>>,
    Extension(requester): Extension<Requester>,
) -> Result<Json<Value>> {
    let (core, _) = budget(&state, &requester, "core", false).await?;
    let (search, _) = budget(&state, &requester, "search", false).await?;
    let core = document(&core, requester.limit("core").0);
    Ok(Json(
        json!({"resources":{"core":core,"search":document(&search,requester.limit("search").0)},"rate":core}),
    ))
}

pub(super) fn handlers() -> Vec<(&'static str, MethodRouter<Arc<AppState>>)> {
    vec![("rate-limit/get", get(get_limits))]
}
