//! A signed-in user talking to the application's JSON API and HTML forms, for the
//! pull request tests. The including test declares `db` and `native` (see
//! support/native_database.rs).
#![allow(dead_code)]
use super::{db, native};
use axum::{
    body::{to_bytes, Body},
    http::{header, Request, StatusCode},
    Router,
};
use base64::Engine;
use serde_json::{json, Value};
use tower::ServiceExt;

/// What came back: the status, the redirect target if any, and the body.
pub struct Reply {
    pub status: StatusCode,
    pub location: Option<String>,
    pub body: Vec<u8>,
}

impl Reply {
    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }
}

/// One signed-in user talking to the application.
pub struct Client {
    pub router: Router,
    pub session: String,
    pub csrf: String,
}

impl Client {
    pub async fn login(router: &Router, pool: &db::DbPool, user: i64) -> Self {
        let session = native::token();
        let csrf = native::token();
        db::users::sessions::create_session(
            pool,
            &session,
            user,
            None,
            None,
            chrono::Utc::now() + chrono::Duration::hours(1),
        )
        .await
        .unwrap();
        assert_eq!(
            db::users::sessions::get_or_create_csrf_token(pool, &session, Some(user), &csrf, 3600)
                .await
                .unwrap(),
            Some(csrf.clone())
        );
        Self {
            router: router.clone(),
            session,
            csrf,
        }
    }

    /// A visitor without a session cookie.
    pub fn anonymous(router: &Router) -> Self {
        Self {
            router: router.clone(),
            session: String::new(),
            csrf: native::token(),
        }
    }

