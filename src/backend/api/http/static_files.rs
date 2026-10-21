//! Embedded native assets + contained dx-generated runtime files. Never a SPA fallback.
use axum::{
    body::Body,
    http::{header, HeaderMap, StatusCode, Uri},
    response::{IntoResponse, Response},
};
use sha2::{Digest, Sha256};
use std::{
    path::{Path, PathBuf},
    sync::OnceLock,
};

struct RuntimeBundle {
    root: PathBuf,
    module: String,
}
static RUNTIME: OnceLock<RuntimeBundle> = OnceLock::new();

/// An asset compiled into the binary. Its ETag is hashed on first use.
struct Embedded {
    path: &'static str,
    bytes: &'static [u8],
    mime: &'static str,
    etag: OnceLock<String>,
}

const fn embedded(path: &'static str, bytes: &'static [u8], mime: &'static str) -> Embedded {
    Embedded {
        path,
        bytes,
        mime,
        etag: OnceLock::new(),
    }
}

static EMBEDDED: [Embedded; 8] = [
    embedded(
        "/assets/logo.svg",
        include_bytes!("../../../webui/assets/logo.svg"),
        "image/svg+xml",
    ),
    embedded(
        "/assets/logo.webp",
        include_bytes!("../../../webui/assets/logo.webp"),
        "image/webp",
    ),
    embedded(
        "/assets/favicon.png",
        include_bytes!("../../../webui/assets/favicon.png"),
        "image/png",
    ),
    embedded(
        "/assets/tailwind.css",
        include_bytes!("../../../webui/assets/tailwind.css"),
        "text/css; charset=utf-8",
    ),
    embedded(
        "/assets/actions.css",
        include_bytes!("../../../webui/assets/actions.css"),
        "text/css; charset=utf-8",
    ),
    embedded(
        "/assets/native.css",
        include_bytes!("../../../webui/components/native.css"),
        "text/css; charset=utf-8",
    ),
    embedded(
        "/assets/fonts/mona-sans.woff2",
        include_bytes!("../../../webui/assets/fonts/mona-sans.woff2"),
        "font/woff2",
    ),
    embedded(
        "/assets/fonts/OFL.txt",
        include_bytes!("../../../webui/assets/fonts/OFL.txt"),
        "text/plain; charset=utf-8",
    ),
];

static DOWNLOADS: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(16);

fn runtime() -> Result<&'static RuntimeBundle, ()> {
    if let Some(runtime) = RUNTIME.get() {
        return Ok(runtime);
    }
    let root = std::env::var_os("WEBUI_DIST")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("target/dx/codeyard/release/web/public"));
    let root = std::fs::canonicalize(root).map_err(|_| ())?;
    let index_path = root.join("index.html");
    let metadata = std::fs::metadata(&index_path).map_err(|_| ())?;
    if metadata.len() > 1024 * 1024 {
        return Err(());
    }
    let index = std::fs::read_to_string(index_path).map_err(|_| ())?;
    let module = generated_module(&index).ok_or(())?;
    let file = std::fs::canonicalize(root.join(module.trim_start_matches('/'))).map_err(|_| ())?;
    if !file.starts_with(&root) || !file.is_file() {
        return Err(());
    }
    Ok(RUNTIME.get_or_init(|| RuntimeBundle { root, module }))
}

