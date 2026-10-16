//! Same-origin submission and local redirect policy shared by HTTP adapters.
use axum::http::{header, HeaderMap};

pub(crate) fn same_origin_submission(headers: &HeaderMap) -> bool {
    if let Some(site) = headers.get("sec-fetch-site") {
        if !matches!(site.to_str().ok(), Some("same-origin" | "none")) {
            return false;
        }
    }
    // Never trust X-Forwarded-Host/Proto from clients. PUBLIC_URL can pin the
    // externally visible origin for a proxy deployment; otherwise compare Host.
    let source = headers
        .get(header::ORIGIN)
        .or_else(|| headers.get(header::REFERER));
    let Some(source) = source else {
        return true;
    }; // synchronizer token is still mandatory
    let Ok(source) = source.to_str() else {
        return false;
    };
    let Ok(origin) = url::Url::parse(source) else {
        return false;
    };
    if !matches!(origin.scheme(), "http" | "https")
        || !origin.username().is_empty()
        || origin.password().is_some()
    {
        return false;
    }
    if let Ok(public_url) = std::env::var("PUBLIC_URL") {
        return url::Url::parse(&public_url).is_ok_and(|target| target.origin() == origin.origin());
    }
    let Some(host) = headers.get(header::HOST).and_then(|v| v.to_str().ok()) else {
        return false;
    };
    url::Url::parse(&format!("{}://{host}", origin.scheme())).is_ok_and(|target| {
        target.path() == "/"
            && target.query().is_none()
            && target.fragment().is_none()
            && target.username().is_empty()
            && target.password().is_none()
            && target.origin() == origin.origin()
    })
}

/// Restrict redirects to local absolute paths. Encoded delimiters are refused in the path;
/// the query and fragment are data and pass through unchanged.
pub fn safe_redirect_path(path: &str) -> bool {
    if !path.starts_with('/')
        || path.starts_with("//")
        || path.chars().any(|c| c.is_control() || c == '\\')
    {
        return false;
    }
    let route = path.split(['?', '#']).next().unwrap_or_default();
    let lower = route.to_ascii_lowercase();
    !lower.starts_with("/%2f")
        && !["%5c", "%0d", "%0a", "%00"]
            .iter()
            .any(|bad| lower.contains(bad))
}