    /// Sends any body and keeps the parts of the answer tests look at.
    pub async fn exchange(
        &self,
        method: &str,
        path: &str,
        content_type: &str,
        body: Vec<u8>,
    ) -> Reply {
        let mut request = Request::builder()
            .method(method)
            .uri(path)
            .header("x-csrf-token", &self.csrf)
            .header(header::CONTENT_TYPE, content_type);
        if !self.session.is_empty() {
            request = request.header(header::COOKIE, format!("session_id={}", self.session));
        }
        let response = self
            .router
            .clone()
            .oneshot(request.body(Body::from(body)).unwrap())
            .await
            .unwrap();
        let status = response.status();
        let location = response
            .headers()
            .get(header::LOCATION)
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned);
        let body = to_bytes(response.into_body(), 8 * 1024 * 1024)
            .await
            .unwrap()
            .to_vec();
        Reply {
            status,
            location,
            body,
        }
    }

    pub async fn send(
        &self,
        method: &str,
        path: &str,
        content_type: &str,
        body: String,
    ) -> (StatusCode, String) {
        let reply = self
            .exchange(method, path, content_type, body.into_bytes())
            .await;
        (
            reply.status,
            String::from_utf8_lossy(&reply.body).into_owned(),
        )
    }

    /// Posts a `multipart/form-data` form: these text fields, then an optional file
    /// part named `picture`.
    pub async fn multipart(
        &self,
        path: &str,
        fields: &[(&str, &str)],
        picture: Option<(&str, &[u8])>,
    ) -> Reply {
        let boundary = "----pull-request-test-boundary";
        let mut body = Vec::new();
        for (name, value) in fields {
            body.extend_from_slice(
                format!("--{boundary}\r\nContent-Disposition: form-data; name=\"{name}\"\r\n\r\n{value}\r\n")
                    .as_bytes(),
            );
        }
        if let Some((content_type, bytes)) = picture {
            body.extend_from_slice(
                format!("--{boundary}\r\nContent-Disposition: form-data; name=\"picture\"; filename=\"me\"\r\nContent-Type: {content_type}\r\n\r\n")
                    .as_bytes(),
            );
            body.extend_from_slice(bytes);
            body.extend_from_slice(b"\r\n");
        }
        body.extend_from_slice(format!("--{boundary}--\r\n").as_bytes());
        self.exchange(
            "POST",
            path,
            &format!("multipart/form-data; boundary={boundary}"),
            body,
        )
        .await
    }

    /// A JSON request; the answer is parsed (null when it has no JSON body).
    pub async fn api(&self, method: &str, path: &str, body: Value) -> (StatusCode, Value) {
        let (status, text) = self
            .send(method, path, "application/json", body.to_string())
            .await;
        let value = if status == StatusCode::NO_CONTENT {
            assert!(
                text.is_empty(),
                "{method} {path}: unexpected 204 body: {text}"
            );
            Value::Null
        } else {
            serde_json::from_str(&text)
                .unwrap_or_else(|error| panic!("{method} {path}: invalid JSON: {error}: {text}"))
        };
        (status, value)
    }

    /// A JSON request that has to answer with `expected`.
    pub async fn expect(
        &self,
        method: &str,
        path: &str,
        body: Value,
        expected: StatusCode,
    ) -> Value {
        let (status, value) = self.api(method, path, body).await;
        assert_eq!(status, expected, "{method} {path}: {value}");
        value
    }

    /// Saves a new file on `branch` through the editor API and returns the new tip.
    pub async fn save(&self, branch: &str, file: &str, tip: &str) -> String {
        let content = base64::engine::general_purpose::STANDARD.encode(format!("{file}\n"));
        let saved = self
            .expect(
                "POST",
                &format!("/api/repos/alice/proj/contents/{file}"),
                json!({"content": content, "message": format!("Add {file}"), "branch": branch, "expected_revision": tip}),
                StatusCode::CREATED,
            )
            .await;
        saved["sha"].as_str().unwrap().to_owned()
    }

    pub async fn branch(&self, name: &str, sha: &str) {
        self.expect(
            "POST",
            "/api/repos/alice/proj/branches",
            json!({"name": name, "sha": sha}),
            StatusCode::OK,
        )
        .await;
    }

    pub async fn open_pull(&self, title: &str, head: &str, base: &str) -> Value {
        self.expect(
            "POST",
            "/api/repos/alice/proj/pulls",
            json!({"title": title, "head_branch": head, "base_branch": base}),
            StatusCode::CREATED,
        )
        .await
    }

    pub async fn open_draft(&self, title: &str, head: &str, base: &str) -> Value {
        self.expect(
            "POST",
            "/api/repos/alice/proj/pulls",
            json!({"title": title, "head_branch": head, "base_branch": base, "is_draft": true}),
            StatusCode::CREATED,
        )
        .await
    }

    /// An HTML page, as the browser would get it.
    pub async fn page(&self, path: &str) -> (StatusCode, String) {
        self.send("GET", path, "text/html", String::new()).await
    }

    /// Posts an HTML form together with this session's CSRF token and keeps the whole
    /// reply, so tests can read the redirect target.
    pub async fn form_reply(&self, path: &str, pairs: &[(&str, &str)]) -> Reply {
        let mut body = url::form_urlencoded::Serializer::new(String::new());
        body.append_pair("csrf_token", &self.csrf);
        for (name, value) in pairs {
            body.append_pair(name, value);
        }
        self.exchange(
            "POST",
            path,
            "application/x-www-form-urlencoded",
            body.finish().into_bytes(),
        )
        .await
    }

    /// Posts an HTML form together with this session's CSRF token.
    pub async fn form(&self, path: &str, pairs: &[(&str, &str)]) -> (StatusCode, String) {
        let reply = self.form_reply(path, pairs).await;
        (reply.status, reply.text())
    }

    pub async fn pull(&self, number: i64) -> Value {
        self.expect(
            "GET",
            &format!("/api/repos/alice/proj/pulls/{number}"),
            Value::Null,
            StatusCode::OK,
        )
        .await
    }

    /// Merges with the commits the pull request records right now.
    pub async fn merge_current(&self, number: i64) -> Value {
        let pull = self.pull(number).await;
        self.expect(
            "POST",
            &format!("/api/repos/alice/proj/pulls/{number}/merge"),
            json!({"expected_head": pull["head_sha"], "expected_base": pull["base_sha"]}),
            StatusCode::OK,
        )
        .await
    }
}

pub fn text<'a>(value: &'a Value, key: &str) -> &'a str {
    value[key]
        .as_str()
        .unwrap_or_else(|| panic!("{key} in {value}"))
}

pub fn moved_forward(before: &Value, after: &Value) -> bool {
    let parse = |value: &Value| {
        chrono::DateTime::parse_from_rfc3339(text(value, "updated_at")).expect("RFC 3339 time")
    };
    parse(after) > parse(before)
}
