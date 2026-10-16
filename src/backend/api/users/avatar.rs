//! One address for every account picture: `/avatars/NAME`. An account with an uploaded picture
//! is sent to that file, whose own visibility rules apply. Any other name, including a deleted
//! account's, gets a generated square with the first letter, so a timeline never has a gap.

use crate::backend::{app::AppState, db};
use axum::{
    extract::{Path, State},
    http::{header, HeaderValue, StatusCode},
    response::{IntoResponse, Redirect, Response},
};
use std::sync::Arc;

/// Dark enough that the white letter stays readable on a light or a dark page.
const COLORS: [&str; 8] = [
    "#0969da", "#1a7f37", "#9a6700", "#bc4c00", "#cf222e", "#8250df", "#bf3989", "#57606a",
];

const CACHE: &str = "public, max-age=30";

/// The generated picture for `name`: the same colour and letter every time.
pub(crate) fn default_picture(name: &str) -> String {
    let hash = name
        .to_lowercase()
        .bytes()
        .fold(0xcbf29ce484222325u64, |hash, byte| {
            (hash ^ u64::from(byte)).wrapping_mul(0x100000001b3)
        });
    let color = COLORS[(hash % COLORS.len() as u64) as usize];
    let letter = name
        .chars()
        .next()
        .map_or_else(|| "?".to_owned(), |first| first.to_uppercase().to_string());
    let letter = letter
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;");
    format!(
        "<svg xmlns=\"http://www.w3.org/2000/svg\" viewBox=\"0 0 64 64\" width=\"64\" height=\"64\">\
<rect width=\"64\" height=\"64\" rx=\"12\" fill=\"{color}\"/>\
<text x=\"32\" y=\"32\" fill=\"#ffffff\" font-family=\"ui-sans-serif, system-ui, sans-serif\" \
font-size=\"32\" font-weight=\"600\" text-anchor=\"middle\" dominant-baseline=\"central\">{letter}</text></svg>"
    )
}

pub(crate) async fn avatar_get(
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
) -> Response {
    let pool = &state.pool;
    // Users and organizations share one name space; a name that is no user may be an organization.
    let file_id = match db::users::accounts::get_user_by_username(pool, &name).await {
        Ok(Some(user)) => user.avatar_file_id,
        Ok(None) => db::organizations::get_public_organization(pool, &name)
            .await
            .ok()
            .flatten()
            .and_then(|org| org.avatar_file_id),
        Err(_) => None,
    };
    let mut response = match file_id {
        Some(id) => Redirect::temporary(&format!("/api/files/{id}/download")).into_response(),
        None => {
            let mut response = default_picture(&name).into_response();
            let headers = response.headers_mut();
            headers.insert(
                header::CONTENT_TYPE,
                HeaderValue::from_static("image/svg+xml"),
            );
            headers.insert(
                header::CONTENT_SECURITY_POLICY,
                HeaderValue::from_static("default-src 'none'; style-src 'unsafe-inline'"),
            );
            response
        }
    };
    if response.status() == StatusCode::OK || response.status().is_redirection() {
        let headers = response.headers_mut();
        headers.insert(header::CACHE_CONTROL, HeaderValue::from_static(CACHE));
        headers.insert(
            header::X_CONTENT_TYPE_OPTIONS,
            HeaderValue::from_static("nosniff"),
        );
    }
    response
}

#[cfg(test)]
mod tests {
    use super::default_picture;

    #[test]
    fn a_name_always_gets_the_same_colour_and_its_first_letter() {
        let first = default_picture("alice");
        assert_eq!(first, default_picture("alice"));
        assert_eq!(first, default_picture("Alice"));
        assert!(first.contains(">A</text>"));
        assert_ne!(default_picture("alice"), default_picture("bob"));
    }

    #[test]
    fn odd_names_are_escaped_and_non_ascii_letters_are_kept() {
        assert!(default_picture("<script>").contains("&lt;"));
        assert!(!default_picture("<script>").contains("<script>"));
        assert!(default_picture("émile").contains(">É</text>"));
        assert!(default_picture("").contains(">?</text>"));
    }
}