/// Read only the dx-generated EXTERNAL module URL, never its CSR document/body.
/// dx 0.7.10 appends auto-initialization to generated glue, so no custom loader JS.
fn generated_module(index: &str) -> Option<String> {
    for script in index.split("<script").skip(1) {
        let tag = script.split_once('>')?.0;
        if !(tag.contains("type=\"module\"") || tag.contains("type='module'")) {
            continue;
        }
        let src = attribute(tag, "src")?;
        if !src.starts_with('/') || src.starts_with("//") || src.contains('\\') || src.contains('%')
        {
            return None;
        }
        let base = url::Url::parse("https://generated.invalid/").ok()?;
        let url = base.join(src).ok()?;
        if url.origin() != base.origin() || url.query().is_some() || url.fragment().is_some() {
            return None;
        }
        let path = url.path(); // normalize dx's /./assets/... prefix
        if runtime_relative(path).is_some() && path.ends_with(".js") {
            return Some(path.into());
        }
    }
    None
}
fn attribute<'a>(tag: &'a str, name: &str) -> Option<&'a str> {
    let tail = tag.split_once(&format!("{name}="))?.1;
    let quote = tail.chars().next()?;
    if !matches!(quote, '\'' | '"') {
        return None;
    }
    tail[1..].split_once(quote).map(|(value, _)| value)
}
pub(crate) fn runtime_module() -> Result<&'static str, ()> {
    runtime().map(|runtime| runtime.module.as_str())
}

pub async fn asset(uri: Uri, headers: HeaderMap) -> Response {
    if let Some(file) = EMBEDDED.iter().find(|file| file.path == uri.path()) {
        let etag = file
            .etag
            .get_or_init(|| format!("W/\"{:x}\"", Sha256::digest(file.bytes)));
        return asset_response(Body::from(file.bytes), file.mime, etag, &headers);
    }
    match runtime() {
        Ok(runtime) => serve_runtime(&runtime.root, &uri, &headers).await,
        Err(_) => StatusCode::NOT_FOUND.into_response(),
    }
}
fn runtime_relative(path: &str) -> Option<&str> {
    if !(path.starts_with("/assets/") || path.starts_with("/wasm/"))
        || path.contains('%')
        || path.contains('\\')
        || path.chars().any(|c| c.is_control())
        || path.split('/').any(|segment| matches!(segment, "." | ".."))
    {
        return None;
    }
    if !(path.ends_with(".js") || path.ends_with(".wasm")) {
        return None;
    }
    Some(path.trim_start_matches('/'))
}
async fn serve_runtime(root: &Path, uri: &Uri, headers: &HeaderMap) -> Response {
    let Some(relative) = runtime_relative(uri.path()) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let file = match tokio::fs::canonicalize(root.join(relative)).await {
        Ok(file) if file.starts_with(root) => file,
        _ => return StatusCode::NOT_FOUND.into_response(),
    };
    let metadata = match tokio::fs::metadata(&file).await {
        Ok(metadata) if metadata.is_file() && metadata.len() <= 64 * 1024 * 1024 => metadata,
        Ok(_) => return StatusCode::NOT_FOUND.into_response(),
        Err(_) => return StatusCode::SERVICE_UNAVAILABLE.into_response(),
    };
    let Ok(modified) = metadata.modified().and_then(|time| {
        time.duration_since(std::time::UNIX_EPOCH)
            .map_err(std::io::Error::other)
    }) else {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    };
    let version = format!("{:x}-{:x}", metadata.len(), modified.as_nanos());
    // Include change time so replacing a bundle while preserving mtimes invalidates it.
    #[cfg(unix)]
    let version = {
        use std::os::unix::fs::MetadataExt;
        format!(
            "{version}-{:x}-{:x}",
            metadata.ctime(),
            metadata.ctime_nsec()
        )
    };
    let etag = format!("W/\"{version}\"");
    let mime = if relative.ends_with(".wasm") {
        "application/wasm"
    } else {
        "text/javascript; charset=utf-8"
    };
    if not_modified(headers, &etag) {
        return asset_response(Body::empty(), mime, &etag, headers);
    }
    let Ok(permit) = DOWNLOADS.try_acquire() else {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    };
    let file = match tokio::fs::File::open(&file).await {
        Ok(file) => file,
        Err(_) => return StatusCode::SERVICE_UNAVAILABLE.into_response(),
    };
    let stream = tokio_util::io::ReaderStream::with_capacity(file, 64 * 1024);
    let stream =
        futures_util::stream::try_unfold((stream, permit), |(mut stream, permit)| async move {
            use futures_util::StreamExt;
            stream
                .next()
                .await
                .transpose()
                .map(|chunk| chunk.map(|chunk| (chunk, (stream, permit))))
        });
    asset_response(
        super::middleware::download_body(stream),
        mime,
        &etag,
        headers,
    )
}

