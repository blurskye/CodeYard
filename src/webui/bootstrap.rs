//! Safe typed bootstrap for the supported Dioxus hydration protocol.
use super::app::PageEnvelope;
use serde::{Deserialize, Serialize};

pub const ROOT_ID: &str = "dioxus-root";
pub const BOOTSTRAP_ID: &str = "codeyard-bootstrap";
pub const MODEL_CONTENT_TYPE: &str = "application/vnd.codeyard.page+json";
pub const MAX_PAGE_MODEL_BYTES: usize = 16 * 1024 * 1024;
pub const THEME_CONTENT_TYPE: &str = "application/vnd.codeyard.theme+json";
pub const DIOXUS_VERSION: &str = "0.7.10";

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Bootstrap {
    pub protocol: u8,
    pub renderer: String,
    pub initial: PageEnvelope,
    pub hydration: HydrationData,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct HydrationData {
    pub data: String,
    pub debug_types: String,
    pub debug_locations: String,
}

/// JSON is non-executable data. Escape HTML raw-text delimiters as well as JS
/// separators so a view field can never terminate the application/json script.
pub fn safe_json<T: Serialize>(value: &T) -> Result<String, serde_json::Error> {
    serde_json::to_string(value).map(|json| {
        json.replace('&', "\\u0026")
            .replace('<', "\\u003c")
            .replace('>', "\\u003e")
            .replace('\u{2028}', "\\u2028")
            .replace('\u{2029}', "\\u2029")
    })
}

#[cfg(not(target_arch = "wasm32"))]
pub fn prerender(initial: PageEnvelope) -> (String, Bootstrap) {
    use dioxus::prelude::*;
    let context = dioxus_fullstack_core::HydrationContext::default();
    context
        .error_entry()
        .insert(&None, std::panic::Location::caller());
    let mut dom = VirtualDom::new_with_props(
        super::app::App,
        super::app::AppProps {
            initial: initial.clone(),
        },
    );
    dom.insert_any_root_context(Box::new(context.clone()));
    dom.in_scope(ScopeId::ROOT, || {
        dioxus::dioxus_core::provide_create_error_boundary(
            dioxus_fullstack_core::init_error_boundary,
        )
    });
    dom.rebuild_in_place();
    let mut renderer = dioxus_ssr::Renderer::new();
    renderer.pre_render = true;
    let html = renderer.render(&dom);
    let serialized = context.serialized();
    #[cfg(debug_assertions)]
    let (debug_types, debug_locations) = {
        // Framework type metadata is useful for compatibility checks; absolute
        // builder/source locations are not browser presentation data.
        let locations = serde_json::from_str::<Vec<String>>(&serialized.debug_locations)
            .ok()
            .and_then(|locations| {
                serde_json::to_string(&vec!["server render"; locations.len()]).ok()
            })
            .unwrap_or_else(|| "[]".to_string());
        (serialized.debug_types, locations)
    };
    #[cfg(not(debug_assertions))]
    let (debug_types, debug_locations) = ("[]".into(), "[]".into());
    (
        html,
        Bootstrap {
            protocol: 1,
            renderer: DIOXUS_VERSION.into(),
            initial,
            hydration: HydrationData {
                data: serialized.data,
                debug_types,
                debug_locations,
            },
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn json_raw_text_escape_roundtrips_without_closing_the_script() {
        let source = "</script><script>alert('x')</script>&\u{2028}\u{2029}";
        let encoded = safe_json(&source).unwrap();
        assert!(!encoded.contains('<'));
        assert!(!encoded.contains('&'));
        assert!(!encoded.contains('\u{2028}'));
        assert_eq!(serde_json::from_str::<String>(&encoded).unwrap(), source);
    }
}
