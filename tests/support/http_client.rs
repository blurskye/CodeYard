//! A signed-in user talking to the application's JSON API and HTML forms, in process. The
//! shortcuts that name a repository (`save`, `branch`, `open_pull`, `pull`) use `alice/proj`.
use crate::support::native_database as native;
use axum::{
    body::{to_bytes, Body},
    http::{header, Request, StatusCode},
    Router,
};
use base64::Engine;
use codeyard::backend::db;
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

    /// The id a redirect names after `marker`, such as `#review-` or `#thread-`.
    pub fn id_after(&self, marker: &str) -> i64 {
        let location = self.location.as_deref().expect("a redirect");
        location
            .rsplit_once(marker)
            .unwrap_or_else(|| panic!("{marker} in {location}"))
            .1
            .parse()
            .unwrap_or_else(|_| panic!("{marker} in {location}"))
    }
}

/// The page without the comments Dioxus writes between text nodes, so a count or a label reads
/// as one piece of text next to its tag.
pub fn visible_text(html: &str) -> String {
    let mut out = String::with_capacity(html.len());
    let mut rest = html;
    while let Some(start) = rest.find("<!--") {
        out.push_str(&rest[..start]);
        rest = rest[start..].split_once("-->").map_or("", |(_, tail)| tail);
    }
    out.push_str(rest);
    out
}

/// The value of the first form field called `name` on the page, if it has one.
pub fn field_value(html: &str, name: &str) -> Option<String> {
    let at = html.find(&format!("name=\"{name}\""))?;
    let start = html[..at].rfind('<')?;
    let end = at + html[at..].find('>')?;
    let tag = &html[start..=end];
    let value = tag.find("value=\"")? + "value=\"".len();
    Some(tag[value..].split('"').next()?.to_owned())
}

/// The value of the form field called `name`; the page has to have it.
pub fn hidden_value(html: &str, name: &str) -> String {
    field_value(html, name).unwrap_or_else(|| panic!("no {name} field with a value: {html}"))
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
        self.save_text(branch, file, &format!("{file}\n"), tip)
            .await
    }

    /// Saves `content` as a new file `file` on `branch` and returns the new tip.
    pub async fn save_text(&self, branch: &str, file: &str, content: &str, tip: &str) -> String {
        let content = base64::engine::general_purpose::STANDARD.encode(content);
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

    /// Overwrites `file` on `branch` with `content` and returns the new tip.
    pub async fn update_text(&self, branch: &str, file: &str, content: &str, tip: &str) -> String {
        let content = base64::engine::general_purpose::STANDARD.encode(content);
        let saved = self
            .expect(
                "PUT",
                &format!("/api/repos/alice/proj/contents/{file}"),
                json!({"content": content, "message": format!("Change {file}"), "branch": branch, "expected_revision": tip}),
                StatusCode::OK,
            )
            .await;
        saved["commit"]["sha"].as_str().unwrap().to_owned()
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

    /// An HTML page that has to answer 200. Use it where the test shows the page lacks
    /// something, so an error page cannot pass for it.
    pub async fn page_ok(&self, path: &str) -> String {
        let (status, html) = self.page(path).await;
        assert_eq!(status, StatusCode::OK, "{path}: {html}");
        html
    }

    /// A page as typed data, the way the browser loads it on navigation. Answers that are not
    /// pages, such as redirects, come back through `exchange`.
    pub async fn model(&self, path: &str) -> (StatusCode, codeyard::webui::app::Page) {
        let mut request = Request::builder().uri(path).header(
            header::ACCEPT,
            codeyard::webui::bootstrap::MODEL_CONTENT_TYPE,
        );
        if !self.session.is_empty() {
            request = request.header(header::COOKIE, format!("session_id={}", self.session));
        }
        let response = self
            .router
            .clone()
            .oneshot(request.body(Body::empty()).unwrap())
            .await
            .unwrap();
        let status = response.status();
        assert_eq!(
            response.headers()[header::CONTENT_TYPE],
            codeyard::webui::bootstrap::MODEL_CONTENT_TYPE,
            "{path}"
        );
        let body = to_bytes(response.into_body(), 8 * 1024 * 1024)
            .await
            .unwrap();
        let envelope: codeyard::webui::app::PageEnvelope = serde_json::from_slice(&body)
            .unwrap_or_else(|error| panic!("{path}: not a page model: {error}"));
        (status, envelope.page)
    }

    /// Posts an HTML form together with this session's CSRF token and keeps the whole
    /// reply, so tests can read the redirect target.
    pub async fn form_reply(&self, path: &str, pairs: &[(&str, &str)]) -> Reply {
        let mut body = url::form_urlencoded::Serializer::new(String::new());
        body.append_pair("csrf_token", &self.csrf);
        // Comment forms carry the repository identity rendered in the page.
        if path.contains("/pulls/")
            && path.split('?').next().unwrap().ends_with("/comments")
            && !pairs.iter().any(|(key, _)| *key == "repository_id")
        {
            let page_path = path.split("/comments").next().unwrap();
            let (_, page) = self.page(page_path).await;
            if let Some(value) = field_value(&page, "repository_id") {
                body.append_pair("repository_id", &value);
            }
        }
        // New-issue forms carry the repository identity and the rendered template revision.
        if path.ends_with("/issues/new") {
            let template = pairs
                .iter()
                .find(|(name, _)| *name == "template")
                .map_or("", |(_, value)| *value);
            let (_, page) = self
                .page(&format!(
                    "{path}?template={}",
                    codeyard::webui::encode_component(template)
                ))
                .await;
            for name in ["repository_id", "template_revision"] {
                if !pairs.iter().any(|(key, _)| *key == name) {
                    if let Some(value) = field_value(&page, name) {
                        body.append_pair(name, &value);
                    }
                }
            }
        }
        let path_only = path.split('?').next().unwrap();
        if path_only.ends_with("/milestones/new")
            || (path_only.contains("/issues/") && path_only.ends_with("/comments"))
        {
            let page_path = path_only.strip_suffix("/comments").unwrap_or(path_only);
            let (_, page) = self.page(page_path).await;
            for name in ["repository_id", "issue_id"] {
                if !pairs.iter().any(|(key, _)| *key == name) {
                    if let Some(value) = field_value(&page, name) {
                        body.append_pair(name, &value);
                    }
                }
            }
        }
        if path_only.contains("/issues/")
            && path_only.ends_with("/edit")
            && !pairs.iter().any(|(key, _)| *key == "expected_body")
        {
            if let Some((issue, comment)) = path_only.split_once("/comments/") {
                let comment_id = comment.trim_end_matches("/edit").parse::<i64>().unwrap();
                let (status, comments) = self
                    .api(
                        "GET",
                        &format!("/api/repos{issue}/comments?per_page=100"),
                        Value::Null,
                    )
                    .await;
                if status == StatusCode::OK {
                    if let Some(original) = comments
                        .as_array()
                        .unwrap()
                        .iter()
                        .find(|value| value["id"].as_i64() == Some(comment_id))
                        .and_then(|value| value["body"].as_str())
                    {
                        body.append_pair(
                            "expected_body",
                            &serde_json::to_string(original).unwrap(),
                        );
                    }
                }
            }
        }
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
