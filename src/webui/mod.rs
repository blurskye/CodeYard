//! Shared safe Dioxus views: useful server HTML and identical browser hydration.
//! No backend/DB authority crosses into this module.
pub mod app;
pub mod bootstrap;
pub mod components;
pub mod document;
pub mod hydration;
pub mod navigation;
pub mod pages;

/// Percent-encode one path segment or query value, never an entire URL.
pub fn encode_component(value: &str) -> String {
    let mut encoded = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
            encoded.push(byte as char);
        } else {
            use std::fmt::Write;
            write!(encoded, "%{byte:02X}").expect("writing to String");
        }
    }
    encoded
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// RSX event-handler/default props must be constructed in a framework scope.
    pub(crate) fn in_scope<T>(render: impl FnOnce() -> T) -> T {
        use dioxus::prelude::*;
        let dom = VirtualDom::new(|| rsx! {});
        dom.in_scope(ScopeId::ROOT, render)
    }
    #[test]
    fn encodes_segments_and_utf8_without_query_ambiguity() {
        assert_eq!(encode_component("a/b +é?"), "a%2Fb%20%2B%C3%A9%3F");
    }
}