fn not_modified(headers: &HeaderMap, etag: &str) -> bool {
    headers
        .get_all(header::IF_NONE_MATCH)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .any(|value| {
            value.split(',').map(str::trim).any(|candidate| {
                candidate == "*"
                    || candidate.strip_prefix("W/").unwrap_or(candidate)
                        == etag.trim_start_matches("W/")
            })
        })
}

fn asset_response(body: Body, mime: &'static str, etag: &str, headers: &HeaderMap) -> Response {
    let unchanged = not_modified(headers, etag);
    Response::builder()
        .status(if unchanged { StatusCode::NOT_MODIFIED } else { StatusCode::OK })
        .header(header::CONTENT_TYPE, mime)
        // Weak validators remain valid across the router's content encodings.
        .header(header::ETAG, etag)
        .header(header::VARY, "accept-encoding")
        // Revalidate unversioned native assets and generated filenames alike;
        // immutable may be enabled only after delivery proves hashed provenance.
        .header(header::CACHE_CONTROL, "public, max-age=0, must-revalidate")
        .header(header::X_CONTENT_TYPE_OPTIONS, "nosniff")
        .header(
            header::CONTENT_SECURITY_POLICY,
            "default-src 'self'; script-src 'self' 'wasm-unsafe-eval'; connect-src 'self'; object-src 'none'; base-uri 'none'; frame-ancestors 'none'",
        )
        .body(if unchanged { Body::empty() } else { body })
        .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{body::to_bytes, routing::get, Router};
    use tower::ServiceExt;

    #[tokio::test]
    async fn native_assets_have_correct_mime_and_preserved_bytes() {
        for (path, mime) in [
            ("/assets/logo.svg", "image/svg+xml"),
            ("/assets/logo.webp", "image/webp"),
            ("/assets/favicon.png", "image/png"),
            ("/assets/tailwind.css", "text/css; charset=utf-8"),
            ("/assets/actions.css", "text/css; charset=utf-8"),
            ("/assets/fonts/mona-sans.woff2", "font/woff2"),
        ] {
            let response = asset(path.parse().unwrap(), HeaderMap::new()).await;
            assert_eq!(response.status(), StatusCode::OK);
            assert_eq!(response.headers()[header::CONTENT_TYPE], mime);
            assert_eq!(
                response.headers()[header::X_CONTENT_TYPE_OPTIONS],
                "nosniff"
            );
            let bytes = to_bytes(response.into_body(), 8 * 1024 * 1024)
                .await
                .unwrap();
            assert!(!bytes.is_empty());
            match path {
                "/assets/logo.svg" => {
                    assert_eq!(&bytes[..], include_bytes!("../../../webui/assets/logo.svg"));
                }
                "/assets/logo.webp" => {
                    assert_eq!(
                        &bytes[..],
                        include_bytes!("../../../webui/assets/logo.webp")
                    );
                }
                "/assets/favicon.png" => {
                    assert_eq!(
                        &bytes[..],
                        include_bytes!("../../../webui/assets/favicon.png")
                    );
                }
                _ => {}
            }
        }
    }
    #[tokio::test]
    async fn missing_traversal_and_documents_never_fall_back_to_html() {
        for path in [
            "/assets/missing.css",
            "/assets/../Cargo.toml",
            "/assets/%2e%2e/Cargo.toml",
            "/assets/index.html",
            "/unknown",
            "/api/missing",
        ] {
            assert_eq!(
                asset(path.parse().unwrap(), HeaderMap::new())
                    .await
                    .status(),
                StatusCode::NOT_FOUND,
                "{path}"
            );
        }
    }
    #[tokio::test]
    async fn head_has_headers_and_no_body() {
        let app = Router::new().route("/assets/*path", get(asset));
        let response = app
            .oneshot(
                axum::http::Request::builder()
                    .method("HEAD")
                    .uri("/assets/logo.webp")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()[header::CONTENT_TYPE], "image/webp");
        assert!(to_bytes(response.into_body(), 1024)
            .await
            .unwrap()
            .is_empty());
    }
    #[tokio::test]
    async fn assets_revalidate_get_head_and_changed_runtime_files() {
        use axum::http::Request;
        use tower_http::compression::CompressionLayer;

        let temp = tempfile::tempdir().unwrap();
        std::fs::create_dir(temp.path().join("assets")).unwrap();
        let file = temp.path().join("assets/runtime.js");
        std::fs::write(&file, "/* unchanged */\n".repeat(200)).unwrap();
        let root = temp.path().to_owned();
        let app = Router::new()
            .route(
                "/assets/runtime.js",
                get(move |uri: Uri, headers: HeaderMap| {
                    let root = root.clone();
                    async move { serve_runtime(&root, &uri, &headers).await }
                }),
            )
            .route("/assets/*path", get(asset))
            .layer(CompressionLayer::new());
        let mut runtime_tag = String::new();
        for path in [
            "/assets/native.css",
            "/assets/fonts/mona-sans.woff2",
            "/assets/runtime.js",
        ] {
            let response = app
                .clone()
                .oneshot(Request::builder().uri(path).body(Body::empty()).unwrap())
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            let etag = response.headers()[header::ETAG]
                .to_str()
                .unwrap()
                .to_owned();
            assert!(etag.starts_with("W/\""));
            let body = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
            assert!(!body.is_empty());
            for method in ["GET", "HEAD"] {
                for condition in [
                    etag.clone(),
                    etag.trim_start_matches("W/").to_owned(),
                    format!("\"stale\", {etag}"),
                    "*".into(),
                ] {
                    let response = app
                        .clone()
                        .oneshot(
                            Request::builder()
                                .method(method)
                                .uri(path)
                                .header(header::IF_NONE_MATCH, condition)
                                .header(header::ACCEPT_ENCODING, "gzip")
                                .body(Body::empty())
                                .unwrap(),
                        )
                        .await
                        .unwrap();
                    assert_eq!(
                        response.status(),
                        StatusCode::NOT_MODIFIED,
                        "{method} {path}"
                    );
                    assert_eq!(response.headers()[header::ETAG], etag);
                    assert_eq!(
                        response.headers()[header::CACHE_CONTROL],
                        "public, max-age=0, must-revalidate"
                    );
                    assert!(response.headers()[header::VARY]
                        .to_str()
                        .unwrap()
                        .contains("accept-encoding"));
                    assert!(to_bytes(response.into_body(), 1024)
                        .await
                        .unwrap()
                        .is_empty());
                }
            }
            let response = app
                .clone()
                .oneshot(
                    Request::builder()
                        .uri(path)
                        .header(header::IF_NONE_MATCH, "\"stale\"")
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            assert_eq!(response.headers()[header::ETAG], etag);
            assert_eq!(
                to_bytes(response.into_body(), 1024 * 1024).await.unwrap(),
                body
            );
            if path.ends_with(".js") {
                runtime_tag = etag;
            }
        }
        // A compressed response uses the same weak validator as the identity representation.
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/assets/runtime.js")
                    .header(header::ACCEPT_ENCODING, "gzip")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.headers()[header::CONTENT_ENCODING], "gzip");
        assert_eq!(response.headers()[header::ETAG], runtime_tag);

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o000)).unwrap();
            // chmod changes the validator; obtain the new one without reading the file.
            let response = app
                .clone()
                .oneshot(
                    Request::builder()
                        .uri("/assets/runtime.js")
                        .header(header::IF_NONE_MATCH, "*")
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::NOT_MODIFIED);
            let tag = response.headers()[header::ETAG].clone();
            let response = app
                .clone()
                .oneshot(
                    Request::builder()
                        .uri("/assets/runtime.js")
                        .header(header::IF_NONE_MATCH, tag)
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::NOT_MODIFIED);
            std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o600)).unwrap();
        }
        std::fs::write(&file, "/* changed!! */\n".repeat(200)).unwrap();
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/assets/runtime.js")
                    .header(header::IF_NONE_MATCH, &runtime_tag)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_ne!(response.headers()[header::ETAG], runtime_tag);
        assert_eq!(
            to_bytes(response.into_body(), 1024 * 1024).await.unwrap(),
            "/* changed!! */\n".repeat(200)
        );
        std::fs::remove_file(file).unwrap();
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/assets/runtime.js")
                    .header(header::IF_NONE_MATCH, "*")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn runtime_downloads_stream_large_files() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::create_dir(temp.path().join("wasm")).unwrap();
        std::fs::write(temp.path().join("wasm/test.wasm"), vec![0; 2 * 1024 * 1024]).unwrap();
        let uri = "/wasm/test.wasm".parse().unwrap();
        let response = serve_runtime(temp.path(), &uri, &HeaderMap::new()).await;
        assert_eq!(response.status(), StatusCode::OK);
        use axum::body::HttpBody;
        assert_eq!(response.body().size_hint().lower(), 0);
        assert_eq!(
            to_bytes(response.into_body(), 3 * 1024 * 1024)
                .await
                .unwrap()
                .len(),
            2 * 1024 * 1024
        );
    }

    #[test]
    fn generated_loader_requires_external_same_origin_module() {
        assert_eq!(
            generated_module(
                "<script type=\"module\" async src=\"/./assets/codeyard-123.js\"></script>"
            ),
            Some("/assets/codeyard-123.js".into())
        );
        for index in [
            "<script>evil()</script>",
            "<script type=\"module\" src=\"//evil.test/app.js\"></script>",
            "<script type=\"module\" src=\"/assets/%2e%2e/app.js\"></script>",
        ] {
            assert!(generated_module(index).is_none());
        }
    }
    #[tokio::test]
    async fn contained_framework_js_wasm_and_snippets_are_served_without_index() {
        let temp = tempfile::TempDir::new().unwrap();
        std::fs::create_dir_all(temp.path().join("assets/snippets")).unwrap();
        std::fs::create_dir_all(temp.path().join("wasm")).unwrap();
        std::fs::write(
            temp.path().join("assets/framework.js"),
            "/* generated fixture */",
        )
        .unwrap();
        std::fs::write(temp.path().join("wasm/framework_bg.wasm"), b"\0asm").unwrap();
        std::fs::write(temp.path().join("assets/snippets/glue.js"), "/* fixture */").unwrap();
        let root = std::fs::canonicalize(temp.path()).unwrap();
        for (path, mime) in [
            ("/assets/framework.js", "text/javascript; charset=utf-8"),
            ("/wasm/framework_bg.wasm", "application/wasm"),
            ("/assets/snippets/glue.js", "text/javascript; charset=utf-8"),
        ] {
            let response = serve_runtime(&root, &path.parse().unwrap(), &HeaderMap::new()).await;
            assert_eq!(response.status(), StatusCode::OK);
            assert_eq!(response.headers()[header::CONTENT_TYPE], mime);
        }
        for path in [
            "/assets/../framework.js",
            "/assets/%2e%2e/framework.js",
            "/assets/missing.js",
            "/index.html",
        ] {
            assert_eq!(
                serve_runtime(&root, &path.parse().unwrap(), &HeaderMap::new())
                    .await
                    .status(),
                StatusCode::NOT_FOUND
            );
        }
        #[cfg(unix)]
        {
            let outside = tempfile::NamedTempFile::new().unwrap();
            std::os::unix::fs::symlink(outside.path(), root.join("assets/escape.js")).unwrap();
            assert_eq!(
                serve_runtime(
                    &root,
                    &"/assets/escape.js".parse().unwrap(),
                    &HeaderMap::new()
                )
                .await
                .status(),
                StatusCode::NOT_FOUND
            );
        }
    }
}
