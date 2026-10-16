//! Authorized page-model transport and decoding.
#[cfg(any(target_arch = "wasm32", test))]
#[derive(Debug, PartialEq, Eq)]
pub(super) enum LoadError {
    InvalidTarget,
    TooLarge,
    InvalidModel,
    UnsafeLocation,
    Unavailable(u16),
    Timeout,
    Transport(&'static str),
}

#[cfg(any(target_arch = "wasm32", test))]
impl std::fmt::Display for LoadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidTarget => f.write_str("This navigation target is not safe."),
            Self::TooLarge => f.write_str("The page is too large for interactive navigation. Open it normally."),
            Self::InvalidModel => f.write_str("The server returned an incompatible page model. Reload this page."),
            Self::UnsafeLocation => f.write_str("The server returned an unsafe page location."),
            Self::Unavailable(status) => write!(f, "The server could not load this page (HTTP {status}). Your current content has been preserved."),
            Self::Timeout => f.write_str("The page request timed out. Your current content has been preserved."),
            Self::Transport(message) => f.write_str(message),
        }
    }
}

#[cfg(target_arch = "wasm32")]
pub(super) enum PageLoad {
    Model(crate::webui::app::PageEnvelope),
    Navigate(String),
}

#[cfg(target_arch = "wasm32")]
pub(super) async fn load_page(
    target: &str,
    signal: &web_sys::AbortSignal,
) -> Result<PageLoad, LoadError> {
    if !crate::webui::navigation::safe_page_target(target) {
        return Err(LoadError::InvalidTarget);
    }
    // The gloo-net 0.6 builder appends '&' to an existing query even when
    // no query parameters are added. The echoed model location then triggers
    // router.replace and another fetch. Build the browser request directly so
    // the complete, already-encoded target survives unchanged.
    let options = web_sys::RequestInit::new();
    options.set_credentials(web_sys::RequestCredentials::SameOrigin);
    options.set_signal(Some(signal));
    let request = web_sys::Request::new_with_str_and_init(target, &options)
        .map_err(|_| LoadError::Transport("The page request could not be prepared."))?;
    request
        .headers()
        .set("Accept", crate::webui::bootstrap::MODEL_CONTENT_TYPE)
        .map_err(|_| LoadError::Transport("The page request headers could not be prepared."))?;
    let response = gloo_net::http::Request::from(request)
        .send()
        .await
        .map_err(|_| {
            LoadError::Transport(
                "The page could not be loaded. Check your connection and try again.",
            )
        })?;
    decode_page_response(response).await
}

#[cfg(any(target_arch = "wasm32", test))]
#[derive(Debug, PartialEq, Eq)]
enum ResponseKind {
    Model,
    Navigate,
    Unavailable,
}

#[cfg(any(target_arch = "wasm32", test))]
fn response_kind(status: u16, content_type: &str) -> ResponseKind {
    if status >= 500 || matches!(status, 408 | 425 | 429) {
        ResponseKind::Unavailable
    } else if content_type.split(';').next().map(str::trim)
        == Some(crate::webui::bootstrap::MODEL_CONTENT_TYPE)
    {
        ResponseKind::Model
    } else {
        ResponseKind::Navigate
    }
}

#[cfg(any(target_arch = "wasm32", test))]
fn decode_page_model(text: &str) -> Result<crate::webui::app::PageEnvelope, LoadError> {
    if text.len() > crate::webui::bootstrap::MAX_PAGE_MODEL_BYTES {
        return Err(LoadError::TooLarge);
    }
    let envelope: crate::webui::app::PageEnvelope =
        serde_json::from_str(text).map_err(|_| LoadError::InvalidModel)?;
    if !crate::webui::navigation::safe_page_target(&envelope.location) {
        return Err(LoadError::UnsafeLocation);
    }
    Ok(envelope)
}

