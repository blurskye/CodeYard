//! Primary REST limits use persistent counters, including anonymous requests.
use axum::response::IntoResponse;

use axum::extract::Request;
use axum::http::HeaderMap;
use axum::http::StatusCode;
use axum::middleware::Next;
use axum::response::Response;
use axum::routing::get;
use axum::routing::MethodRouter;
use axum::Extension;
use axum::Json;
use codeyard_app::app::AppState;
use codeyard_db_identity::api_limits::Budget;
use codeyard_model::github_error::Error;
use codeyard_model::github_error::Result;
use serde_json::json;
use serde_json::Value;
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
    codeyard_db_identity::api_limits::budget(
        &state.pool,
        codeyard_db_identity::api_limits::Request {
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

pub(crate) async fn enforce(
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
        let parts: Vec<_> = path.trim_start_matches('/').split('/').collect();
        let logs = crate::auth::read_method(request.method())
            && matches!(
                parts.as_slice(),
                ["repos", _, _, "actions", "runs" | "jobs", _, "logs"]
                    | ["repos", _, _, "actions", "runs", _, "attempts", _, "logs"]
                    | ["repos", _, _, "actions", "jobs", _, "steps", _, "logs"]
            );
        let no_authorization = !request
            .headers()
            .contains_key(axum::http::header::AUTHORIZATION);
        let mut key = None;
        let authentication = if crate::auth::application_credentials(path) {
            if crate::apps::client_credentials(path) {
                let app = crate::apps::application(&state, request.headers(), parts[1]).await?;
                key = Some(format!("oauth-app:{}", app.id));
            } else if let Some(value) =
                codeyard_identity::github_auth::credential(request.headers())
            {
                if let Some(app) = codeyard_identity::github_apps::authenticate(&state.pool, value)
                    .await
                    .map_err(Error::unavailable)?
                {
                    key = Some(format!("app:{}", app.id));
                }
            }
            None
        } else if logs && no_authorization {
            // Only log downloads/polling accept sessions and capabilities; all other REST
            // routes retain token-only authentication. The handler validates the capability.
            let capability = request.uri().query().is_some_and(|query| {
                url::form_urlencoded::parse(query.as_bytes())
                    .any(|(key, _)| key == "codeyard_download")
            });
            if !capability && codeyard_api_http::session::has_session_cookie(request.headers()) {
                let user =
                    codeyard_api_http::session::get_current_user(&state.pool, request.headers())
                        .await
                        .map_err(|(status, axum::Json(error))| Error::new(status, error.message))?;
                key = Some(format!("user:{}", user.id));
            }
            None
        } else {
            codeyard_identity::github_auth::authenticate(&state, request.headers(), false).await?
        };
        if let Some(auth) = authentication {
            key = Some(if auth.is_job() {
                let job = codeyard_db_actions::ci_tokens::get(&state.pool, auth.token.id)
                    .await
                    .map_err(Error::unavailable)?
                    .ok_or_else(codeyard_identity::github_auth::required)?;
                format!("job-repository:{}", job.repo_id)
            } else {
                format!("user:{}", auth.user.id)
            });
        }
        let requester = Requester {
            authenticated: key.is_some(),
            key: key.unwrap_or_else(|| {
                format!(
                    "ip:{}",
                    codeyard_identity::client_address::client_address(request.headers())
                        .unwrap_or("unknown")
                )
            }),
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
        let consume = !(path == "/rate_limit" && crate::auth::read_method(request.method()));
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

pub(crate) fn handlers() -> Vec<(&'static str, MethodRouter<Arc<AppState>>)> {
    vec![("rate-limit/get", get(get_limits))]
}
