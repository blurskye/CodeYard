//! A request for the old name of a repository that moved to an owner in another cell is
//! redirected to its current name, as GitHub does after a transfer: 301 for reads, 307 for
//! writes so clients repeat the method and body. Only requests that found nothing look,
//! including anonymous Git requests, which get a credential challenge for unknown names.
use axum::extract::{Request, State};
use axum::http::{header, Method, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use codeyard_app::app::AppState;
use std::sync::Arc;

pub(crate) fn redirect(
    State(state): State<Arc<AppState>>,
    request: Request,
    next: Next,
) -> futures_util::future::BoxFuture<'static, Response> {
    Box::pin(async move {
        let method = request.method().clone();
        let uri = request.uri().clone();
        let response = next.run(request).await;
        let status = response.status();
        if !matches!(status, StatusCode::NOT_FOUND | StatusCode::UNAUTHORIZED) {
            return response;
        }
        let Some(old) = OldPath::parse(uri.path()) else {
            return response;
        };
        if status == StatusCode::UNAUTHORIZED && !old.is_git() {
            return response;
        }
        let Ok(Some((owner, name))) =
            codeyard_db_core::cells::repositories::redirect(&state.pool, old.owner, old.name).await
        else {
            return response;
        };
        let mut location = format!("{}{owner}/{name}{}{}", old.prefix, old.suffix, old.rest);
        if let Some(query) = uri.query() {
            location.push('?');
            location.push_str(query);
        }
        let status = if matches!(method, Method::GET | Method::HEAD) {
            StatusCode::MOVED_PERMANENTLY
        } else {
            StatusCode::TEMPORARY_REDIRECT
        };
        let Ok(value) = header::HeaderValue::from_str(&location) else {
            return response;
        };
        if old.prefix == API {
            let body = serde_json::json!({ "message": "Moved Permanently", "url": location });
            (status, [(header::LOCATION, value)], axum::Json(body)).into_response()
        } else {
            (status, [(header::LOCATION, value)]).into_response()
        }
    })
}

const API: &str = "/api/v3/repos/";

/// `/api/v3/repos/{owner}/{name}...` or a web or Git path `/{owner}/{name}[.git]...`.
struct OldPath<'a> {
    prefix: &'static str,
    owner: &'a str,
    name: &'a str,
    suffix: &'static str,
    rest: &'a str,
}

impl<'a> OldPath<'a> {
    fn is_git(&self) -> bool {
        self.prefix != API
            && ["/info/refs", "/git-upload-pack", "/git-receive-pack"].contains(&self.rest)
    }
    fn parse(path: &'a str) -> Option<Self> {
        let (prefix, path) = match path.strip_prefix(API) {
            Some(path) => (API, path),
            None if path.starts_with("/api/") => return None,
            None => ("/", path.strip_prefix('/')?),
        };
        let (owner, path) = path.split_once('/')?;
        let end = path.find('/').unwrap_or(path.len());
        let (name, rest) = path.split_at(end);
        let (name, suffix) = match name.strip_suffix(".git") {
            Some(name) if prefix != API => (name, ".git"),
            _ => (name, ""),
        };
        let valid = |part: &str| {
            !part.is_empty()
                && part.len() <= 100
                && part
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
        };
        (valid(owner) && valid(name)).then_some(Self {
            prefix,
            owner,
            name,
            suffix,
            rest,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::OldPath;

    #[test]
    fn old_paths_keep_their_shape_and_reject_other_routes() {
        let api = OldPath::parse("/api/v3/repos/alice/project/issues/7").unwrap();
        assert_eq!(
            (api.prefix, api.owner, api.name, api.suffix, api.rest),
            ("/api/v3/repos/", "alice", "project", "", "/issues/7")
        );
        let git = OldPath::parse("/alice/project.git/info/refs").unwrap();
        assert_eq!(
            (git.prefix, git.owner, git.name, git.suffix, git.rest),
            ("/", "alice", "project", ".git", "/info/refs")
        );
        assert!(git.is_git() && !api.is_git());
        let web = OldPath::parse("/alice/project").unwrap();
        assert_eq!((web.owner, web.name, web.rest), ("alice", "project", ""));
        assert!(!web.is_git());
        assert!(OldPath::parse("/api/v3/users/alice/repos").is_none());
        assert!(OldPath::parse("/alice").is_none());
        assert!(OldPath::parse("/alice/%2e%2e").is_none());
        assert!(OldPath::parse("/alice//x").is_none());
    }
}