#[cfg(target_arch = "wasm32")]
async fn decode_page_response(response: gloo_net::http::Response) -> Result<PageLoad, LoadError> {
    let response_url = response.url();
    let location = local_response_target(&response_url)?;
    let content_type = response.headers().get("Content-Type").unwrap_or_default();
    match response_kind(response.status(), &content_type) {
        ResponseKind::Unavailable => return Err(LoadError::Unavailable(response.status())),
        // Permanent errors are real pages, not successful old content under a new
        // URL. Native HTML falls back to navigation; a typed error is decoded.
        ResponseKind::Navigate => return Ok(PageLoad::Navigate(location)),
        ResponseKind::Model => {}
    }
    let text = response
        .text()
        .await
        .map_err(|_| LoadError::Transport("The page response was interrupted."))?;
    decode_page_model(&text).map(PageLoad::Model)
}

#[cfg(target_arch = "wasm32")]
fn local_response_target(response_url: &str) -> Result<String, LoadError> {
    let current = web_sys::window()
        .ok_or(LoadError::Transport("Browser location unavailable."))?
        .location()
        .href()
        .map_err(|_| LoadError::Transport("Browser location unavailable."))?;
    let current =
        url::Url::parse(&current).map_err(|_| LoadError::Transport("Browser location invalid."))?;
    let response = url::Url::parse(response_url)
        .map_err(|_| LoadError::Transport("Response location invalid."))?;
    if current.origin() != response.origin() {
        return Err(LoadError::Transport(
            "The server redirected to another origin. Open the link normally.",
        ));
    }
    let mut target = response.path().to_string();
    if let Some(query) = response.query() {
        target.push('?');
        target.push_str(query);
    }
    Ok(target)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn permanent_error_models_render_but_outages_preserve_current_content() {
        for status in [200, 401, 403, 404, 422] {
            assert_eq!(
                response_kind(status, crate::webui::bootstrap::MODEL_CONTENT_TYPE),
                ResponseKind::Model
            );
        }
        for status in [401, 403, 404] {
            assert_eq!(
                response_kind(status, "text/html; charset=utf-8"),
                ResponseKind::Navigate
            );
        }
        for status in [408, 429, 503] {
            assert_eq!(
                response_kind(status, crate::webui::bootstrap::MODEL_CONTENT_TYPE),
                ResponseKind::Unavailable
            );
        }
    }

    #[test]
    fn typed_not_found_envelope_decodes_without_a_success_status() {
        let model = crate::webui::app::PageEnvelope {
            title: "Not found".into(),
            location: "/missing".into(),
            chrome: crate::webui::document::Chrome::default(),
            page: crate::webui::app::Page::Error {
                status: 404,
                message: "Not found".into(),
                repo: None,
                signed_in: false,
            },
        };
        let json = serde_json::to_string(&model).unwrap();
        assert_eq!(decode_page_model(&json).unwrap(), model);
        let mut unsafe_model = model;
        unsafe_model.location = "//another-origin.invalid".into();
        assert!(decode_page_model(&serde_json::to_string(&unsafe_model).unwrap()).is_err());
        assert_eq!(
            decode_page_model(&serde_json::to_string(&unsafe_model).unwrap()),
            Err(LoadError::UnsafeLocation)
        );
    }

    #[test]
    fn malformed_or_oversized_models_return_typed_failures() {
        assert_eq!(
            decode_page_model("{invalid private content"),
            Err(LoadError::InvalidModel)
        );
        assert_eq!(
            decode_page_model(&" ".repeat(16 * 1024 * 1024 + 1)),
            Err(LoadError::TooLarge)
        );
        assert_eq!(
            LoadError::InvalidTarget.to_string(),
            "This navigation target is not safe."
        );
        assert_eq!(
            LoadError::InvalidModel.to_string(),
            "The server returned an incompatible page model. Reload this page."
        );
        assert_eq!(LoadError::Unavailable(503).to_string(), "The server could not load this page (HTTP 503). Your current content has been preserved.");
        assert_eq!(
            LoadError::Timeout.to_string(),
            "The page request timed out. Your current content has been preserved."
        );
        assert_eq!(
            LoadError::Transport("The page response was interrupted.").to_string(),
            "The page response was interrupted."
        );
    }
}
